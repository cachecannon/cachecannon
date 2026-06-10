use metriken_exposition::{
    Counter as SnapCounter, Gauge as SnapGauge, Histogram as SnapHistogram, MsgpackToParquet,
    ParquetOptions, Snapshot, SnapshotV2,
};
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

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((mut socket, _peer)) => {
                        tokio::spawn(async move {
                            let mut buf = [0u8; 1024];
                            // Read the request (we don't parse it, just need to consume it)
                            if let Err(e) = socket.read(&mut buf).await {
                                tracing::debug!("prometheus read error: {}", e);
                                return;
                            }

                            // Generate Prometheus output
                            let body = generate_prometheus_output();

                            let response = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: text/plain; version=0.0.4\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(),
                                body
                            );

                            if let Err(e) = socket.write_all(response.as_bytes()).await {
                                tracing::debug!("prometheus write error: {}", e);
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

fn generate_prometheus_output() -> String {
    use std::fmt::Write as _;

    let mut output = String::new();

    for metric in metriken::metrics().iter() {
        let name = metric.name();
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
            metriken::Value::Other(any) => {
                // Try to downcast to AtomicHistogram. We emit a "summary"
                // (with `quantile="..."` rows) rather than a Prometheus
                // "histogram" (which would require cumulative `_bucket{le=}`
                // rows). The previous code declared `histogram` while
                // emitting `quantile`, which is invalid exposition.
                if let Some(histogram) = any.downcast_ref::<metriken::AtomicHistogram>()
                    && let Some(snapshot) = histogram.load()
                {
                    write_help(&mut output, name, description);
                    let _ = writeln!(output, "# TYPE {} summary", name);

                    let quantiles = [0.50, 0.90, 0.95, 0.99, 0.999, 0.9999];
                    if let Ok(Some(results)) = snapshot.quantiles(&quantiles) {
                        for (quantile, bucket) in results.entries() {
                            let _ = writeln!(
                                output,
                                "{}{{quantile=\"{}\"}} {}",
                                name,
                                quantile.as_f64(),
                                bucket.end()
                            );
                        }
                    }

                    // Output count and sum
                    let mut count = 0u64;
                    let mut sum = 0u128;
                    for bucket in snapshot.into_iter() {
                        let bucket_count = bucket.count();
                        count += bucket_count;
                        // Use midpoint of bucket for sum approximation
                        let midpoint = (bucket.start() as u128 + bucket.end() as u128) / 2;
                        sum += bucket_count as u128 * midpoint;
                    }
                    let _ = writeln!(output, "{}_count {}", name, count);
                    let _ = writeln!(output, "{}_sum {}", name, sum);
                }
            }
            // Handle any future Value variants
            _ => {}
        }
    }

    output
}

/// Emit a `# HELP` line if `description` is non-empty. The text is escaped
/// per the Prometheus exposition format: backslashes and newlines only.
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
                counters.push(SnapCounter {
                    name: name.to_string(),
                    value: v,
                    metadata,
                });
            }
            Some(metriken::Value::Gauge(v)) => {
                gauges.push(SnapGauge {
                    name: name.to_string(),
                    value: v,
                    metadata,
                });
            }
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

                    histograms.push(SnapHistogram {
                        name: name.to_string(),
                        value: h,
                        metadata,
                    });
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
