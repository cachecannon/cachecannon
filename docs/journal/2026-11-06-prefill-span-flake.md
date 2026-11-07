# A paced prefill test's upper bound under host stalls

- **Opened:** 2026-11-06
- **Status:** OPEN — fix on `fix-prefill-span-flake`, not yet run on
  `macos-latest`. The disjoint-worker tests now bound the time between SETs
  with gaps over 25 ms left out, the number of those gaps, and the time they
  take. On fresh runs at 160 CPU spinners on one host, after every bound was
  set, it failed 0 of 40 healthy runs against 15 for the old bound, and 30 of
  32 mutant runs. A pacer that stops for under 1.5 s in total, in fewer than
  15 stops, passes wherever the stops fall. No product change.

`workers_whose_endpoints_drained_leave_the_rate_to_the_others` and its memcache
twin asserted that 8000 keys at 4000 per second take between 1.6 and 2.4 s. In
the last 60 CI runs, counting attempts that were rerun, they failed five times,
all on `Test (macos-latest)`:

| SHA | Test | Span |
|-----|------|------|
| `1f557567` | RESP | 2.400 s |
| `fe314f81` | memcache, attempt 2 | 2.406 s |
| `cb741e64` | memcache | 2.576 s |
| `318aaf12` | RESP | 2.592 s |
| `82a79d08` | RESP | 2.631 s |

In the same 60 runs, two other wall-clock tests failed on `Test (macos-latest)`
in attempts that were later rerun:
`prefill_is_paced_to_the_limit_over_memcache` at 1.505 s against 1.5 s
(`c71cb2e6`), and `rate_limit_paces_rows` (`3235d408`). This effort did not
change them. `gh run list` reports only the latest attempt, so it hides these.

## Method

All measurements are from one 16-core macOS host, debug build, running the
tests' shape (four endpoints, 16 threads, 16 connections, pipeline 1) unless
stated. The stress arms ran `yes` spinners beside the test, with arms
interleaved. A temporary test printed each run's span, the spans of ten equal
runs of keys (deciles) and the gaps between consecutive SETs.

The positive controls were mutants:

- `split`: `prefill_ratelimiter` split evenly per worker in `src/runner.rs`.
- `pop`: the token taken before the key is popped in `src/worker.rs`, the
  regression named in the memcache test's header.
- `sleep30`, `sleep50`: a refused connection sleeps 30 or 50 ms.
- `tail30`, `tail100`, `tail1000`: after 6000 tokens, a refused connection
  sleeps 30, 100 or 1000 ms.
- `tail:7600:300`, `tail:7800:600`, `tail:7900:900`: after 7600, 7800 or 7900
  tokens, a refused connection sleeps 300, 600 or 900 ms.

## Results

Runs failing the old bound, `1.6 s < span < 2.4 s`, on healthy code:

| Stress | Runs | Fails | Note |
|--------|------|-------|------|
| None (1-minute load 21 to 72) | 60 | 0 | RESP only, max 2.125 s |
| 48 spinners | 12 | 0 | RESP only, max 2.293 s |
| 128 spinners | 60 | 40 | Max 3.469 s |
| 128 spinners, pipeline 1 | 20 | 11 | Same session as the next row |
| 128 spinners, pipeline 8 | 60 | 16 | |
| 128 spinners | 60 | 16 | |
| 160 spinners | 60 | 22 | Max 4.492 s |
| 160 spinners | 40 | 15 | |

The check that shipped, `assert_paced_for_two_seconds` in `tests/common/mod.rs`,
requires `1.6 s < paced < 2.7 s`, fewer than 15 gaps over 25 ms, and under 1.5 s
in those gaps. `paced` is the time in gaps of at most 25 ms. On the last
160-spinner session, run after all three bounds were set:

| Arm | Runs | Old bound fails | New check fails |
|-----|------|-----------------|-----------------|
| Healthy | 40 | 15 | 0 |
| `pop` | 4 | 4 | 4 |
| `split` | 4 | 4 | 4 |
| `sleep30` | 4 | 4 | 4 |
| `tail30` | 4 | 4 | 4 |
| `tail100` | 4 | 4 | 4 |
| `tail:7600:300` | 4 | 4 | 3 |
| `tail:7800:600` | 4 | 4 | 4 |
| `tail:7900:900` | 4 | 4 | 3 |

