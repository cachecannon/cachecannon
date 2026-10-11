use metriken_exposition::{
    Counter as SnapCounter, Gauge as SnapGauge, Histogram as SnapHistogram, Snapshot, SnapshotV2,
};
use metriken_storage::convert::{MsgpackToParquet, ParquetOptions};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::SharedState;

mod stream;

/// Admin server that exposes Prometheus metrics and optionally records Parquet.
pub struct AdminServer {
    listen_addr: Option<SocketAddr>,
    parquet_path: Option<PathBuf>,
    parquet_interval: Duration,
    shared: Arc<SharedState>,
    stop_notify: Arc<Notify>,
}

impl AdminServer {
    pub fn new(
        listen_addr: Option<SocketAddr>,
        parquet_path: Option<PathBuf>,
        parquet_interval: Duration,
        shared: Arc<SharedState>,
    ) -> Self {
        Self {
            listen_addr,
            parquet_path,
            parquet_interval,
            shared,
            stop_notify: Arc::new(Notify::new()),
        }
    }

    /// Run the admin server. This function spawns async tasks and returns immediately.
    pub fn run(self) -> AdminHandle {
        let stop_notify = Arc::clone(&self.stop_notify);

        let handle = std::thread::Builder::new()
            .name("admin".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create admin runtime");

                rt.block_on(async move {
                    let mut tasks = Vec::new();

                    // Spawn Prometheus server if configured
                    if let Some(addr) = self.listen_addr {
                        let stop_notify = Arc::clone(&self.stop_notify);
                        tasks.push(tokio::spawn(async move {
                            if let Err(e) = run_prometheus_server(addr, stop_notify).await {
                                tracing::error!("prometheus server error: {}", e);
                            }
                        }));
                    }

                    // Spawn Parquet recorder if configured
                    if let Some(path) = self.parquet_path {
                        let interval = self.parquet_interval;
                        let shared = Arc::clone(&self.shared);
                        let stop_notify = Arc::clone(&self.stop_notify);
                        tasks.push(tokio::spawn(async move {
                            if let Err(e) =
                                run_parquet_recorder(path, interval, shared, stop_notify).await
                            {
                                tracing::error!("parquet recorder error: {}", e);
                            }
                        }));
                    }

                    // Wait for all tasks
                    for task in tasks {
                        if let Err(e) = task.await {
                            tracing::error!("admin task panicked: {}", e);
                        }
                    }
                });
            })
            .expect("failed to spawn admin thread");

        AdminHandle {
            handle: Some(handle),
            stop_notify,
        }
    }
}

pub struct AdminHandle {
    handle: Option<std::thread::JoinHandle<()>>,
    stop_notify: Arc<Notify>,
}

impl AdminHandle {
    pub fn shutdown(&mut self) {
        self.stop_notify.notify_waiters();
        if let Some(handle) = self.handle.take()
            && let Err(e) = handle.join()
        {
            tracing::error!("admin thread panicked: {:?}", e);
        }
    }
}

impl Drop for AdminHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn run_prometheus_server(addr: SocketAddr, stop_notify: Arc<Notify>) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("prometheus server listening on {}", addr);
    serve_admin(listener, stop_notify).await
}

/// Accept connections on `listener` until `stop_notify` fires. One request per
/// connection.
async fn serve_admin(listener: TcpListener, stop_notify: Arc<Notify>) -> io::Result<()> {
    // Shared by every stream subscription on this server.
    let passes = Arc::new(stream::passes());

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((mut socket, _peer)) => {
                        let passes = Arc::clone(&passes);
                        tokio::spawn(async move {
                            let mut buf = [0u8; 1024];
                            let read = match socket.read(&mut buf).await {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::debug!("admin read error: {}", e);
                                    return;
                                }
                            };

                            let target = request_target(&buf[..read]);

                            // The stream is the one response that is not a
                            // whole body: it writes until the client goes away,
                            // which ends it with a write error.
                            if let Some(("/metrics/stream", query)) = target {
                                if let Err(e) = stream::serve(&mut socket, query, &passes).await {
                                    tracing::debug!("metrics stream ended: {}", e);
                                }
                                return;
                            }

                            // An unparseable request names no target, so there
                            // is nothing to serve it from.
                            let body = match target {
                                Some((path, _)) => respond(path),
                                None => AdminBody::NotFound,
                            };

                            if let Err(e) = socket.write_all(&http_response(body)).await {
                                tracing::debug!("admin write error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!("accept error: {}", e);
                    }
                }
            }
            _ = stop_notify.notified() => {
                break;
            }
        }
    }

    Ok(())
}

/// What the admin server serves for a request path.
enum AdminBody {
    Prometheus(String),
    Msgpack(Vec<u8>),
    NotFound,
    ServerError,
}

