# Viewer charts from `metriken-charts`

- **Opened:** 2026-11-05
- **Status:** CLOSED — shipped in #273 (`53242aa`), with the y-axis floor
  restored through `metriken-charts` 0.1.1 and a heatmap zoom regression fixed
  in 0.1.2. The Server and Client sections, which never drew data, are fixed on
  `viewer-section-sources`.

The viewer carried a copy of the rezolus chart modules from the initial commit
(`91b624c`) and its own plot spec types. `metriken-charts` 0.1.0 publishes the
same modules with a plot builder, and rezolus uses it (rezolus #1639, #1644).
The crate depends only on `include_dir` and `serde`, so the `metriken-core`
`links` constraint does not apply.

## Alternative not taken

Local branch `viewer-rezolus-charts` (`3af0400`) copies the rezolus frontend
into `src/viewer/assets/lib/`: 59 files, +18,098/-2,680. It was not used as the
base because the crate needs a Mithril wrapper and a card around each chart, not
`data.js`, `selection.js` or `viewer_core.js`.

## What the crate required

- `Chart` is a plain class. `ChartView` in `script.js` drives it from the Mithril
  lifecycle.
- The chart modules do not draw titles, so `chartCard` adds the
  `.chart-wrapper` and `.chart-header` markup that `charts.css` styles.
- `ChartsState` has no `setTimeInfo`. Its metadata requests are gone, and
  `interval` comes from the section JSON.
- `style.css` defines nine more properties; the `--compare-*` and `--topo-*`
  properties in `charts.css` style features the viewer does not draw.
- `opts.style` is set explicitly on every plot, and `chart.js` uses it ahead of
  `resolveStyle`.
- `Plot::histogram_latency` is not used: its 0 to 100 s range would change where
  slow values are drawn.

## Refuted: per-chart x-axes break the page

The crate uses `dataMin`/`dataMax` where the old modules used the recording's
time range, and shared zoom is by percentage, so charts with different extents
would zoom to different times. Measured over the Overview, Throughput, Latency,
Cache and Connections queries, the latest chart start and end were:

| Recording | Length | Step | Latest start | Latest end |
|---|---|---|---|---|
| Short run | 300 s | 1 s | 1.8 s | 0.1 s before the end |
| Long run | 14,400 s | 3.6 s | 6.3 s | 0.9 s before the end |

After a drag zoom on the 300 s recording, charts in a section differed by under
1 s. The Server and Client sections were measured once they drew: chart extents
there span 430 s to 431 s on the server recording and 424 s on the client's.
Reopen if a metric first appears mid-run: none of these recordings has one.

## Percentage y-axis

The old `line.js` gave percentage charts a minimum 1% span, so a flat 100% hit
rate drew 99% to 100%. `metriken-charts` 0.1.0 leaves `yAxis.min` and `max` null
and ECharts draws 40% to 160%. `format.range` clamps values and does not set the
axis, so the page could not restore this.

`metriken-charts` 0.1.1 adds `PlotBuilder::with_min_y_span` (iopsystems/metriken
#371). A fixed 1% in the crate was rejected: it changed every rezolus percentage
chart, and it drew a 0.05% error spike at 5% of the plot height. Hit Rate sets
1% and Error Rate 0.1%, each with `percentage_range`. The old span was set once
from all points; these also hold when zoomed.

## Checks

Headless Chromium against the 300 s recording, on this branch and on `main`:
all five sections draw, the heatmap toggle goes both ways, drag zoom syncs, and
double-click resets. RESET ZOOM raised `chartsState.resetZoom is not a function`
on `main` and works here. On the 14,400 s recording the Latency section took
250 s to load on both debug builds, run side by side.

Those checks missed two problems. On 0.1.1 a heatmap first drawn
as a heatmap took its narrowest zoom from its first cell, so after a drag charts
read `[0,100]` and `[28.1,56.6]` and RESET ZOOM did nothing. `main` was not
affected. iopsystems/metriken #373 fixed it in 0.1.2, and the same steps then
read `[28.1,56.6]` on both charts and `[0,100]` after the reset.

The second is gap handling, which `main` does not have, and neither does it
have percentile lines, so the regression from `main` is only the line charts'
stepping. On a recording longer than 6,000 intervals the step exceeds 1.5
intervals. `scatter.js` gaps its percentile lines by `chart.interval`, so they
broke at every point. With the step forced to 3 s on the 300 s recording, each of the five lines on a Latency chart
held 99 nulls among 100 points. Line charts use `seriesGapGeometry`, which judges
gaps against their median spacing, so they kept their points but drew unstepped,
where `main` uses `step: 'start'`. `script.js` now passes the query step through
`getStepOverride`, and on that run the percentile lines hold no nulls and line
charts stay stepped.

On the same recording with `--server` and `--client`, the Server and Client
sections drew no data on this branch or on `main`: see below. Not tested: the
`QueryExplorer` component, which has no section route.

## Server and Client sections show no data

Found while checking `CPU Usage %`, and not caused by this change. `api::routes`
queried only the benchmark file, so every query in those sections returned
`Metric not found`. `main` drew empty charts; #273 hid them, which left only the
group headings.

`viewer-section-sources` mounts `api::routes` once per recording, at `/api/v1`,
`/server/api/v1` and `/client/api/v1`, and each `View` names its root in `api`.
A recording's time range and interval come from that root, so `script.js` reads
`metadata` once per section, on the first plot that gets a response. `chartStep` is now set in `Main.view`: charts read
it at draw time, and a section preloading in the background has its own step.
With the step forced by `CHART_POINTS = 100`, it read 3.0 on Overview, 4.32 on
Server and 4.26 on Client, and kept those values after the preload and on
returning to each section.

Each section spans its own recording: 432 s and 426 s here, against 300 s for
the benchmark. `docs/reference.md` said these sections were aligned to the
benchmark timeline, which no version did.

The CPU query was also wrong, which the routing had hidden. `cpu_usage` counts
nanoseconds, and the query ended in `/ cpu_cores * 100`. Run directly against the
client recording, it returned about 9e9 where the host used about 9%. A
`Unit::Percentage` chart multiplies by 100 when it draws, so it needs a fraction,
and the query was 1e11 too high. It now ends in `/ cpu_cores / 1000000000`, and
reads a mean of 86.9% and a maximum of 93.3% on the server recording, and 9.05%
and 10.5% on the client's. It also used `state="user|system"`, which matches both
states only because `metriken-query` 0.37 splits `=` on `|`. That stopped in the
0.4x line, so the query uses `=~`. Every `cpu_usage` state in these recordings is
user or system, so the filter changes nothing today.
