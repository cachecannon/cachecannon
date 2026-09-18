# Generator saturation at high connection counts

**Status:** OPEN — one hypothesis refuted, one fix shipped, the root cause
still unidentified and attributable to a constant this effort introduced.

Opened 2026-09-18, following a cache-server characterisation run on 2026-09-16/17
that reported latency which turned out to be substantially the load generator's
own response-reaping delay.

## What we set out to do

A characterisation run (c6gn.4xlarge generator, 16 vCPU Graviton2, 25 GbE ->
i4i.4xlarge; 100% GET, 56 KiB values, fixed 20,000 req/s) measured p50 rising
283 us -> 2130 us as connections went 64 -> 10,000. Client-side
`tcp_packet_latency` — socket-becomes-readable to userspace-reads-it — showed
**72% of the reported p50 at 10,000 connections was the generator failing to
read responses that had already arrived**, with TCP srtt flat (203-354 us)
throughout. The target was being blamed for the client.

Two things were wanted: find why the generator saturates, and make it say so
rather than reporting the delay as target latency.

## What shipped

| Commit | PR | Effect |
|---|---|---|
| `3409f3a` | #150 | Removed `threads = 8` from the guide's "High-Throughput Configuration". The default is `available_parallelism()` and always has been; the example was capping generators that would otherwise size themselves. Added a "Is the generator the bottleneck?" troubleshooting entry with the `tcp_packet_latency` check. |
| `ab3a54c` | #151 | `Value::CounterGroup` was falling through a catch-all at both exposition sites, so every ringline runtime counter was incremented and then dropped. `ringline/pool{op="recv_parked"}`, `ringline/bytes{op="fallback_received"}` and the rest now reach `/metrics` and the parquet snapshot. Also fixed `/metrics` serving invalid Prometheus text — linking ringline registers `ringline/connections/active`, whose `/` is not a legal metric name. |
| `6e7bfe9` | #152 | `general.recv_ring_size` / `general.recv_buffer_size`, both defaulting to ringline's own values. Instrumentation only — see the refuted hypothesis below. |
| `f56f1b9` | #158 | Removed `jittered_idle_sleep`. See "The jitter episode". |

Released as v0.0.24 (`c1ed1db`), alongside #157 which is unrelated to this
effort.

## Refuted: recv-buffer ring starvation

**The hypothesis.** ringline's provided-buffer ring defaults to 256 buffers x
16 KiB per worker, shared by every connection on that worker. A 56 KiB response
needs four of them. At 10,000 connections over 8 workers that is 1,250
connections per worker against 256 buffers — 0.2 buffers per connection — so
the ring should empty, multishot recv should terminate with `ENOBUFS`, and
connections should park with their sockets readable and nothing reading them.
The latency knee lined up with the point where buffers-per-connection fell
below the four a response needs.

**The refutation.** Direct measurement with `ringline_diag = true` on the same
rig, `threads` unset (16 workers), 10,000 connections: every worker reported
`parks=0 fallbacks=0`. The ring never ran dry.

**The mechanism error.** The arithmetic divided ring depth by *connections*, but
a connection does not hold a buffer — a buffer is held only between a completion
and the client draining it. At ~1,250 responses/sec/worker over a ~65 us loop
iteration that is a fraction of one buffer in use on average against 256
available, roughly three orders of magnitude of headroom. Buffers-per-connection
was never a meaningful ratio.

**The methodological error**, which is the more reusable one: the correlation
between buffers-per-connection and reaping latency was two monotone functions of
connection count. Any two such series line up. It was labelled a hypothesis and
then ranked first anyway, and cost the reporting engineer a day.

A second attempt to justify the same change on CQE count also fails. That
generator was spending ~12M CQEs/s to move 20,000 req/s (from `iters=4568370`,
`cqes_1st_avg=60.32`, `cqes_2nd_avg=39.53` across 16 workers); request I/O
accounts for under 1% of that, so fitting a response into one buffer moves
~0.5% of the budget.

**Condition to reopen:** a workload showing non-zero
`ringline/pool{op="recv_parked"}` or `ringline/bytes{op="fallback_received"}`,
now that #151 makes both visible. ringline's own server-side sweeps do reach
millions of parks at small ring depths, so the geometry has a regime where it
matters — client-side at these payloads is not it.

## The jitter episode

