//! Vector-search benchmarking (`valkey-lab search`).
//!
//! Measures Valkey Search (`FT.*`) performance the way ann-benchmarks does:
//! load a dataset, build an HNSW index, then run a fixed query set and score
//! every result against precomputed ground truth. A result is a point in
//! (recall, latency, QPS) space, never a lone number.
//!
//! Three phases, measured independently (see issue #113):
//!
//! - **load**: pipelined `HSET doc:<row> vec <f32-LE bytes>` for every `train`
//!   vector.
//! - **index**: `FT.CREATE` an HNSW index, then poll `FT.INFO` until the
//!   backfill completes and the mutation queue drains. Build time is measured
//!   from issuing `FT.CREATE` to the ready condition, not the `FT.CREATE`
//!   reply (which returns before indexing finishes).
//! - **query**: the `test` vectors, single client, one request in flight, via
//!   `FT.SEARCH` KNN at one `ef_search`. Latency is recorded per query and
//!   recall@k is scored against the `neighbors` ground truth.
//!
//! This module deliberately does not reuse the KV workload machinery: search
//! replays a fixed query set and scores correctness, it does not hammer the
//! server with synthetic traffic.

mod dataset;
mod phases;
mod report;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};

use clap::Args;

use dataset::Dataset;
use report::RunReport;

/// Channel carrying the worker task's outcome back to the main thread.
type Outcome = Result<RunReport, String>;

/// `valkey-lab search` flags.
#[derive(Args, Debug, Clone)]
pub struct SearchArgs {
    /// Path to an ann-benchmarks-format HDF5 dataset
    /// (`train`, `test`, `neighbors` datasets; `distance` attribute).
    #[arg(long)]
    pub dataset: PathBuf,

    /// Server hostname or IP.
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub host: String,

    /// Server port.
    #[arg(short = 'p', long, default_value_t = 6379)]
    pub port: u16,

    /// Neighbors to retrieve per query (recall is scored at this k).
    #[arg(short = 'k', long, default_value_t = 10)]
    pub k: usize,

    /// HNSW `EF_RUNTIME` for the query phase.
    #[arg(long, default_value_t = 128)]
    pub ef_search: usize,

    /// HNSW `M` (max outgoing edges per node).
    #[arg(long, default_value_t = 16)]
    pub m: usize,

    /// HNSW `EF_CONSTRUCTION`.
    #[arg(long, default_value_t = 200)]
    pub ef_construction: usize,

    /// Index name.
    #[arg(long, default_value = "vlab-search")]
    pub index: String,

    /// Number of HSETs per pipelined batch in the load phase.
    #[arg(long, default_value_t = 256)]
    pub load_batch: usize,

    /// Keep the index and keys after the run (default drops the index).
    #[arg(long, default_value_t = false)]
    pub keep_index: bool,
}

/// Everything the ringline worker task needs, installed before launch.
struct Job {
    args: SearchArgs,
    endpoint: SocketAddr,
    dataset: Dataset,
    tx: Mutex<Option<Sender<Outcome>>>,
}

static JOB: OnceLock<Job> = OnceLock::new();

/// Handler with a single job: run the three phases once on worker 0.
struct SearchHandler {
    worker_id: usize,
}

impl ringline::AsyncEventHandler for SearchHandler {
    #[allow(clippy::manual_async_fn)]
    fn on_accept(&self, _conn: ringline::ConnCtx) -> impl Future<Output = ()> + 'static {
        // Client-only: no accepts expected.
        async {}
    }

    fn create_for_worker(worker_id: usize) -> Self {
        SearchHandler { worker_id }
    }

    fn on_start(&self) -> Option<Pin<Box<dyn Future<Output = ()> + 'static>>> {
        if self.worker_id != 0 {
            return None;
        }
        Some(Box::pin(async {
            let job = JOB.get().expect("search job installed before launch");
            let result = phases::run_all(job).await;
            let tx = job
                .tx
                .lock()
                .expect("job sender lock")
                .take()
                .expect("job sender taken once");
            // Receiver hangup means the main thread already gave up; nothing
            // useful to do from a worker task.
            let _ = tx.send(result);
        }))
    }
}

/// Entry point for the `search` subcommand.
pub fn run(args: SearchArgs) -> Result<(), Box<dyn std::error::Error>> {
    let dataset = dataset::load(&args.dataset)?;
    if args.k > dataset.neighbors_k() {
        return Err(format!(
            "k = {} exceeds ground-truth depth {} in {}",
            args.k,
            dataset.neighbors_k(),
            args.dataset.display()
        )
        .into());
    }
    if args.k > 1000 {
        // ringline-redis parses replies with a 1024-element collection cap and
        // closes the connection on overflow; NOCONTENT keeps replies at 1 + k.
        return Err("k > 1000 exceeds the reply-size ceiling (see issue #113)".into());
    }

    let endpoint = (args.host.as_str(), args.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| format!("could not resolve {}:{}", args.host, args.port))?;

    println!(
        "dataset: {} ({} train, {} test, dim {}, metric {})",
        args.dataset.display(),
        dataset.train.nrows(),
        dataset.test.nrows(),
        dataset.dim(),
        dataset.metric,
    );

    let (tx, rx): (Sender<Outcome>, Receiver<Outcome>) = channel();
    JOB.set(Job {
        args: args.clone(),
        endpoint,
        dataset,
        tx: Mutex::new(Some(tx)),
    })
    .map_err(|_| "search job may only be installed once per process")?;

    let ringline_config = ringline::ConfigBuilder::new()
        .workers(1)
        .pin_to_core(false)
        .tcp_nodelay(true)
        .build()?;
    let (shutdown_handle, handles) =
        ringline::RinglineBuilder::new(ringline_config).launch::<SearchHandler>()?;

    let outcome = rx.recv();
    shutdown_handle.shutdown();
    for handle in handles {
        let _ = handle.join();
    }

    let job = JOB.get().expect("search job installed");
    match outcome {
        Ok(Ok(run_report)) => {
            report::print(&run_report, &job.args, &job.dataset);
            Ok(())
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err("search worker exited without reporting a result".into()),
    }
}
