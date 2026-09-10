# Adaptive post-prefill quiesce

- **Opened:** 2026-09-10
- **Status:** OPEN — deferred, pre-build. A fixed `prefill_settle` window shipped
  instead (#137); this entry records the measured-quiesce alternative and the
  condition that would justify building it.
- **PRs:** #134 (`confirm_failures`), #137 (`prefill_settle`)

## Problem

Prefill leaves the **server's** page cache full of dirty pages. Linux does not
write those back promptly: `dirty_expire_centisecs` defaults to `3000` — thirty
seconds — after which the flusher thread evicts everything past expiry in one
burst.

Measured against a disk-backed cache server (1M × 56 KiB keys, guest-side
rezolus recording on the server):

```
t+s    write MB/s
 390      427.7     prefill
 400      148.3     prefill ends
 420        0.0     first measured window begins
 440      483.5  <- ~4.8 GB burst, 40s after prefill ended
 450+       0.0
```

The burst landed inside the first measured window and took its p99 to 151 ms.
The saturation search read that as the knee, fixed its bisection ceiling there,
and reported **4.8K req/s on hardware that sustains 55–60K**. The tell was that
three configurations — plaintext, `memcopy` and TLS — returned byte-identical
numbers; TLS cannot legitimately match plaintext to three significant figures,
so all three had bisected from the same burst and the search was measuring its
own bisection path rather than the server.

cachecannon cannot address this at the source. The dirty pages are on the server
and cachecannon is the client, so there is no `sync()` available to it. The
remedy has to be temporal.

## What shipped instead

`workload.prefill_settle` (#137): a fixed window, default 60s, added to warmup
only when `prefill` is true. Warmup already discards its samples and keeps
traffic flowing, so the burst lands in discarded load rather than in the first
measured window. 60s clears the 30s expiry with headroom for the flush itself.

`confirm_failures` (#134) is complementary and already landed. It rescues the
*verdict* — the retry fires after the burst has passed — but not the wasted
window or the misleading first sample.

## Why a fixed window may not be enough

The 60s default is derived from a documented kernel constant plus judgement
about flush duration. Two ways that reasoning fails:

- **Flush longer than the headroom.** The observed burst drained ~4.8 GB in
  about ten seconds because that server `fsync`s each SET, so most data was already
  on the device and only the residue expired. A server that buffers writes
  without `fsync`, or a slower device, could take substantially longer, and a
  fixed 60s would clip it.
- **A tuned or containerised host.** `dirty_expire_centisecs` is writable and
  cgroup writeback changes the accounting. A host set to 60s expiry defeats a
  60s settle exactly.

In both cases the failure is silent in the direction that matters: the run still
completes and reports a plausible knee.

## Design sketch

Replace the constant with a measurement. After prefill, drive traffic at a low
fixed rate and watch the latency distribution; begin the measured phase when it
has been stable across N consecutive short windows, subject to a ceiling.

The machinery already exists — rate limiting, per-window latency histograms and
step evaluation are what the saturation search is built from — so this is
plausibly a reuse of `SaturationSearchState`'s windowing rather than new
infrastructure.

Open questions, all of which need data rather than argument:

- What stability criterion? A p99 within X% across N windows is the obvious
  first try, but the burst is a *tail* event and a short window may not contain
  one, which would declare stability during a lull between bursts.
- What probe rate? Too low and the tail is unobservable; too high and the probe
  perturbs the thing it is waiting for.
- What ceiling, and what should happen at it — proceed with a warning, or fail?
  Proceeding silently reintroduces the original failure mode.

## Reopen condition

Build this when a fixed `prefill_settle` demonstrably fails: a run where the
first measured window is still contaminated after the configured settle, with
server-side evidence (block I/O or writeback counters) showing the burst arriving
after the window closed. Absent that, the fixed window is the proportionate fix
and the adaptive version is speculative.

A second, weaker trigger: if `prefill_settle` acquires per-target tuning in
practice — different values checked into different configs — that is evidence the
constant is doing a job a measurement should be doing.

## Evidence

- Burst timeline and `dirty_expire_centisecs = 3000` confirmed in-guest on the
  affected run; the same recording shows futex (`syscall{op="lock"}`) rate
  falling from ~2,000–4,200/s during prefill to a flat ~455/s throughout the read
  phase, which excludes lock contention as the cause and was the hypothesis this
  investigation started from.
- The corrected run, with prefill separated from measurement by a server-side
  `sync`, passed at the same 5000 req/s start rate that previously failed at
  p99 151 ms.
