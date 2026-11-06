# Latency histograms from `metriken`

- **Opened:** 2026-11-04
- **Status:** CLOSED — shipped in #275. `ShardedHistogram` is replaced by
  `metriken::ShardedCounterHistogram`, after a change to `metriken` released as
  0.11.7. Against `main` at `53242aa`, GET p50 and p99 at 1.38M/s and
  closed-loop throughput stayed inside the A-vs-A range. Zen 1 only.

`src/sharded_histogram.rs` (195 lines, #259 and #264) gave each worker its own
`AtomicHistogram` buckets. `metriken` now publishes a sharded histogram. The
question was whether `ShardedHistogram` could be removed without a performance
regression.

Set aside: `ShardedCounterHistogramGroup`, which cachecannon does not use, and
any change to how the histograms are exposed.

All numbers below are measured unless marked derived. Microbenchmarks and
end-to-end runs used one 24-vCPU guest on hv02 (Threadripper 1950X, Zen 1),
Debian 13, Linux 6.18, rustc 1.99.1. vCPUs 0 and 1 are one physical core, and
the same holds for each following pair. Ratios are paired by round, written as
median [min, max] of 8 rounds. The microbenchmark and the raw results are not
in the tree; the SystemsLab experiment IDs identify the runs.

## Function

At `(7, 64)`, the only configuration `LatencyHistogram` uses, `ShardedHistogram`
and `metriken` 0.11.7's type produced identical buckets for 300 threads and 60M
values including `0`, `1`, `2^63` and `u64::MAX`, with one thread in ten setting
no shard id. A second run at `(5, 10)`, with values masked to 0-2047, gave
identical buckets and identical errors. This was a release build on macOS, and
the test is not in the tree. Both types in that test were built on `histogram`
1.6.5, so it does not compare `histogram` versions.

Separately, `histogram` 1.4.1 and 1.6.5 `AtomicHistogram` at `(7, 64)` gave
identical buckets, errors, bucket bounds and ten quantiles over 202M values, on
arm64 and on x86_64 under Rosetta. That test is not in the tree.

`ShardedHistogram` and the `metriken` type differ in four ways, and none changes
a reported number:

- After only out-of-range writes, `ShardedHistogram` returned an empty histogram
  from `load()` and metriken's returns `None`. At `(7, 64)` every `u64` is in
  range.
- A thread with no shard id used an address-derived shard in `ShardedHistogram`.
  Metriken's takes its own id from a pool. Totals are the same.
- Metriken's reads its own thread-local. ringline sets it on every worker
  (`ringline/src/worker.rs:1423` at `ab81ceb`), and `set_thread_shard` in
  `src/sharded_counter.rs` now sets both. No production thread records into a
  `LatencyHistogram` without a shard id.
- A static is 14,368 bytes against 22,536. Shard buckets are the same 58 KiB,
  allocated on first write in both.

## What the microbenchmark showed

The benchmark increments one histogram in a loop from N threads. B is
`ShardedHistogram`, B2 a byte copy of B for the A-vs-A range, and C `metriken`
0.11.6. D2, D3 and D4 below are G, H and J in the raw logs.

**Prediction refuted: C equals B.** I predicted the swap would sit inside the
A-vs-A range. Run `01a1a851` gave C/B 0.977 at 8 threads, with 0.976 and 0.977
at 4 and 24 threads. The largest A-vs-A gap at 1, 4, 8 and 24 threads was 2.5%,
so I repeated the comparison, and `01a1ab8b` and `01a1abab` gave 0.9771 and
0.9753. Twelve threads is excluded: the two identical binaries ranged 0.881 to
1.231 there. In the first run the dependency bump alone (`metriken` 0.11.3 to
0.11.6, which brings `histogram` 1.6.5) raised throughput 6.7% at 8 threads.
That run's bench restricted N threads to vCPUs `0..N`, so 8 threads ran on 4
cores.

**Cause of the gap.** `metriken` 0.11.6 called `in_range` in `add`, and
`AtomicHistogram::add` then checked the same range. Moving the first check out
of `add` (arm D) gave D/B 1.016 with threads sharing cores and 1.000 with one
thread per core, where C/B was 0.977 and 1.000 (`01a1a93d`). Removing the check
changes about 47 instructions and 8 branches per increment to about 44 and 7
(`01a1a93d`, `perf stat`).

**Refuted: D is the fix.** D was faster in the one-call-site benchmark and was
not run end to end. In the real binary the larger `add` stopped being inlined:
32 sites called an outlined `add`, and one increment took 47 instructions, three
pushes and pops among them, against 38 for B and C (cross-compiled x86 build,
`RESPONSE_LATENCY` in `record_routed_latency`). The benchmark has one call site,
so `add` stayed inlined and the benchmark did not show this. A second benchmark
with three call sites and the `Result` discarded, the shape of `src/worker.rs`,
gave D/B 0.815 (`01a1ab8b`).

**Refuted: D2, a `#[cold]` function that does the add and returns `Result`.**
Prediction: at least 1.005 of B at one call site. Measured 0.893, and 0.892 on
repeat. At three call sites it was 1.053. I attribute the loss to the 24-byte
`Result` being returned through memory, from reading the assembly. The same run
refuted a prediction that 8 threads on 8 cores would show no difference between
arms: D2 was about 10% slower there.

**D3, a cold function returning `Option<&AtomicHistogram>`.** All five
predictions held (`01a1abab`): 1.0145 at one call site, 1.0719 at three, both
8 threads on 4 cores. Review then found a behaviour change the runs could not
show: `shard_index()` still ran first, so an out-of-range write took a pool
shard, which 0.11.6 does not do.

**D4, shipped.** The range check and `shard_index` both moved into `init_shard`
(iopsystems/metriken#375, `f18d34c`). Ratios to 0.11.6 (`01a1ae7f`, 360 runs):

| Benchmark | 1 thread | 8 threads on 4 cores | 8 threads on 8 cores |
|---|---|---|---|
| Three call sites | 0.9995 [0.979, 1.023] | 1.075 [1.047, 1.089] | 1.016 [0.995, 1.050] |
| One call site, `black_box` | 0.9985 [0.981, 1.005] | 1.039 [0.526, 1.045] | 0.9995 [0.962, 1.004] |
| One call site, `is_err()` | 0.965 [0.951, 0.981] | 1.036 [1.033, 1.051] | 0.961 [0.867, 1.088] |

A-vs-A medians were 0.991 to 1.001. Two results are not explained. The
`is_err()` loss at 1 thread is outside A-vs-A and reproduced across sessions. In
`01a1abab`, D2's and D3's increment loops in that benchmark had the same
instructions apart from register names and `inc` placement and read 1.0073 and
0.9551 of B, so I suspect code layout, and I did not test that. One run in the
`black_box` row ran at half speed. It is the only run below 0.75 of its arm's
median in the 1120 of `01a1ab8b`, `01a1abab` and `01a1ae7f`. `01a1a851` and
`01a1a93d` each had one such run, at 0.66 and 0.60.

## End to end

Workload: memcached on vCPUs 8-23, restarted per run, over loopback, with 256
connections, pipeline 16, 80/20 get/set, 64-byte values and 100,000 keys, 20 s
measured. Runs use the default 60 s `prefill_settle`.

Two earlier runs used D3 and did not answer the closed-loop question.
`01a1ac4e` had memcached at 8.0 of 8 threads, so it measured the server.
`01a1acb3` raised memcached to 16 threads; its busiest sample was 13.5 against
the limit of 13 written beforehand, so that one did not count either. At 1.38M/s
p50 and p99 did not move in `01a1ac4e`.

The final run compares what this change actually does. `01a1af9d` hit my 7200 s
`payload_timeout` after 6 rounds and returned nothing, so `01a1b024` reran the
same arms with a longer limit. All 96 runs had no errors, timeouts or drops. The
letters restart here: this B is not the microbenchmark's B.

- A: `main` at `53242aa` (`metriken` 0.11.3, `histogram` 1.4.1,
  `ShardedHistogram`)
- A2: byte copy of A
- B: A plus `metriken` 0.11.7, `ShardedHistogram` kept
- S: this change

Modes: rl, 8 workers on vCPUs 0-7 at 1,378,588/s. cl8, closed loop, 8 workers on
0-7. cl2, closed loop, 2 workers on vCPUs 0-1, one core.

| Mode | Statistic | A | A2/A | B/A | S/A |
|---|---|---|---|---|---|
| rl | GET p50 | 286 µs | 1.0000 [0.993, 1.007] | 1.0035 [1.000, 1.007] | 1.0000 [0.993, 1.007] |
| rl | GET p99 | 942 µs | 1.0000 [0.974, 1.021] | 1.0084 [0.979, 1.022] | 1.0000 [0.983, 1.013] |
| cl8 | throughput | 2.907M/s | 0.9998 [0.993, 1.006] | 1.0005 [0.991, 1.008] | 1.0001 [0.995, 1.005] |
| cl2 | throughput | 776,848/s | 0.9972 [0.992, 1.009] | 0.9949 [0.978, 1.011] | 0.9992 [0.990, 1.013] |
| cl2 | cycles:u | | 0.9958 [0.980, 1.010] | 1.0190 [1.010, 1.037] | 0.9949 [0.988, 1.014] |

Reported latency values are bucket ends, so rl p50 resolves about 0.7% and p99
about 0.4%. A 1.0000 median can come from rounds one bucket either side: S/A p99
is the mean of 0.9957 and 1.0042, and no round matched A's bucket.

Predictions, written before the run, all held: the S/A median inside the A2/A
range for rl p50 and p99 and for cl8 and cl2 throughput, and at least 0.99 for
cl2. The cl2 validity rule, a median busiest memcached sample below 8 of 16
threads and throughput below 2.0M/s, was met.

cl2 is the run that shows the generator as the limit. memcached's busiest sample
had a median of 3.7 of 16 threads at 0.78M/s, and it reached 2.9M/s on this
guest. cl2 resolves about ±0.9%, the A-vs-A range, so it cannot show the effect
predicted before the run, derived as under 10 cycles per response against about
6,800 for a generator on 4 cores at 2M/s, under 0.2%. cl8 has the weakness of
the earlier runs: memcached at 13.5 of 16 threads, so it does not show the
generator was the limit.

Latency beyond p99 varies too much between identical binaries to compare. In rl,
A2/A alone spans 0.666 to 1.301 at p99.9, and S/A 0.778 to 1.205 sits inside it.

`perf stat` counted user-mode cycles (`cycles:u`) for the whole process,
including prefill and warmup, so the per-response values are useful only as
ratios. Kernel-mode cycles are not counted.

B's 1.9% more user-mode cycles than A in cl2 is outside A-vs-A and S does not
have it. B also read 1.0125 in rl and 1.0145 in cl8, both above the A2/A range.
I do not know why B costs cycles here, and B's throughput stays inside the
A-vs-A range. The microbenchmark measured a 6.7% gain for a similar dependency
bump at 8 threads on 4 cores and none at 1 thread, on the increment loop alone.

## Not measured

- ARM, including the Graviton4 shape in the #259 CHANGELOG entry. Reopen if a
  run on ARM shows p50 or p99 outside its A-vs-A range against this change.
- Connection counts other than 256, and pipelines other than 16.
- Latency beyond p99.
