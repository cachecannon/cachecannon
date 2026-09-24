# Generator saturation at high connection counts

**Status:** CLOSED 2026-09-23 — both causes found and fixed, one hypothesis
refuted and one diagnosis wrong. Closed-loop generator CPU went from a pinned
core per worker to 0.195; rate-limited cells fell 85-94%.

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
| `a09e991` | #166 | One task per worker polls the ratelimiter and funds queued claims, instead of every connection polling it. Decouples wakeup rate from connection count. 85-94% less generator CPU on every rate-limited cell, with latency improving rather than trading against it. |
| `8a1a7a5` | #167 | One deadline timer per connection, armed for its oldest in-flight request, instead of one io_uring timeout per recv. This was the closed-loop pin. |
| `4954208` | #170 | A dispatcher wait now completes when the dispatcher is done with the claim, funded or not. Follow-up to #166. |
| `460eea7` | #171 | Only a connection the limiter just refused parks on the dispatcher. Follow-up to #166. |

Released as v0.0.24 (`c1ed1db`), then v0.0.25 (`0bc3fcd`) and v0.0.26
(`050795f`).

v0.0.24's notes call #157 — which began enforcing `connection.request_timeout` —
"unrelated to this effort." It was the cause of the open item. See below.

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

**Why closed-loop is null.** (`idle_sleep` and `IDLE_WAKEUPS_PER_TOKEN` were
both removed by #166; what follows describes the code as it was.) `idle_sleep`
returned `IDLE_SLEEP_MIN` unchanged when `rate == 0`, and `rate` is 0 whenever
there is no ratelimiter. Both halves of the idle path were then constant, so
jitter or not, the scaling in #153 does nothing in closed-loop. The conclusion
drawn at the time — that 512 connections per worker therefore "poll the worker
flat" — went one step too far. The idle path being constant is all this null
result shows; it does not show that the poll is where the core went, and it was
not.

**A trap worth naming**, because it produced a wrong "no effect anywhere"
conclusion mid-investigation: per-worker CPU reads 1.00 in *both* arms at 4096,
because 1.00 is the ceiling. A saturated metric cannot show an improvement —
the gain appears as more work done per unit of the pinned resource, which here
is latency. Checking CPU and then p999 (the noisiest percentile, and the one
where this effect is smallest) missed a change that is a third at p50.

What was holding that ceiling is now known: the per-recv timeout timer, which
neither arm of this A/B touched (see "Closed-loop: not the poll at all"). So the
1.00 in both columns was not a coincidence of two saturating causes — it was one
cause, present identically in both arms, which the comparison could not see.

## Resolved: two causes, and the second diagnosis was wrong

This section previously read "Open: what actually pins the core" and attributed
the pin, in both regimes, to `IDLE_WAKEUPS_PER_TOKEN = 64` and the
per-connection idle poll it drove. That was right for the rate-limited cells. It
was wrong for closed-loop, the regime it called "worse" and where the pin
survived every change this entry made until #167.

### Rate-limited: the cost was real, the proposed fix was not

The arithmetic held. At 4096 connections and 300,000 req/s over 8 workers each
worker woke 512/213us = 2.4M times/s to grant 37,500 tokens/s. What the entry
got wrong was the remedy: it proposed lowering `K`, or deriving it.

No value of `K` is a free win, because under per-connection polling the wakeup
rate and the smoothness of the offered load are *the same quantity*. Measured at
2048 connections and 20,000 req/s, cutting the wakeup rate eightfold halved CPU
and took p99 from ~270us to 376-1868us, with schedule-slip p99 rising 100us ->
803us. Tuning `K` only picks a point on that line; the entry was proposing to
move along it and calling that a fix.

#166 made the two separable instead. One task per worker polls the limiter and
funds queued claims, so wakeups scale with the token rate and the cadence
governing smoothness is the dispatcher's alone. Cores per worker, same rack,
same cells, only the commit differing:

| cell | before | after |
|---|---|---|
| 4096 conns @ 300K req/s | 0.999 | 0.149 |
| 4096 @ 200K | 0.964 | 0.117 |
| 4096 @ 100K | 0.685 | 0.047 |
| 4096 @ 50K | 0.406 | 0.026 |
| 2048 @ 100K | 0.434 | 0.053 |

4096 @ 300K also went p50 827 -> 238us and p99 1720 -> 409us. CPU and latency
both improved, which is what distinguishes this from the tuning attempts above:
those traded one against the other.

#170 and #171 fixed two defects in the dispatcher, both found after it merged
and neither affecting measured results. #171 is the one worth knowing about:
every idle connection parked on the dispatcher, not only the ones the limiter
had refused, so it funded connections with nothing to send and warmup opened
with up to 131K requests released at once. All of it landed in warmup, which is
excluded from results, so the A/B above did not see it.

### Closed-loop: the request-timeout timer

The pin was `io_timeout_cancel`. #157 armed an io_uring timeout for every recv
and cancelled it when the reply arrived, and the kernel's cancel walks the
ring's pending-timeout list, which holds about one timer per connection.
Profiled on the rack at 4096 closed-loop connections, 8 threads, pipeline 32,
**the cancel was 69% of generator CPU** (63% at 2048).

#167 keeps one deadline timer per connection, armed for its oldest in-flight
request and left running when replies arrive. Cores per worker:

| cell | before | after |
|---|---|---|
| 2048 closed, 1 s timeout | 0.754 | 0.189 |
| 4096 closed, 1 s timeout | 0.999 | **0.195** |
| 4096 closed, timeout off | 0.194 | 0.156 |

**The ~480K timeouts per 60 s cited above as evidence of saturation were caused
by the timer cost, not by the server.** At 4096 the same cell went 1.02M req/s
with ~480K timeouts to 1.22M req/s with none, and p99 692ms -> 111ms. The
timer accounting consumed the CPU the workers needed to read replies, so replies
fell past the 1 s deadline and were counted as timeouts. This entry opened on a
generator's latency being read as the target's; the timeout counter was the same
error in a different metric.

### Why the wrong diagnosis was not obviously wrong

The closed-loop arithmetic in the old section was correct and predicted the
right magnitude: `512 / 100us` = 5.12M wakeups/s/worker does pin a core, and
closed-loop CPU did scale with connections per worker. But so does the
pending-timeout list, by construction — it is about one entry per connection.
Both mechanisms are monotone in connections-per-worker and both predict a pin at
4096 on 8 workers.

So the close condition this entry wrote for itself could not have settled
anything. It proposed experiment `01a0b549-7b11`, the same cells at
`threads = 16`, reasoning that if halving connections per worker cleared the
pin, "the per-connection poll is confirmed as the cost." It would not have been:
halving connections per worker halves the timer list too, so both hypotheses
predict the same result and the test distinguishes nothing. **This is the second
time in this one entry that two monotone functions of connection count were
taken for a mechanism** — the first is the refuted recv-ring hypothesis above,
where the trap is named explicitly.

A profile settled it. `f56f1b9` and `82f6168` differ by the jitter alone, and
the closed-loop cells were flat between them to three decimals, which ruled out
the idle path without identifying what replaced it. The profile showed
`io_timeout_cancel` at 69%, and a `timeout off` arm reproduced the difference in
one run.

### Close condition, met

The entry asked for "a run at 4096 closed-loop where per-worker CPU is below
1.00." It is 0.195 with the timeout on, at 1.22M req/s with zero timeouts. The
timeout-off arm at 0.156 bounds what remains: the per-connection timer now costs
about 0.04 cores per worker at 4096, roughly a fifth of the loop's total, for
correct timeout enforcement.

## What was learned

- Check `tcp_packet_latency` against reported latency before attributing any
  latency to the server under test. Now documented in `docs/guide.md`.
- A generator that cannot keep up reports latency, not an error. #151 exists so
  the next person does not need `ringline_diag` and a log scrape to find that
  out.
- Closed-loop and rate-limited are different code paths — since #166, different
  mechanisms entirely — not two settings of one path. Reasoning about one from
  the other was wrong three times in this effort, in both directions, the last
  time for a week.
- A metric pinned at its ceiling cannot show an improvement. Pick the metric
  the gain can move.
- Two quantities that both rise with connection count will correlate. That is
  not a mechanism. This was recorded here after it cost a day and then happened
  again in the same entry, so recording it as a caution did not help. Stated as
  a procedure it might: before running a test, write down what result would rule
  the hypothesis out. The `threads = 16` test had no such result, and that was
  checkable before it ran.
- When a benchmark enforces a limit, price the enforcement. #157's timeout cost
  69% of generator CPU and then reported ~480K timeouts per 60 s that it had
  caused itself. Measure any new enforcement path against the same path
  disabled, in the cell with the most connections, before believing what it
  reports.
- A null result bounds only what it measured. "The jitter is not the cost" was
  sound; "the poll is the cost" did not follow from it and was wrong. Three A/Bs
  did not locate the cost and one profile did.
- If CPU drops and latency drops together, the constraint is gone. If one
  improves at the other's expense, it has only moved. Every attempt to tune
  `IDLE_WAKEUPS_PER_TOKEN` produced the second result; replacing the
  per-connection design in #166 produced the first.
