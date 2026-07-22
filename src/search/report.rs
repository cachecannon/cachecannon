//! Terminal report for a search run.
//!
//! Recall is the headline; latency and throughput are only meaningful next to
//! it, and a recall number without its definition and knobs is a lie — so the
//! report always echoes k, ef_search, M, ef_construction, and the dataset.

use std::time::Duration;

use super::SearchArgs;
use super::dataset::Dataset;

pub struct PhaseTimings {
    pub items: usize,
    pub elapsed: Duration,
    /// Effective rows per pipelined batch (load phase only; the byte budget
    /// may cap it below `--load-batch`).
    pub batch: usize,
}

pub struct QueryStats {
    pub queries: usize,
    pub elapsed: Duration,
    pub latencies_ns: Vec<u64>,
    pub mean_recall: f64,
}

pub struct RunReport {
    pub load: PhaseTimings,
    pub index: PhaseTimings,
    pub query: QueryStats,
}

/// Nearest-rank percentile (1-indexed rank ceil(q/100 * n)).
fn percentile_ns(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((q / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

pub fn print(report: &RunReport, args: &SearchArgs, dataset: &Dataset) {
    let mut sorted = report.query.latencies_ns.clone();
    sorted.sort_unstable();
    let mean_ns = if sorted.is_empty() {
        0
    } else {
        sorted.iter().sum::<u64>() / sorted.len() as u64
    };
    let qps = report.query.queries as f64 / report.query.elapsed.as_secs_f64();
    let load_rate = report.load.items as f64 / report.load.elapsed.as_secs_f64();

    println!();
    println!("search results");
    println!("==============");
    println!(
        "config       index={} metric={} dim={} k={} ef_search={} M={} ef_construction={}",
        args.index,
        dataset.metric,
        dataset.dim(),
        args.k,
        args.ef_search,
        args.m,
        args.ef_construction,
    );
    println!(
        "load         {} vectors in {:.2}s ({:.0} vectors/s, pipelined batches of {})",
        report.load.items,
        report.load.elapsed.as_secs_f64(),
        load_rate,
        report.load.batch,
    );
    println!(
        "index build  {:.2}s (FT.CREATE to ready: backfill complete, mutation queue empty)",
        report.index.elapsed.as_secs_f64(),
    );
    println!(
        "queries      {} single-client queries in {:.2}s ({qps:.0} QPS)",
        report.query.queries,
        report.query.elapsed.as_secs_f64(),
    );
    println!(
        "latency ms   p50 {:.3}  p90 {:.3}  p99 {:.3}  p99.9 {:.3}  mean {:.3}  max {:.3}",
        ms(percentile_ns(&sorted, 50.0)),
        ms(percentile_ns(&sorted, 90.0)),
        ms(percentile_ns(&sorted, 99.0)),
        ms(percentile_ns(&sorted, 99.9)),
        ms(mean_ns),
        ms(sorted.last().copied().unwrap_or(0)),
    );
    println!(
        "recall@{}    {:.4}  (ann-benchmarks definition: |returned ∩ true_k| / k)",
        args.k, report.query.mean_recall,
    );
}
