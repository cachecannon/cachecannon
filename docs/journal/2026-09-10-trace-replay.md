# Cache trace replay

- **Opened:** 2026-09-10
- **Status:** OPEN — scoping, pre-build. No code yet. Records what replay would
  have to do, what it deliberately would not, and the cheaper artifact that may
  make it unnecessary.
- **PRs:** none yet

## Problem

Every keyspace cachecannon can generate is synthetic and memoryless:
`[workload.keyspace]` offers `count`, `length`, `format` and `distribution`
(uniform or zipf). Both distributions are stationary — the popularity of a key
is fixed for the life of the run.

Real cache traffic is not. Keys become hot, cool, and are replaced; the working
set at an instant is not the working set over a day; and the popularity
distribution is frequently not zipf at all. The Twitter production cache study
(Yang et al., OSDI '20, 54 clusters, week-long traces) found exactly that, and
it is the standard public evidence on the point.

That leaves a gap that matters when the numbers inform capacity. Measuring with
`uniform` gives a genuine worst case: every key equally likely means the cache
gets no help from locality. Measuring with `zipf` gives a better-looking case
that is arguably *less* realistic, because it has roughly the right skew and
entirely the wrong dynamics — no churn, no rotation, no drift. Neither is a
forecast, and a reader cannot tell from a result which of the two it is closer
to for their workload.

## What the question actually needs

The constraint that drove this is a tail-latency one: a cache serving from a
device cannot meet a millisecond-scale p99 unless nearly all requests are served
from memory, because a device read costs milliseconds and the 99th percentile
request is by definition among the worst one percent. Measured on a
disk-backed server: 55.8K req/s SLO-compliant with the working set at 57% of
RAM, 4.8K at 113%, and unmeetable at 227% where the median itself floors above
the target.

So the sizing question reduces to: **what hit rate does this workload get at
this cache size?** That is a miss-ratio curve, and an MRC does not require
replay. It can be computed from a sampled key-access trace — `memcached`'s own
`watch fetchers` stream is one source — using SHARDS (Waldspurger et al., FAST
'15) to get an accurate curve from roughly a one percent sample.

**This matters for whether to build replay at all.** An MRC answers "how much
memory for what hit rate". It does not answer "what throughput and tail latency
at that hit rate", because it models the cache, not the server. Replay is the
tool for the second question. Get the MRC first: it is cheaper, it may bound the
answer well enough on its own, and it tells us whether replay is even needed.

## Scope

### Replay semantics — the decision that matters most

Three modes are possible and they are different experiments:

1. **Key-sequence replay.** Take the trace's key order; keep cachecannon's own
   rate control. Preserves the access pattern — skew, churn, locality, reuse
   distance — and discards the trace's timing.
2. **Timing-faithful replay.** Reproduce inter-arrival times as recorded.
   Preserves burstiness; makes the rate a property of the trace.
3. **Scaled replay.** Trace timing multiplied by a speedup factor.

**(1) is the primary mode and the only one in scope initially.** The reason is
that the saturation search — the feature that produces the number we care about
— requires cachecannon to control the offered rate. Under (2) the rate is fixed
by the trace, so "maximum SLO-compliant throughput" is not a question that can
be asked. (2) answers a real and different question (does the server survive the
real burst pattern), and should be its own effort rather than a flag bolted onto
this one.

### In scope

- A normalized trace format, columnar (parquet), streamed rather than loaded:
  a week of production traffic does not fit in memory, and the ecosystem here
  already reads and writes parquet.
- Converters as separate tooling, not in the hot path: `memcached watch`
  output and the Twitter trace CSV are the two worth supporting first.
- Key mapping. Trace keys are hashed or anonymized and are the wrong shape for
  servers that constrain key format. Needs a deterministic trace-key to
  on-wire-key function, injective enough to preserve identity — the same trace
  key must always produce the same on-wire key, or reuse distance is destroyed
  and the trace measures nothing.
- **Variable value sizes.** Traces carry them and they drive both bytes on the
  wire and memory pressure, which is the mechanism under study.
  `[workload.values]` currently has a single fixed `length`; the value pool is
  one gigabyte of random bytes sliced at that length. Honoring per-key sizes is
  a real change, not a detail.
- Sharding. A trace comes from a fleet and is replayed against one node.
  Something of the form `--shard k/N` over a hash of the key, so a single server
  sees a representative fraction rather than the whole fleet's traffic.
- Warmup by trace prefix. With a trace, "prefill" is ill-defined: prefilling the
  whole key universe is wrong, since the universe includes keys that appear once
  a week. Replaying a leading window of the trace and discarding its results is
  both realistic and consistent with how an MRC is computed.

### Explicitly out of scope

- Timing fidelity (mode 2 above) — a separate effort.
- TTL and expiry emulation.
- Per-client fidelity: traces carry client ids; connection assignment by client
  is not modelled.
- Value *contents*. Sizes matter; bytes do not.
- Writing traces. cachecannon replays; capture belongs to whatever produced it.

## Decision criteria

Build this when both hold:

1. An MRC from real traffic shows the workload's hit rate at a plausible node
   size sits in a regime where the synthetic numbers do not transfer — that is,
   materially better than `uniform` at the same working-set ratio.
2. A question remains that the MRC cannot answer, specifically achievable
   throughput or tail latency under that access pattern.

If (1) fails, the uniform floor is the answer and replay adds nothing. If (2)
fails, the MRC is the answer and replay is unnecessary. Neither is known yet,
which is why this entry is open and no code exists.

## Open questions

- Does key-sequence replay without timing actually preserve the property that
  matters? Reuse distance is preserved by construction, but a cache's behaviour
  also depends on *rate* of reuse relative to eviction. Worth validating against
  a trace with known MRC before trusting a replay result.
- How is a trace's keyspace reconciled with a run that wants a specific working
  set size? Sharding changes the working set; so does truncating the trace.
  Both need to be stated in a result or it is not reproducible.
- What is the minimum useful trace length, and how is it decided rather than
  guessed?
- Does the saturation search still mean the same thing under replay? Its knee is
  currently a property of the server at a fixed access pattern; under replay the
  pattern varies within a run, so a single knee may not be well defined.

## Evidence

- No replay or trace code exists: `grep -ri "trace\|replay" src/` returns only
  `tracing` imports.
- `[workload.keyspace]` is `count`, `length`, `format`, `distribution`,
  `zipf_theta` — nothing sequence-derived.
- `[workload.values]` is a single `length`; `VALUE_POOL_SIZE` is a 1 GiB random
  pool sliced at that length (`src/runner.rs`, `src/worker.rs::value_slice`).
- Measured tail behaviour that motivates the question, on a disk-backed server
  under `uniform` at 113% of RAM: p50 356us and p99 979us both inside a
  500us/1ms SLO, while p999 was 44.3ms. A median-based health check calls that
  configuration healthy.