/// The request target from an HTTP request line (`GET /metrics HTTP/1.1`),
/// split into its path and its query string without the `?` (empty when there
/// is none). Only `/metrics/stream` reads the query. `None` when the bytes
/// are not a request line naming an absolute path.
fn request_target(request: &[u8]) -> Option<(&str, &str)> {
    let line = request.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let target = line.split_whitespace().nth(1)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    path.starts_with('/').then_some((path, query))
}

/// Serve one path.
///
/// The two metric paths are the ones `rezolus record` probes: it tries
/// `/metrics/binary` for a msgpack source first and falls back to `/metrics`
/// for a Prometheus one (`probe_endpoint`, rezolus src/recorder/mod.rs). Serving
/// msgpack is what lets a run be recorded into the same `.rez` archive as the
/// server- and client-side agents, as one more `--endpoint`.
fn respond(path: &str) -> AdminBody {
    match path {
        // `/` served the exposition text before paths were routed at all; a
        // scraper pointed at it must not start 404ing.
        "/" | "/metrics" => AdminBody::Prometheus(generate_prometheus_output()),
        "/metrics/binary" => match Snapshot::to_msgpack(&create_snapshot()) {
            Ok(bytes) => AdminBody::Msgpack(bytes),
            Err(e) => {
                tracing::warn!("failed to serialize snapshot as msgpack: {}", e);
                AdminBody::ServerError
            }
        },
        _ => AdminBody::NotFound,
    }
}

