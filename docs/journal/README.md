# Engineering Journal

An in-repo, code-grounded record of non-trivial efforts: what we set out to do,
the GO/NO-GO decision, what happened, and what was learned. Entries live here so
they are versioned with the code, greppable, and readable by the next engineer
(or agent) without leaving the tree.

Conventions:

- One markdown file per effort, named `YYYY-MM-DD-slug.md` (open date).
- Ground every claim in source: real commit SHAs, file paths, measured numbers.
  Never invent figures. If a detail isn't in the source, say so or omit it.
- NO-GOs and dead-ends are first-class entries — record the mechanism and the
  condition under which to reopen.
- Issues/PRs are the task layer; this journal is the narrative/decision layer.
  Link them together; don't let a PR be the only record of a non-trivial effort.

## Entries

| Date | Effort | Status |
|------|--------|--------|
| 2026-10-19 | [Following cluster topology changes mid-run](2026-10-19-topology-following.md) | OPEN — design, pre-build. Both cluster modes read the node list once at startup (#239); proposes a shared topology with stable node ids, a watcher thread, and three load models: per-node (today's behaviour, the default), routed (draw the key, then route it) and mirrored (every node gets the whole keyspace), where routed open and closed loop need different implementations |
| 2026-10-12 | [KV-cache prefix replay (`[workload.kv_replay]`)](2026-10-12-kv-replay.md) | OPEN — built and tested on mio locally against an in-process RESP server; not yet run against a real server, on io_uring, or at scale |
| 2026-10-12 | [Reading received values (`workload.read_values`)](2026-10-12-read-values.md) | OPEN — built and tested on mio locally; client CPU per request with the option on vs off not yet measured (needs Linux, io_uring) |
| 2026-10-08 | [Recording cachecannon into `.dendro`](2026-10-08-dendro-recording.md) | OPEN — steps 2 and 3 built 2026-10-11 (metriken 0.11 / exposition 0.22 / query 0.36; `/metrics/stream` through metriken's stream route), against ringline main `dfff2e5` through `[patch.crates-io]`. Remaining: step 1 (guide), in-process recording with `metriken-recorder` (#176 with a published crate), `cachecannon view` reads `.dendro`, and the dashboard template moves here and travels in the archive |
| 2026-09-18 | [Generator saturation at high connection counts](2026-09-18-generator-saturation.md) | CLOSED 2026-09-23 — two causes: per-connection ratelimiter polling (#166) and a per-recv io_uring timeout whose cancel was 69% of generator CPU (#167). 4096 closed-loop went 0.999 -> 0.195 cores/worker; rate-limited cells fell 85-94%. recv-ring starvation refuted (`parks=0`); the closed-loop diagnosis this entry first recorded was wrong |
| 2026-09-10 | [Adaptive post-prefill quiesce](2026-09-10-adaptive-prefill-quiesce.md) | OPEN — deferred, pre-build. A fixed `prefill_settle` window shipped instead (#137); records the measured-quiesce alternative and what would justify building it |
| 2026-09-10 | [Cache trace replay](2026-09-10-trace-replay.md) | OPEN — key-sequence replay built 2026-10-22 as `[workload.trace_replay]` on cachesim's `cachetrace` Parquet instead of the proposed container; tested on mio against a fake server, not yet against a real server, on io_uring, or at scale |
