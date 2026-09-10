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
| 2026-09-10 | [Adaptive post-prefill quiesce](2026-09-10-adaptive-prefill-quiesce.md) | OPEN — deferred, pre-build. A fixed `prefill_settle` window shipped instead (#137); records the measured-quiesce alternative and what would justify building it |
| 2026-09-10 | [Cache trace replay](2026-09-10-trace-replay.md) | OPEN — scoping, pre-build. Replay a real access sequence instead of a stationary synthetic distribution; records why key-sequence replay is the primary mode and why a miss-ratio curve may make it unnecessary |