Healthy runs there spent at most 0.688 s in gaps over 25 ms and had at most 9 of
them. The two mutant runs that passed spent 0.599 s and 1.265 s in such gaps.

Where each bound came from:

- 25 ms was set before the last 128-spinner session.
- 2.7 s was chosen after that session, where healthy `paced` reached 2.396 s
  and `split` started at 2.979 s.
- Fewer than 15 gaps came from a 40-run sweep made during review of this change,
  not recorded here, with healthy runs at 0 to 5 gaps and its lowest mutant at
  21.
- 1.5 s was chosen from the first 160-spinner session, where healthy runs spent
  at most 0.967 s in gaps over 25 ms. On that session the check with all three
  bounds failed 4 of 60 healthy runs and 41 of 42 runs of the first seven
  mutants, so those figures are not an independent test.

## Mechanism

This is inferred from where the excess falls; per-connection state and lost
tokens were not recorded. In the first 128-spinner session, 89.6% of the time
over 0.2 s per decile fell in the last three deciles, in single gaps of up to
337 ms. Near the end of a prefill few connections have keys left, so a stopped
thread stops most of the prefill. `WorkerLimiter::paced` holds 10 ms of the
rate, so a longer stop is not made up afterwards.

At 160 spinners some healthy runs spent 3.1 to 3.5 s in gaps of at most 25 ms,
alongside 8 to 22 longer gaps, so an overloaded host slows the paced time as
well as stopping it. The bounds tried here do not separate that from a mild
pacing regression.

## Ruled out

- **The remaining connections cannot use the whole rate.** With no spinners the
  median decile is 0.200 s and 8 of 60 runs have a late decile over 0.21 s, at
  most 0.259 s. Every rate-split run in the first three stress sessions has a
  late decile of at least 0.843 s. Reopen if a run on an idle host stretches
  only its late deciles.
- **A timer in the product.** Gaps over 50 ms, up to 337 ms, become steadily
  rarer with length and have no peak at one value. No sleep on the normal
  prefill path is that long; the 100 ms sleeps in `src/worker.rs` run on
  connect and disconnect errors. Wakeup paths were not exercised. Reopen if gaps
  cluster at one value.
- **Pipeline depth as the cause.** In one interleaved session pipeline 1 failed
  11 of 20 and pipeline 8 failed 16 of 60, with largest gaps of 252 and 280 ms.
  Pipeline 8 still fails, so depth is not the cause, though it may lower the
  failure rate.
- **The second-longest decile under 0.4 s.** It assumed one stall lengthens one
  decile, and failed 14 of 60 healthy runs because several late deciles stretch
  together.
- **Shortest deciles, as `decile_spans` is used elsewhere.** A rate-split run
  keeps its early deciles at 0.2 s, so these pass.
- **`paced < 2.7 s` with no other bound.** A pacer that sends bursts and then
  waits spends its time in long gaps. In one run before the sweeps, `tail1000`
  took 40.6 s with `paced` at 1.604 s and passed.
- **A 5 s cap on the span in place of the 1.5 s cap.** Stops near the end pass
  it: on an idle host `tail:7800:600` took 4.36 s with 4 long gaps in the
  real test, and `tail:7600:300` 4.32 s with 8.
- **Fewer threads.** On an idle host `split` took 2.08 and 2.13 s at 4 threads,
  2.50 to 4.83 s at 8, 3.98 to 6.28 s at 12 and 4.47 to 8.00 s at 16, two runs
  per protocol at 8 to 16 threads. The regression shows only with many workers.

## Not known

- Whether lost tokens contribute. A 100 ms hold took runs over 2.1 s from 5 of
  12 to 2 of 12 at 48 spinners, which 12 runs cannot separate from chance.
  `WorkerLimiter::dropped()` would measure it and was not read.
- Whether the CI failures have this shape. CI logs record only the span.
- Why the failures appear on `macos-latest` and not on Linux. Linux 6.1 and
  later runs ringline's io_uring backend and macOS the mio one, and the runners'
  core counts were not checked.
