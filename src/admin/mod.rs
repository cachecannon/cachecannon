use metriken_exposition::{
    Counter as SnapCounter, Gauge as SnapGauge, Histogram as SnapHistogram, ParquetOptions,
    ParquetSchema, Snapshot, SnapshotV2,
};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
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
                        let shared = Arc::clone(&self.shared);
                        let stop_notify = Arc::clone(&self.stop_notify);
                        tasks.push(tokio::spawn(async move {
                            if let Err(e) = run_prometheus_server(addr, shared, stop_notify).await {
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
                        let _ = task.await;
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
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AdminHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn run_prometheus_server(
    addr: SocketAddr,
    shared: Arc<SharedState>,
    stop_notify: Arc<Notify>,
) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("prometheus server listening on {}", addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((mut socket, _peer)) => {
                        tokio::spawn(async move {
                            let mut buf = [0u8; 1024];
                            // Read the request (we don't really care about it)
                            let _ = socket.read(&mut buf).await;

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

                            let _ = socket.write_all(response.as_bytes()).await;
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
            _ = async {
                while !shared.phase().should_stop() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            } => {
                break;
            }
        }
    }

    Ok(())
}

fn generate_prometheus_output() -> String {
    let mut output = String::new();

    for metric in metriken::metrics().iter() {
        let name = metric.name();
        let value = match metric.value() {
            Some(v) => v,
            None => continue,
        };

        // Handle different metric types
        match value {
            metriken::Value::Counter(v) => {
                output.push_str(&format!("# TYPE {} counter\n", name));
                output.push_str(&format!("{} {}\n", name, v));
            }
            metriken::Value::Gauge(v) => {
                output.push_str(&format!("# TYPE {} gauge\n", name));
                output.push_str(&format!("{} {}\n", name, v));
            }
            metriken::Value::Other(any) => {
                // Try to downcast to AtomicHistogram
                if let Some(histogram) = any.downcast_ref::<metriken::AtomicHistogram>()
                    && let Some(snapshot) = histogram.load()
                {
                    output.push_str(&format!("# TYPE {} histogram\n", name));

                    // Output percentiles as summary-style metrics
                    let percentiles = [50.0, 90.0, 95.0, 99.0, 99.9, 99.99];
                    if let Ok(Some(results)) = snapshot.percentiles(&percentiles) {
                        for (pct, bucket) in results {
                            let quantile = pct / 100.0;
                            output.push_str(&format!(
                                "{}{{quantile=\"{}\"}} {}\n",
                                name,
                                quantile,
                                bucket.end()
                            ));
                        }
                    }

                    // Output count and sum
                    let mut count = 0u64;
                    let mut sum = 0u64;
                    for bucket in snapshot.into_iter() {
                        let bucket_count = bucket.count();
                        count += bucket_count;
                        // Use midpoint of bucket for sum approximation
                        let midpoint = (bucket.start() + bucket.end()) / 2;
                        sum += bucket_count * midpoint;
                    }
                    output.push_str(&format!("{}_count {}\n", name, count));
                    output.push_str(&format!("{}_sum {}\n", name, sum));
                }
            }
            // Handle any future Value variants
            _ => {}
        }
    }

    output
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
    // Collect snapshots during the run
    let mut snapshots: Vec<Snapshot> = Vec::new();

    tracing::info!("parquet recorder started, will write to {:?}", path);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                // Only collect snapshots during the running phase (skip warmup)
                if shared.phase().is_recording() {
                    snapshots.push(create_snapshot());
                }
            }
            _ = stop_notify.notified() => {
                break;
            }
            _ = async {
                while !shared.phase().should_stop() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            } => {
                break;
            }
        }
    }

    // Final snapshot
    snapshots.push(create_snapshot());

    // Write all snapshots to parquet
    if !snapshots.is_empty() {
        // First pass: build schema
        let mut schema = ParquetSchema::new();
        for snapshot in &snapshots {
            schema.push(snapshot.clone());
        }

        // Create file and writer
        let file = File::create(&path).map_err(|e| io::Error::other(e.to_string()))?;

        let metadata = HashMap::from([
            (
                "sampling_interval_ms".to_string(),
                interval.as_millis().to_string(),
            ),
            ("source".to_string(), "cachecannon".to_string()),
            ("version".to_string(), env!("CARGO_PKG_VERSION").to_string()),
        ]);

        let mut writer = schema
            .finalize(file, ParquetOptions::new(), Some(metadata))
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Second pass: write data
        for snapshot in snapshots {
            if let Err(e) = writer.push(snapshot) {
                tracing::warn!("failed to write parquet snapshot: {}", e);
            }
        }

        if let Err(e) = writer.finalize() {
            tracing::warn!("failed to finalize parquet: {}", e);
        }

        tracing::info!("parquet recorder stopped, wrote {:?}", path);
    } else {
        tracing::info!("parquet recorder stopped, no snapshots to write");
    }

    Ok(())
}