`2378ee9` (#153) replaced a fixed 100 us per-connection idle poll with one
scaling as `connections / (64 * rate)`. That part is sound. It also carried
`jittered_idle_sleep`, spreading each sleep over `[0.75, 1.25]` as
thundering-herd insurance against a saturation-search rate step releasing every
waiter at once. The jitter was written without measurement, merged out of a
working tree, and v0.0.23 (`82e375c`) was cut on top.

Measured afterwards on macOS/mio, localhost, 4 threads, 1 KiB values, 20,000
req/s — cores per process, medians of interleaved reps, each arm built from its
release tag:

| connections | v0.0.22 no backoff | v0.0.23 backoff+jitter | v0.0.24 backoff only |
|---|---|---|---|
| 64 | — | **1.163** (1.157-1.205) | 0.664 (0.653-0.671) |
| 2048 | 3.488 (3.451-3.502) | 1.317 (1.302-1.335) | **0.659** (0.655-0.671) |

Ranges are disjoint at every comparison. At 64 connections the sleep is already
at `IDLE_SLEEP_MIN`, so the jitter is the only variable, and it costs 1.75x.
Making it one-sided did not recover it; only removing it did.

> **These figures were re-measured on 2026-09-18 and the first set should not be
> cited.** The original table — `0.54 / 1.10 / 0.51` at 64 and `3.50 / 1.27 /
> 0.51` at 2048 — was taken on a laptop that, unknown at the time, had up to
> three other sessions compiling concurrently; a load average of 16.5 was
> observed in the window, with most of it unattributed. Nothing recorded the
> conditions per run, so the original numbers could not be defended after the
> fact and were re-taken rather than argued for.
>
> What survived: the direction and the existence of the effect, and the
> `no backoff` and `backoff+jitter` columns, which reproduce closely (3.50 ->
> 3.488, 1.27 -> 1.317). What moved: `backoff only`, 0.51 -> 0.66 in both
> configs — a baseline shift in the box rather than a fault, since it moved the
> same way in each. The jitter penalty is therefore **1.75x at 64 connections
> and 2.00x at 2048**, not the 2.16x and 2.49x originally implied.
>
> The re-take records timestamp, load average and external CPU per run. The
> original recorded a number and nothing else, which is the whole reason this
> paragraph exists. A measurement that does not carry its conditions cannot be
> defended later, only repeated.

Rack A/B on Linux/io_uring (Valkey 9.0.1, 8 threads, pipeline 32, 1 s timeout),
`82f6168` vs `f56f1b9`, experiments `01a0b53b-0892` and `01a0b549-7052`:

| cell | metric | baseline | revert |
|---|---|---|---|
| 2048 closed-loop | req/s | 1,275,139 | 1,266,922 |
| 2048 closed-loop | cores/worker | 0.75 | 0.73 |
| 4096 closed-loop x3 | cores/worker | 1.00 | 1.00 |
| 4096 open-loop 300K | p50 | 1294 us | **835 us** |
| 4096 open-loop 300K | p99 | 2572 / 2490 us | **1753 / 1744 us** |
| 4096 open-loop 300K | cores/worker | 1.00 | 1.00 |

So the jitter costs ~35% p50 and ~30% p99 in the rate-limited cells on
io_uring, and nothing at all in closed-loop.

**Why closed-loop is null.** `idle_sleep` returns `IDLE_SLEEP_MIN` unchanged
when `rate == 0`, and `rate` is 0 whenever there is no ratelimiter. Both halves
of the idle path are then constant, so jitter or not, 512 connections per worker
poll the worker flat. The scaling in #153 does nothing in closed-loop.

**A trap worth naming**, because it produced a wrong "no effect anywhere"
conclusion mid-investigation: per-worker CPU reads 1.00 in *both* arms at 4096,
because 1.00 is the ceiling. A saturated metric cannot show an improvement —
the gain appears as more work done per unit of the pinned resource, which here
is latency. Checking CPU and then p999 (the noisiest percentile, and the one
where this effect is smallest) missed a change that is a third at p50.

## Open: what actually pins the core

Unresolved, and traceable to a constant this effort introduced.
`IDLE_WAKEUPS_PER_TOKEN = 64` means the fleet wakes 64 times per token granted
**by construction** — the ratio is the design, not an emergent property.

At the measured cell (4096 connections, 300,000 req/s, 8 workers):

```
idle_sleep       = 4096 / (64 * 300000) = 213 us
conns/worker     = 512
wakeups/s/worker = 512 / 213us          = 2,400,000
tokens/s/worker  = 300000 / 8           =    37,500
```

Closed-loop is worse: `rate == 0` gives the 100 us floor, so
`512 / 100us` = **5.12M wakeups/s per worker** — and those are the cells that
sat at 1.00 core with ~500K timeouts per 60 s and CPU mostly system time.
The regime that polls hardest is the one #153 never scaled.

Two distinct fixes, neither in v0.0.24:

- **Rate-limited:** lower `IDLE_WAKEUPS_PER_TOKEN`, or derive it. `K = 8` still
  oversamples eightfold at 300K wakeups/s/worker instead of 2.4M.
- **Closed-loop:** scaling is the wrong frame, since there is no token stream to
  track. A connection reaches the idle branch there only when `fire` fails on
  backpressure, and polling every 100 us per connection is a poor way to wait
  for a send slot. Wants a real wakeup, or at minimum a backoff.

**Condition to close:** a run at 4096 closed-loop where per-worker CPU is below
1.00. Experiment `01a0b549-7b11` (same cells, `threads = 16`) tests whether
halving connections per worker clears it without changing a line — if it does,
the per-connection poll is confirmed as the cost.

## What was learned

- Check `tcp_packet_latency` against reported latency before attributing any
  latency to the server under test. Now documented in `docs/guide.md`.
- A generator that cannot keep up reports latency, not an error. #151 exists so
  the next person does not need `ringline_diag` and a log scrape to find that
  out.
- Closed-loop and rate-limited are different code paths through `idle_sleep`,
  not two settings of one path. Reasoning about one from the other was wrong
  twice in this effort, in both directions.
- A metric pinned at its ceiling cannot show an improvement. Pick the metric
  the gain can move.
- Two quantities that both rise with connection count will correlate. That is
  not a mechanism.