/// Frame a body as an HTTP/1.1 response. Bytes, not a string: the msgpack body
/// is not UTF-8.
fn http_response(body: AdminBody) -> Vec<u8> {
    let (status, content_type, body) = match body {
        AdminBody::Prometheus(text) => ("200 OK", "text/plain; version=0.0.4", text.into_bytes()),
        AdminBody::Msgpack(bytes) => ("200 OK", "application/msgpack", bytes),
        AdminBody::NotFound => ("404 Not Found", "text/plain", Vec::new()),
        AdminBody::ServerError => ("500 Internal Server Error", "text/plain", Vec::new()),
    };

    let mut response = format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        status,
        content_type,
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn generate_prometheus_output() -> String {
    use std::fmt::Write as _;

    let mut output = String::new();

    for metric in metriken::metrics().iter() {
        // Every cachecannon metric name is already a legal Prometheus name, so
        // this is a no-op for all of them (asserted in `every_local_metric_name_
        // is_already_prometheus_legal`). ringline's are not -- `ringline/pool`
        // -- and they reach this endpoint via the `CounterGroup` arm below.
        let name = &sanitize_name(metric.name());
        let value = match metric.value() {
            Some(v) => v,
            None => continue,
        };
        let description = metric.description();

        // Handle different metric types
        match value {
            metriken::Value::Counter(v) => {
                write_help(&mut output, name, description);
                let _ = writeln!(output, "# TYPE {} counter", name);
                let _ = writeln!(output, "{} {}", name, v);
            }
            metriken::Value::Gauge(v) => {
                write_help(&mut output, name, description);
                let _ = writeln!(output, "# TYPE {} gauge", name);
                let _ = writeln!(output, "{} {}", name, v);
            }
            // Counter groups are one registry entry holding N labelled slots.
            // ringline exposes its whole runtime this way -- `ringline/pool`
            // carries RECV_PARKED and BUFFER_RING_EMPTY, `ringline/bytes`
            // carries FALLBACK_RECEIVED -- so without this arm the generator's
            // own saturation signals were counted at runtime and then dropped
            // here, which is exactly how a buffer-starved run reads as server
            // latency. Same silent-drop the `Histogram` arm above was added to
            // fix; groups were left behind.
            //
            // Slot labels come from ringline's `init_metadata()`, which its
            // worker startup calls, so only slots the runtime declared are
            // emitted -- an unlabelled group renders nothing rather than a
            // column of bare indices.
            metriken::Value::CounterGroup(group) => {
                let mut rows: Vec<(String, u64)> = group
                    .metadata_snapshot()
                    .into_iter()
                    .filter_map(|(idx, slot)| {
                        group.counter_value(idx).map(|v| (format_labels(&slot), v))
                    })
                    .collect();
                if !rows.is_empty() {
                    // `metadata_snapshot()` order is unspecified; sort so a
                    // scrape diff reflects value changes, not map iteration.
                    rows.sort();
                    write_help(&mut output, name, description);
                    let _ = writeln!(output, "# TYPE {} counter", name);
                    for (labels, v) in rows {
                        let _ = writeln!(output, "{}{} {}", name, labels, v);
                    }
                }
            }
            // metriken-core 0.2 carries histograms in a dedicated `Histogram`
            // variant, so this -- not the `Other` arm below -- is what every
            // AtomicHistogram reaches. `create_snapshot` was fixed for the same
            // reason (#117); this endpoint was left behind, and until now
            // served no latency data at all.
            metriken::Value::Histogram(h) => {
                if let Some(snapshot) = h.load() {
                    write_histogram_summary(&mut output, name, description, &snapshot);
                }
            }
            metriken::Value::Other(any) => {
                // Retained for histograms that still surface as `Other`.
                if let Some(histogram) = any.downcast_ref::<metriken::AtomicHistogram>()
                    && let Some(snapshot) = histogram.load()
                {
                    write_histogram_summary(&mut output, name, description, &snapshot);
                }
            }
            // Handle any future Value variants
            _ => {}
        }
    }

    output
}

/// Render one histogram as a Prometheus "summary" -- `quantile="..."` rows plus
/// `_count`/`_sum` -- rather than a "histogram" (which would require cumulative
/// `_bucket{le=}` rows).
fn write_histogram_summary(
    out: &mut String,
    name: &str,
    description: Option<&str>,
    snapshot: &histogram::Histogram,
) {
    use std::fmt::Write as _;

    write_help(out, name, description);
    let _ = writeln!(out, "# TYPE {} summary", name);

    let quantiles = [0.50, 0.90, 0.95, 0.99, 0.999, 0.9999];
    if let Ok(Some(results)) = snapshot.quantiles(&quantiles) {
        for (quantile, bucket) in results.entries() {
            let _ = writeln!(
                out,
                "{}{{quantile=\"{}\"}} {}",
                name,
                quantile.as_f64(),
                bucket.end()
            );
        }
    }

    let mut count = 0u64;
    let mut sum = 0u128;
    for bucket in snapshot {
        let bucket_count = bucket.count();
        count += bucket_count;
        // Midpoint of the bucket: the exact values are not retained.
        let midpoint = (bucket.start() as u128 + bucket.end() as u128) / 2;
        sum += bucket_count as u128 * midpoint;
    }
    let _ = writeln!(out, "{}_count {}", name, count);
    let _ = writeln!(out, "{}_sum {}", name, sum);
}

/// Emit a `# HELP` line if `description` is non-empty. The text is escaped
/// per the Prometheus exposition format: backslashes and newlines only.
/// Sanitize a metric name for Prometheus, which requires
/// `[a-zA-Z_:][a-zA-Z0-9_:]*`.
///
/// Matches `metriken_exposition::prometheus`'s sanitizer so the two renderings
/// of the same metric agree on a name.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render one counter-group slot's metadata as a Prometheus label set,
/// `{op="recv_parked"}`. Returns an empty string for a slot with no labels, so
/// the caller can concatenate unconditionally.
///
/// Keys are sorted: `metadata_snapshot()` hands back a `HashMap` per slot, and
/// unsorted labels would reorder between scrapes for no reason.
fn format_labels(metadata: &HashMap<String, String>) -> String {
    if metadata.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<String> = metadata
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
        .collect();
    pairs.sort();
    format!("{{{}}}", pairs.join(","))
}

/// Escape a Prometheus label value: backslash, double quote and newline.
fn escape_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn write_help(out: &mut String, name: &str, description: Option<&str>) {
    use std::fmt::Write as _;

    let Some(desc) = description else { return };
    if desc.is_empty() {
        return;
    }
    let _ = write!(out, "# HELP {} ", name);
    for ch in desc.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('\n');
}

/// Create a snapshot with the `metric` key added to each metric's metadata.
/// This matches the format expected by metriken-query.
fn create_snapshot() -> Snapshot {
    let start = Instant::now();
    let timestamp = SystemTime::now();

    let mut counters = Vec::new();
    let mut gauges = Vec::new();
    let mut histograms = Vec::new();

    for metric in metriken::metrics().iter() {
        let value = metric.value();
        if value.is_none() {
            continue;
        }

        let name = metric.name();

        // Build metadata with `metric` key first (like rezolus does)
        let mut metadata: HashMap<String, String> =
            [("metric".to_string(), name.to_string())].into();

        // Add any existing metadata from the metric (excluding description)
        for (k, v) in metric.metadata().iter() {
            metadata.insert(k.to_string(), v.to_string());
        }

        match value {
            Some(metriken::Value::Counter(v)) => {
                counters.push(SnapCounter::new(name.to_string(), v, metadata));
            }
            Some(metriken::Value::Gauge(v)) => {
                gauges.push(SnapGauge::new(name.to_string(), v, metadata));
            }
            // metriken-core 0.2 carries histograms in a dedicated `Histogram`
            // variant. Without this arm every latency histogram fell through to
            // the catch-all below and was silently dropped, so neither the
            // parquet recording nor the Prometheus endpoint carried any latency
            // data -- it existed only in the terminal summary.
            Some(metriken::Value::Histogram(h)) => {
                if let Some(hist) = h.load() {
                    metadata.insert(
                        "grouping_power".to_string(),
                        h.config().grouping_power().to_string(),
                    );
                    metadata.insert(
                        "max_value_power".to_string(),
                        h.config().max_value_power().to_string(),
                    );
                    histograms.push(SnapHistogram::new(name.to_string(), hist, metadata));
                }
            }
            // The recording half of the counter-group fix; see the matching arm
            // in `generate_prometheus_output`. Flattened one series per slot,
            // because `SnapshotV2` has no group representation -- counters,
            // gauges and histograms are the only three things it can carry.
            //
            // Naming follows metriken-exposition's own snapshotter: the slot
            // index is appended as `x{idx}`, the base name stays in the `metric`
            // metadata key, and the slot's labels (`op = recv_parked`) are
            // merged in. Getting this right now means a recording written today
            // still lines up when cachecannon moves off 0.16.
            Some(metriken::Value::CounterGroup(group)) => {
                let mut slots = group.metadata_snapshot();
                // Unspecified iteration order upstream; keep parquet column
                // order stable across snapshots within a run.
                slots.sort_by_key(|(idx, _)| *idx);
                for (idx, slot) in slots {
                    let Some(value) = group.counter_value(idx) else {
                        continue;
                    };
                    let mut slot_metadata = metadata.clone();
                    slot_metadata.extend(slot);
                    counters.push(SnapCounter::new(
                        format!("{name}x{idx}"),
                        value,
                        slot_metadata,
                    ));
                }
            }
            // Retained for metrics that still surface as `Other`.
            Some(metriken::Value::Other(other)) => {
                let histogram = if let Some(h) = other.downcast_ref::<metriken::AtomicHistogram>() {
                    h.load()
                } else if let Some(h) = other.downcast_ref::<metriken::RwLockHistogram>() {
                    h.load()
                } else {
                    None
                };

                if let Some(h) = histogram {
                    // Add histogram config to metadata
                    metadata.insert(
                        "grouping_power".to_string(),
                        h.config().grouping_power().to_string(),
                    );
                    metadata.insert(
                        "max_value_power".to_string(),
                        h.config().max_value_power().to_string(),
                    );

                    histograms.push(SnapHistogram::new(name.to_string(), h, metadata));
                }
            }
            _ => {}
        }
    }

    let duration = start.elapsed();

    Snapshot::V2(SnapshotV2 {
        systemtime: timestamp,
        duration,
        metadata: [
            ("source".to_string(), "cachecannon".to_string()),
            ("version".to_string(), env!("CARGO_PKG_VERSION").to_string()),
        ]
        .into(),
        counters,
        gauges,
        histograms,
    })
}

async fn run_parquet_recorder(
    path: PathBuf,
    interval: Duration,
    shared: Arc<SharedState>,
    stop_notify: Arc<Notify>,
) -> io::Result<()> {
    // Stream snapshots to a temp msgpack file as they arrive, then convert
    // to parquet at the end. This keeps peak memory at one snapshot rather
    // than buffering the entire run's worth.
    let temp_path = msgpack_temp_path(&path);
    let temp_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .read(true)
        .open(&temp_path)?;
    let mut writer = BufWriter::new(temp_file);
    let mut snapshot_count: usize = 0;

    tracing::info!(
        "parquet recorder started, staging at {:?}, will write to {:?}",
        temp_path,
        path
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                // Only collect snapshots during the running phase (skip warmup)
                if shared.phase().is_recording() {
                    match append_snapshot(&mut writer, &create_snapshot()) {
                        Ok(()) => snapshot_count += 1,
                        Err(e) => tracing::warn!("failed to stage parquet snapshot: {}", e),
                    }
                }
            }
            _ = stop_notify.notified() => {
                break;
            }
        }
    }

    // Final snapshot
    match append_snapshot(&mut writer, &create_snapshot()) {
        Ok(()) => snapshot_count += 1,
        Err(e) => tracing::warn!("failed to stage final parquet snapshot: {}", e),
    }

    // Flush staged snapshots to disk before converting.
    let temp_file = writer.into_inner().map_err(|e| e.into_error())?;
    temp_file.sync_all()?;
    drop(temp_file);

    if snapshot_count == 0 {
        let _ = std::fs::remove_file(&temp_path);
        tracing::info!("parquet recorder stopped, no snapshots to write");
        return Ok(());
    }

    // Convert msgpack stream → parquet (two passes over the temp file).
    let converter = MsgpackToParquet::with_options(ParquetOptions::new())
        .metadata(
            "sampling_interval_ms".to_string(),
            interval.as_millis().to_string(),
        )
        .metadata("source".to_string(), "cachecannon".to_string())
        .metadata("version".to_string(), env!("CARGO_PKG_VERSION").to_string());

    match converter.convert_file_path(&temp_path, &path) {
        Ok(rows) => {
            tracing::info!("parquet recorder stopped, wrote {:?} ({} rows)", path, rows);
        }
        Err(e) => {
            tracing::warn!("failed to convert msgpack stream to parquet: {}", e);
        }
    }

    let _ = std::fs::remove_file(&temp_path);

    Ok(())
}

