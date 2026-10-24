//! Per-thread sharded latency histograms.
//!
//! A single [`metriken::AtomicHistogram`] is one process-wide bucket array.
//! Every response from every worker thread does a `fetch_add` on it, and a
//! benchmark's latencies cluster into a handful of buckets, so all workers
//! contend on the same few cache lines. That contention caps the whole
//! process at a throughput no thread count can move. The request counters
//! avoid the same problem with per-thread shards (see
//! [`crate::sharded_counter`]); this module does the same for histograms.
//!
//! Each worker thread increments its own shard. Readers merge the shards on
//! `load()`, which is a snapshot operation on the reporting path, not the
//! hot path. Shards are allocated lazily, so a thread that never records a
//! value costs nothing.

use std::sync::atomic::{AtomicBool, Ordering};

use metriken::histogram::{Config, Error, Histogram};
use metriken::{AtomicHistogram, HistogramMetric, Metric, Value};

use crate::sharded_counter::{NUM_SHARDS, shard_index};

/// A latency histogram with one [`AtomicHistogram`] shard per worker thread.
///
/// `G` is the grouping power and `M` the max value power of every shard; see
/// [`AtomicHistogram::new`] for their constraints.
pub struct ShardedHistogram<const G: u8, const M: u8> {
    shards: [AtomicHistogram; NUM_SHARDS],
    /// Set on the first increment so `load()` can answer `None` without
    /// touching every shard for a histogram nothing has written to.
    written: AtomicBool,
}

impl<const G: u8, const M: u8> ShardedHistogram<G, M> {
    /// Create a sharded histogram. Shards allocate on first use.
    #[allow(clippy::declare_interior_mutable_const)]
    pub const fn new() -> Self {
        Self {
            shards: [const { AtomicHistogram::new(G, M) }; NUM_SHARDS],
            written: AtomicBool::new(false),
        }
    }

    /// Increment the bucket containing `value` in the calling thread's shard.
    #[inline]
    pub fn increment(&self, value: u64) -> Result<(), Error> {
        if !self.written.load(Ordering::Relaxed) {
            self.written.store(true, Ordering::Relaxed);
        }
        self.shards[shard_index()].increment(value)
    }

    /// The configuration shared by every shard.
    pub fn config(&self) -> Config {
        self.shards[0].config()
    }

    /// Merge every shard into one snapshot. Returns `None` if nothing has
    /// ever been recorded.
    pub fn load(&self) -> Option<Histogram> {
        if !self.written.load(Ordering::Relaxed) {
            return None;
        }
        let mut merged: Option<Histogram> = None;
        for shard in &self.shards {
            let Some(snapshot) = shard.load() else {
                continue;
            };
            merged = Some(match merged {
                None => snapshot,
                Some(acc) => acc
                    .wrapping_add(&snapshot)
                    .expect("shards share one histogram config"),
            });
        }
        merged
    }
}

impl<const G: u8, const M: u8> Default for ShardedHistogram<G, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const G: u8, const M: u8> Metric for ShardedHistogram<G, M> {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::Histogram(self))
    }
}

impl<const G: u8, const M: u8> HistogramMetric for ShardedHistogram<G, M> {
    fn config(&self) -> Config {
        ShardedHistogram::config(self)
    }

    fn load(&self) -> Option<Histogram> {
        ShardedHistogram::load(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharded_counter::set_thread_shard;

    fn total(h: &Histogram) -> u64 {
        h.as_slice().iter().sum()
    }

    #[test]
    fn empty_histogram_loads_none() {
        let h: ShardedHistogram<7, 64> = ShardedHistogram::new();
        assert!(h.load().is_none());
    }

    #[test]
    fn config_matches_parameters() {
        let h: ShardedHistogram<5, 32> = ShardedHistogram::new();
        assert_eq!(h.config().grouping_power(), 5);
        assert_eq!(h.config().max_value_power(), 32);
    }

    #[test]
    fn increments_from_many_threads_merge_into_one_snapshot() {
        static H: ShardedHistogram<7, 64> = ShardedHistogram::new();
        const THREADS: usize = 8;
        const PER_THREAD: u64 = 1000;

        let handles: Vec<_> = (0..THREADS)
            .map(|id| {
                std::thread::spawn(move || {
                    set_thread_shard(id);
                    for i in 0..PER_THREAD {
                        H.increment(1_000 + i).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let merged = H.load().expect("values were recorded");
        assert_eq!(total(&merged), THREADS as u64 * PER_THREAD);

        // Every recorded value lands in the merged snapshot's 1_000..2_000 range,
        // so the merge carried the buckets, not just a count.
        let in_range: u64 = merged
            .iter()
            .filter(|b| b.start() >= 1_000 && b.end() < 2_000 + 128)
            .map(|b| b.count())
            .sum();
        assert_eq!(in_range, THREADS as u64 * PER_THREAD);
    }

    #[test]
    fn threads_on_distinct_shards_write_distinct_buckets_arrays() {
        static H: ShardedHistogram<7, 64> = ShardedHistogram::new();
        let a = std::thread::spawn(|| {
            set_thread_shard(1);
            H.increment(10).unwrap();
        });
        let b = std::thread::spawn(|| {
            set_thread_shard(2);
            H.increment(20).unwrap();
        });
        a.join().unwrap();
        b.join().unwrap();
        assert!(H.shards[1].load().is_some());
        assert!(H.shards[2].load().is_some());
        assert!(H.shards[3].load().is_none());
        assert_eq!(total(&H.load().unwrap()), 2);
    }

    #[test]
    fn wrapping_sub_against_a_baseline_snapshot_works() {
        static H: ShardedHistogram<7, 64> = ShardedHistogram::new();
        set_thread_shard(0);
        H.increment(100).unwrap();
        let baseline = H.load().unwrap();
        H.increment(100).unwrap();
        H.increment(100).unwrap();
        let delta = H.load().unwrap().wrapping_sub(&baseline).unwrap();
        assert_eq!(total(&delta), 2);
    }
}
