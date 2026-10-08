# Recording cachecannon into `.dendro`

**Status:** OPEN. Plan opened 2026-10-08, nothing built. Decided 2026-10-08:
`cachecannon view` is bumped to read `.dendro`, not retired.

Related entries opened the same day: rezolus
`docs/journal/2026-10-08-6-0-release-readiness.md`, metriken
`docs/journal/2026-10-08-one-recording-stack.md` (one recording, exposition
and viewing stack for every metriken producer), systemslab
`docs/journal/2026-10-08-dendro-artifacts.md`. #176 proposes recording to an
archive in process; this entry updates its route and adds the stream.

## Today

- `metriken` 0.9, `metriken-exposition` 0.16, `metriken-query` 0.9
  (`Cargo.toml`).
- `--parquet` / `[admin] parquet`: `run_parquet_recorder`
  (`src/admin/mod.rs`) appends a msgpack snapshot to a temporary file on a
  `sleep(interval)` loop and converts it to parquet at exit. #176 lists what
  follows: nothing is readable until the run ends, a killed run leaves only
  the temporary file, the cadence drifts, and there are no acquisition
  windows.
- `[admin] listen` serves `/metrics` (Prometheus text) and `/metrics/binary`
  (a msgpack snapshot).
- `cachecannon view` reads parquet through `metriken-query` 0.9
  (`src/viewer/`).
- `docs/guide.md`, "One archive per run", records cachecannon beside two
  agents with
  `rezolus record --endpoint http://localhost:9090,source=cachecannon,role=loadgen ... -o run.rez`.

## What rezolus 6.0 changes

6.0's `record` writes `.dendro` when `-o` is not given. With `.dendro`
output, `record` probes `/metrics/binary`, takes cachecannon for a Rezolus
agent, asks it for `/metrics/stream`, gets a 404 and refuses the whole run at
startup. The guide's command works only because it names `-o run.rez`. Two
fixes, either sufficient:

- the guide names the Prometheus endpoint:
  `--endpoint http://localhost:9090/metrics,source=cachecannon,role=loadgen,protocol=prometheus`.
- rezolus scrapes `/metrics` from a source that serves neither the stream nor
  a Rezolus agent's `/status` (rezolus entry, "Endpoint detection").

Either way cachecannon's `/metrics` renders histograms as summaries
(`write_histogram_summary`, `src/admin/mod.rs`), so latency arrives as
quantile gauges, which the viewer cannot read as distributions, until step 3.

## Plan

1. **Guide.** Change the guide's example to the Prometheus form above, or keep
   `-o run.rez`, and say why. Remove the workaround when rezolus's detection
   fix ships.
2. **metriken 0.9 → 0.11**, `metriken-exposition` 0.16 → 0.21 (then the
   release carrying the stream route),
   `metriken-query` 0.9 → 0.34. `metriken-core` declares `links`, so a build
   holds one metriken-core version: metriken 0.9 is on core 0.2, and
   metriken-archive 0.3 needs metriken 0.11, on core 0.3.
3. **Serve `/metrics/stream`** through `metriken-exposition`'s stream route
   for a registry (metriken entry, piece 1). Once rezolus detects a source by
   its stream (rezolus entry, "Endpoint detection"), `rezolus record` records
   cachecannon as it records an agent: rows stamped by cachecannon at read
   time, native histograms, into the same `.dendro` as the hosts.
   `/metrics/binary` stays for `.rez` output and older recorders.
4. **Record in process with `metriken-recorder`**, recording cachecannon's own
   registry, for runs without a rezolus recorder (metriken entry, piece 2).
   This is #176's route 1 with a published crate in place of rezolus's
   unpublished `rez`. It addresses three of the problems
   #176 lists: the archive is readable while the run is going, it is complete
   after a crash, and the snapshot timer can be fixed-rate. Acquisition windows
   also need cachecannon's metrics to report one (metriken entry, piece 1).
   `--parquet` stays as an option for one release.
5. **`cachecannon view` reads `.dendro`**: open archives through
   `metriken-archive` 0.3's `ArchiveReader` (a `metriken_query::MetricsSource`),
   and later through `metriken-query` over `metriken-storage`, beside parquet.
   Later, `cachecannon view` mounts `metriken-viewer` in place of its own
   dashboards (metriken entry, piece 5), after rezolus 6.0.0.
6. **The template moves here.** cachecannon's dashboard template
   (`cachecannon.json`, today in rezolus's `crates/dashboard/templates/`) is
   owned by cachecannon and written into the archive: in the stream's
   handshake (step 3) and in its own source's metadata (step 4). The viewer
   loads it from the archive (metriken entry, piece 3).

Steps 3 to 6 need step 2 (metriken-query 0.34's default features pull in
metriken 0.11). Step 3 also waits for the metriken entry's path step 3 (the
stream route), step 4 for path step 4 (`metriken-recorder`), and step 6 for
path step 5 (templates); step 5's `.dendro` reading needs only step 2. Step 3
comes first among them: it is the piece every producer shares, and it puts
cachecannon in the same archive as the hosts.

## GO criteria

- A `rezolus record` run with an agent and cachecannon as endpoints, and no
  `-o`, writes one `.dendro` (`rezolus.dendro`) in which cachecannon's latency
  is a histogram, and `rezolus view rezolus.dendro` shows the cachecannon
  dashboard for the cachecannon recording, with no cachecannon template in the
  viewer.
- `cachecannon --dendro run.dendro` (or the chosen flag) leaves a readable
  archive after `kill -9` partway through a run.
- `cachecannon view run.dendro` shows the same dashboards as for the
  equivalent parquet.