/// Path used for the on-disk msgpack staging file alongside the final
/// parquet output. Sits next to the destination so cleanup is obvious and
/// the user's chosen output filesystem is reused (no /tmp surprises).
fn msgpack_temp_path(parquet_path: &Path) -> PathBuf {
    let mut buf = parquet_path.as_os_str().to_owned();
    buf.push(".msgpack.tmp");
    PathBuf::from(buf)
}

/// Serialize a snapshot to msgpack and append it to the staging file.
fn append_snapshot<W: Write>(writer: &mut W, snapshot: &Snapshot) -> io::Result<()> {
    let bytes = Snapshot::to_msgpack(snapshot).map_err(|e| io::Error::other(e.to_string()))?;
    writer.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_output_carries_latency_histograms() {
        let _ = crate::metrics::RESPONSE_LATENCY.increment(12_345);

        let output = generate_prometheus_output();

        assert!(
            output.contains("# TYPE response_latency summary"),
            "response_latency missing from the exposition output:\n{output}"
        );
        assert!(output.contains("response_latency{quantile="));
        assert!(output.contains("response_latency_count "));
    }

    #[test]
    fn request_target_reads_the_path_from_the_request_line() {
        assert_eq!(
            request_target(b"GET /metrics/binary HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("/metrics/binary", ""))
        );
        assert_eq!(
            request_target(b"GET /metrics?collect=all HTTP/1.1\r\n\r\n"),
            Some(("/metrics", "collect=all"))
        );
        assert_eq!(
            request_target(b"GET /metrics/stream?interval=100ms&layout=long HTTP/1.1\r\n\r\n"),
            Some(("/metrics/stream", "interval=100ms&layout=long"))
        );
        assert_eq!(request_target(b"garbage"), None);
    }

    #[test]
    fn metrics_paths_route_to_their_formats() {
        assert!(matches!(respond("/metrics"), AdminBody::Prometheus(_)));
        // The root kept serving the exposition text before paths were routed;
        // a scraper pointed at it must not start 404ing.
        assert!(matches!(respond("/"), AdminBody::Prometheus(_)));
        assert!(matches!(respond("/metrics/binary"), AdminBody::Msgpack(_)));
        assert!(matches!(respond("/nope"), AdminBody::NotFound));
    }

    #[test]
    fn msgpack_body_decodes_as_a_snapshot_carrying_histograms() {
        let _ = crate::metrics::RESPONSE_LATENCY.increment(12_345);

        let AdminBody::Msgpack(bytes) = respond("/metrics/binary") else {
            panic!("/metrics/binary did not serve msgpack");
        };

        // `Snapshot` is an untagged enum and rmp-serde encodes structs as
        // arrays, so this also pins the variant: a V2 body must not decode as
        // a V1 one.
        let mut snapshot: Snapshot =
            rmp_serde::from_slice(&bytes).expect("msgpack body is not a snapshot");
        assert!(matches!(snapshot, Snapshot::V2(_)));

        assert!(
            snapshot
                .histograms()
                .iter()
                .any(|h| h.name == "response_latency"),
            "decoded snapshot carries no response_latency histogram"
        );
        assert_eq!(
            snapshot.metadata().get("source").map(String::as_str),
            Some("cachecannon")
        );
    }

    #[test]
    fn msgpack_response_is_framed_as_binary() {
        let (target, _) =
            request_target(b"GET /metrics/binary HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let response = http_response(respond(target));

        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("no header/body separator");
        let headers = std::str::from_utf8(&response[..split]).unwrap();
        let body = &response[split + 4..];

        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
        assert!(
            headers.contains("Content-Type: application/msgpack\r\n"),
            "{headers}"
        );
        assert!(
            headers.contains(&format!("Content-Length: {}\r\n", body.len())),
            "content-length disagrees with the {} byte body:\n{headers}",
            body.len()
        );
        rmp_serde::from_slice::<Snapshot>(body).expect("framed body is not a snapshot");
    }

    // Two fixtures rather than one: the test binary runs these in parallel
    // against the one global registry, and a shared group would make each
    // test's expected value depend on whether the other had run yet.
    #[metriken::metric(
        name = "test_prometheus_group",
        description = "Counter-group fixture for the exposition endpoint"
    )]
    static TEST_PROMETHEUS_GROUP: metriken::ShardedCounterGroup =
        metriken::ShardedCounterGroup::new(2);

    #[metriken::metric(
        name = "test_snapshot_group",
        description = "Counter-group fixture for the msgpack snapshot"
    )]
    static TEST_SNAPSHOT_GROUP: metriken::ShardedCounterGroup =
        metriken::ShardedCounterGroup::new(2);

    #[test]
    fn counter_groups_reach_the_prometheus_endpoint() {
        TEST_PROMETHEUS_GROUP.insert_metadata(0, "op".into(), "recv_parked".into());
        assert!(TEST_PROMETHEUS_GROUP.add(0, 7));

        let output = generate_prometheus_output();

        assert!(
            output.contains("# TYPE test_prometheus_group counter"),
            "counter group missing from the exposition output:\n{output}"
        );
        assert!(
            output.contains("test_prometheus_group{op=\"recv_parked\"} 7"),
            "slot not rendered as a labelled series:\n{output}"
        );
        // Slot 1 was never written, so it has no metadata and no value. An
        // unwritten slot must not appear as a zero -- that would read as
        // "measured, and it was fine".
        assert!(
            !output.contains("test_prometheus_group 0"),
            "unwritten slot rendered as a bare zero:\n{output}"
        );
    }

    #[test]
    fn counter_groups_reach_the_msgpack_snapshot() {
        TEST_SNAPSHOT_GROUP.insert_metadata(1, "op".into(), "fallback_received".into());
        assert!(TEST_SNAPSHOT_GROUP.add(1, 9));

        let AdminBody::Msgpack(bytes) = respond("/metrics/binary") else {
            panic!("/metrics/binary did not serve msgpack");
        };
        let mut snapshot: Snapshot =
            rmp_serde::from_slice(&bytes).expect("msgpack body is not a snapshot");

        // Flattened as `{name}x{idx}`, matching metriken-exposition's own
        // snapshotter so the column survives the eventual version bump.
        let slot = snapshot
            .counters()
            .iter()
            .find(|c| c.name == "test_snapshot_groupx1")
            .expect("counter group slot missing from the decoded snapshot")
            .clone();

        assert_eq!(slot.value, 9);
        assert_eq!(
            slot.metadata.get("op").map(String::as_str),
            Some("fallback_received")
        );
        // The base name stays queryable via the `metric` key; the slot is
        // distinguished by its label, not by the column name.
        assert_eq!(
            slot.metadata.get("metric").map(String::as_str),
            Some("test_snapshot_group")
        );
    }

    /// The fixtures above prove this module's own logic. This one proves the
    /// integration: that ringline's real group, with the real slot labels its
    /// `init_metadata()` registers, survives the name sanitizer and comes out
    /// the far end queryable. A ringline upgrade that renamed `op` or the
    /// `ringline/pool` namespace would fail here rather than silently going
    /// missing from a dashboard again.
    ///
    /// `RECV_PARKED` is the specific counter that distinguishes a
    /// buffer-starved generator from a slow server, which is why it is the one
    /// pinned here.
    #[test]
    fn ringlines_own_pool_group_is_exposed_with_its_labels() {
        // Backend-agnostic: ringline registers slot labels from `launch()`, but
        // a unit test never launches a worker, so do it directly. Idempotent.
        ringline::metrics::init_metadata();
        ringline::metrics::POOL.add(ringline::metrics::pool::RECV_PARKED, 3);

        let output = generate_prometheus_output();

        assert!(
            output.contains("# TYPE ringline_pool counter"),
            "ringline/pool missing from the exposition output:\n{output}"
        );
        assert!(
            output.contains("ringline_pool{op=\"recv_parked\"}"),
            "recv_parked not rendered as a labelled series:\n{output}"
        );
    }

    #[test]
    fn every_local_metric_name_is_already_prometheus_legal() {
        for metric in metriken::metrics().iter() {
            let name = metric.name();
            // ringline namespaces its metrics with `/`, which is what the
            // sanitizer is for. Everything else is ours.
            if name.starts_with("ringline/") {
                continue;
            }
            assert_eq!(
                sanitize_name(name),
                name,
                "`{name}` is renamed by sanitization, which moves its series"
            );
        }
    }

    /// The invariant the sanitizer exists to hold, checked against the rendered
    /// output rather than against the name list -- the regression it catches was
    /// a foreign metric (`ringline/connections/active`, registered merely by
    /// linking ringline) reaching the endpoint through an arm that never
    /// considered names it did not own.
    #[test]
    fn exposition_output_has_no_illegal_metric_names() {
        ringline::metrics::init_metadata();

        let output = generate_prometheus_output();

        for line in output.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // `name{labels} value` or `name value`.
            let name = line.split(['{', ' ']).next().unwrap_or(line);
            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                "`{name}` is not a legal Prometheus metric name, which makes the \
                 whole exposition body unparseable:\n{line}"
            );
        }
    }

    #[test]
    fn sanitize_name_replaces_illegal_characters() {
        assert_eq!(sanitize_name("ringline/pool"), "ringline_pool");
        assert_eq!(sanitize_name("with.dots"), "with_dots");
        assert_eq!(sanitize_name("response_latency"), "response_latency");
        assert_eq!(sanitize_name("has:colon"), "has:colon");
    }

    #[test]
    fn labels_are_sorted_and_escaped() {
        let metadata = HashMap::from([
            ("op".to_string(), "recv_parked".to_string()),
            ("id".to_string(), "a\"b".to_string()),
        ]);
        assert_eq!(
            format_labels(&metadata),
            "{id=\"a\\\"b\",op=\"recv_parked\"}"
        );
        assert_eq!(format_labels(&HashMap::new()), "");
    }

    /// `--parquet`'s path end to end with the crates it now uses: snapshots
    /// staged as msgpack, converted by metriken-storage, and read back by the
    /// metriken-query reader `cachecannon view` opens.
    #[test]
    fn parquet_recording_reads_back_through_the_viewer_reader() {
        use metriken_query::{MetricsSource, QueryResult};

        let dir = std::env::temp_dir().join(format!(
            "cachecannon-parquet-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("run.msgpack");
        let parquet = dir.join("run.parquet");

        let mut writer = BufWriter::new(std::fs::File::create(&staged).unwrap());
        for _ in 0..3 {
            // A quantile reads the change between rows, so each row adds one.
            let _ = crate::metrics::RESPONSE_LATENCY.increment(12_345);
            append_snapshot(&mut writer, &create_snapshot()).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(writer);
        let rows = MsgpackToParquet::with_options(ParquetOptions::new())
            .metadata("sampling_interval_ms".to_string(), "5".to_string())
            .metadata("source".to_string(), "cachecannon".to_string())
            .convert_file_path(&staged, &parquet)
            .unwrap();
        assert_eq!(rows, 3);

        let reader = metriken_query::ParquetReader::open(&parquet).unwrap();
        assert_eq!(reader.source(), "cachecannon");
        assert!(reader.has_histogram("response_latency"));
        let (start, end) = reader.time_range().unwrap();
        let result = reader
            .query_range(
                "histogram_quantile(0.5, response_latency)",
                start,
                end,
                0.001,
            )
            .unwrap();
        let QueryResult::Matrix { result } = result else {
            panic!("not a matrix: {result:?}");
        };
        assert!(
            result.iter().any(|s| !s.values.is_empty()),
            "no response_latency quantile read back"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An admin server on an ephemeral port, on its own current-thread runtime
    /// as `AdminServer::run` builds one. Dropping the guard stops it.
    struct TestServer {
        addr: SocketAddr,
        stop: Arc<Notify>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = Arc::new(Notify::new());
            let stop_server = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let listener = TcpListener::from_std(listener).unwrap();
                    serve_admin(listener, stop_server).await.unwrap();
                });
            });
            Self {
                addr,
                stop,
                thread: Some(thread),
            }
        }

        /// Send `GET {target}` and return the connection after the response
        /// head, with the status line and the headers (names lowercased).
        fn get(&self, target: &str) -> (std::net::TcpStream, String, HashMap<String, String>) {
            use std::io::Read;

            let mut conn = std::net::TcpStream::connect(self.addr).unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            // One write: the server reads the request with a single `read`, and
            // `write!` on a TcpStream sends each format piece as its own
            // write, so the server could read `GET ` alone and answer 404.
            let request = format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n");
            conn.write_all(request.as_bytes()).unwrap();

            // Byte at a time, so nothing after the head is consumed.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                conn.read_exact(&mut byte)
                    .expect("connection closed inside the head");
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let mut lines = head.split("\r\n");
            let status = lines.next().unwrap().to_string();
            let headers = lines
                .filter_map(|l| l.split_once(": "))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect();
            (conn, status, headers)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            // Notify until the accept loop has seen it: `notify_waiters` wakes
            // only a task already waiting.
            if let Some(thread) = self.thread.take() {
                while !thread.is_finished() {
                    self.stop.notify_waiters();
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = thread.join();
            }
        }
    }

    /// The route end to end over a socket: headers, then dendro's preamble,
    /// the handshake naming cachecannon, and rows frames carrying the
    /// registry's group with the latency histogram in it.
    #[test]
    fn metrics_stream_serves_handshake_and_rows() {
        use dendro::replicate::Frame;
        use dendro::replicate::wire::FrameReader;

        let _ = crate::metrics::RESPONSE_LATENCY.increment(12_345);
        let server = TestServer::start();

        let (conn, status, headers) = server.get("/metrics/stream?interval=100ms");
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("application/vnd.metriken.replication.v1+dendro")
        );
        assert_eq!(
            headers.get("x-metriken-frame-interval").map(String::as_str),
            Some("100ms")
        );
        assert_eq!(
            headers.get("x-metriken-update-floor").map(String::as_str),
            Some("10ms")
        );
        assert_eq!(
            headers.get("x-metriken-layout").map(String::as_str),
            Some("wide")
        );
        // The body runs until the connection closes.
        assert!(!headers.contains_key("content-length"), "{headers:?}");

        let mut reader = FrameReader::new(conn).expect("no dendro preamble");
        match reader.next_frame().unwrap() {
            Some(Frame::Handshake {
                labels, metadata, ..
            }) => {
                assert_eq!(
                    labels.get("source").map(String::as_str),
                    Some("cachecannon")
                );
                assert_eq!(
                    metadata.get("version").map(String::as_str),
                    Some(env!("CARGO_PKG_VERSION"))
                );
            }
            other => panic!("first frame is not a handshake: {other:?}"),
        }

        // The first rows frame carries every group with its schema; later
        // ones every windowless group again (DefaultRouter's are all
        // windowless). Three frames, so the stream is shown to keep going.
        let group = format!("{}/main", stream::NAMESPACE);
        let mut seqs = Vec::new();
        for i in 0..3 {
            match reader.next_frame().unwrap() {
                Some(Frame::Rows { seq, rows, .. }) => {
                    seqs.push(seq);
                    let row = rows
                        .iter()
                        .find(|r| r.stream == group)
                        .unwrap_or_else(|| panic!("frame {i} has no {group} row"));
                    if i == 0 {
                        // The schema travels in the first row of a stream,
                        // with each member's `metric` label.
                        assert!(
                            row.row
                                .windows(b"response_latency".len())
                                .any(|w| w == b"response_latency"),
                            "first {group} row does not name response_latency"
                        );
                    }
                }
                other => panic!("frame {i} is not a rows frame: {other:?}"),
            }
        }
        assert!(
            seqs.windows(2).all(|w| w[1] > w[0]),
            "seq does not advance: {seqs:?}"
        );
    }

    #[test]
    fn metrics_stream_refuses_a_zero_interval() {
        use std::io::Read;

        let server = TestServer::start();
        let (mut conn, status, _) = server.get("/metrics/stream?interval=0s");
        assert!(status.starts_with("HTTP/1.1 400 "), "{status}");
        let mut body = String::new();
        conn.read_to_string(&mut body).unwrap();
        assert!(!body.is_empty(), "a refusal carries its reason");
    }

    /// An interval equal to the floor is accepted.
    #[test]
    fn metrics_stream_accepts_an_interval_at_the_floor() {
        use dendro::replicate::Frame;
        use dendro::replicate::wire::FrameReader;

        let server = TestServer::start();
        let (conn, status, _) = server.get("/metrics/stream?interval=10ms");
        assert_eq!(status, "HTTP/1.1 200 OK");
        let mut reader = FrameReader::new(conn).expect("no dendro preamble");
        assert!(matches!(
            reader.next_frame().unwrap(),
            Some(Frame::Handshake { .. })
        ));
    }

    #[test]
    fn metrics_stream_refuses_an_interval_below_the_floor() {
        use std::io::Read;

        let server = TestServer::start();
        let (mut conn, status, _) = server.get("/metrics/stream?interval=1ms");
        assert!(status.starts_with("HTTP/1.1 400 "), "{status}");
        let mut body = String::new();
        conn.read_to_string(&mut body).unwrap();
        assert!(body.contains("update floor"), "{body}");
    }

    /// The whole-body routes are unchanged by the stream route beside them.
    #[test]
    fn metrics_binary_still_serves_over_a_socket() {
        use std::io::Read;

        let server = TestServer::start();
        let (mut conn, status, headers) = server.get("/metrics/binary");
        assert_eq!(status, "HTTP/1.1 200 OK");
        let mut body = Vec::new();
        conn.read_to_end(&mut body).unwrap();
        assert_eq!(headers.get("content-length"), Some(&body.len().to_string()));
        rmp_serde::from_slice::<Snapshot>(&body).expect("body is not a snapshot");
    }

    /// Writes what `/metrics/binary` serves to `$CACHECANNON_MSGPACK_FIXTURE`,
    /// for decoding by a consumer built against metriken-exposition 0.19 --
    /// the version `rezolus record` links, which cannot be a dependency here.
    ///
    ///     CACHECANNON_MSGPACK_FIXTURE=/tmp/snap.msgpack \
    ///       cargo test --lib dump_msgpack_fixture -- --ignored
    #[test]
    #[ignore = "writes a fixture file for out-of-tree decoding"]
    fn dump_msgpack_fixture() {
        let _ = crate::metrics::RESPONSE_LATENCY.increment(12_345);
        let path = std::env::var("CACHECANNON_MSGPACK_FIXTURE")
            .expect("set CACHECANNON_MSGPACK_FIXTURE to the output path");

        let AdminBody::Msgpack(bytes) = respond("/metrics/binary") else {
            panic!("/metrics/binary did not serve msgpack");
        };
        std::fs::write(&path, &bytes).expect("failed to write the fixture");
        eprintln!("wrote {} bytes to {path}", bytes.len());
    }
}
