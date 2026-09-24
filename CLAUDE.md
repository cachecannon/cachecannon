# cachecannon

A load generator. That framing is the point of most of what follows: when a
number looks bad, the first question is whether this process produced it.

## Before trusting a measurement

**Check the generator is not the bottleneck.** A load generator that cannot keep
up does not error — it reports latency, and time spent waiting to read a
response that already arrived is indistinguishable from server time in the
benchmark's own numbers. `docs/guide.md` → "Is the generator the bottleneck?"
has the procedure; the short version is to compare client-side
`tcp_packet_latency` (socket-readable → userspace-read) against reported
latency, and to treat per-worker CPU near 1.00 core as disqualifying.

Since #151 the generator's own runtime counters reach `/metrics` and the parquet
snapshot: `ringline/pool{op="recv_parked"}`, `ringline/bytes{op="fallback_received"}`,
`ringline/ring{op="sqe_submit_failures"}`. Non-zero means the client is the
story.

**A metric pinned at its ceiling cannot show an improvement.** Per-worker CPU
reading 1.00 in both arms of an A/B means saturated, not unchanged — the gain,
if any, shows up as more work done per unit of the pinned resource. Pick a
metric the change can move.

## Two regimes, not one setting

Whether a run has a ratelimiter decides which waiting code a connection uses:

```rust
let ratelimiter = if initial_rate > 0 || config.workload.saturation_search.is_some() {
```

- **Rate-limited** (`workload.rate_limit`, *or* `[workload.saturation_search]` —
  either creates a ratelimiter): one `run_dispatcher` task per worker polls the
  limiter and funds queued claims. A connection the limiter has just refused
  parks on `TokenDispatcher::acquire`; wakeups scale with the token rate, not
  with the connection count.
- **Closed-loop** (neither key): no limiter, so no dispatcher. A connection
  reaches the idle path only when `fire` fails on backpressure, and polls at the
  `IDLE_SLEEP_MIN` constant when it does.

These are different code paths. Do not reason about one from the other — that
error was made repeatedly during the effort recorded in
`docs/journal/2026-09-18-generator-saturation.md`, and the closed-loop cause was
misdiagnosed for a week because a rate-limited mechanism was assumed to apply.
Classifying a spec by grepping `rate_limit` alone misses the saturation-search
case.

Two traps from that effort, both live:

- **Only a refused connection may park on the dispatcher.** Parking on any idle
  state (#171) made the dispatcher fund connections that had nothing to send, so
  the limiter was drained for tokens nothing spent and each grant overwrote the
  last. Any other idle state uses `IDLE_SLEEP_MIN`.
- **`acquire` is deliberately untimed.** Wrapping it in `ringline::timeout`
  reintroduces a per-acquire timer, which is the cost the dispatcher exists to
  remove. Liveness comes from the dispatcher's own `drain`.

## Price any per-request timer

`connection.request_timeout` is enforced by **one deadline timer per
connection**, armed for its oldest in-flight request and left running when
replies arrive (`InFlight`, `recv_with_timeout` in `src/worker.rs`). The obvious
implementation — arm a timeout per recv, cancel it on reply — was measured at
**69% of generator CPU** at 4096 closed-loop connections, because the kernel's
`io_timeout_cancel` walks the ring's pending-timeout list, which holds about one
timer per connection. At that cost the workers could not read replies fast
enough to meet the 1 s deadline, so the run also reported ~480K timeouts per
60 s that the timer had caused.

Measure any new per-request kernel operation against the same path disabled, at
the highest connection count, before trusting the counters it drives.

## ringline

**Per-worker pools default to 256 and are sized independently of the workload.**
Three matter: `standalone_task_capacity` and `timer_slots` are scaled by
connections-per-worker in `src/runner.rs` (one was silently capping runs at 2040
connections, the other panicking workers at 4096); `recv_buffer.ring_size` is
deliberately left at ringline's default and exposed as `general.recv_ring_size`
instead. Adding a fourth pool user means checking whether it needs the same
treatment.

**Recv buffers are not held per connection.** A buffer is held only between a
completion and the client draining it, so ring depth divided by connection count
is not a meaningful ratio. Measured: 10,000 connections, 56 KiB responses, four
buffers per response against a 256-buffer ring — `parks=0` on every worker.

**Linking ringline registers its metrics globally** via metriken, whether or not
this crate mentions them. They arrive as `ShardedCounterGroup`, which surfaces as
`Value::CounterGroup` — a variant both exposition sites once dropped through a
catch-all. Metric names from foreign crates are not necessarily legal Prometheus
names (`ringline/connections/active`), which is why `src/admin/mod.rs` sanitizes
on the way out and a test scans the rendered body.

## Conventions

- `docs/journal/` is the narrative layer: one entry per non-trivial effort,
  `YYYY-MM-DD-slug.md`, every claim grounded in a real SHA, path or measured
  number. **Dead ends and refuted hypotheses are first-class entries** — record
  the mechanism and the condition to reopen. See its README.
- Code changes carry a CHANGELOG entry under `[Unreleased]`; docs-only commits
  do not.
- Released CHANGELOG sections are not rewritten. A correction to a shipped
  change goes under `[Unreleased]` describing both.
