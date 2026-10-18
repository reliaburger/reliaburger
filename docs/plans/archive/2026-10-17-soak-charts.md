# Soak charts

Part 1 of [#979](https://github.com/reliaburger/reliaburger/issues/979): show
how a soak went as pictures, not just as tables.

## Why

A soak's evidence directory holds the whole run: every heavy and settle
snapshot carries each node's Bun RSS, open fds and CPU ticks, every job-soak
snapshot carries the controller's cumulative successes, and `events.jsonl`
has every fault, upgrade walk and special. The record only quotes a few
first/last/max rows from that, and the evidence directory is never committed,
so the time series is lost once the host's `/var/tmp` is cleaned. The 0.2.0
jobs final shows why that matters: the moment the job controller ran out of
its run budget (#989) is obvious on a chart and invisible in a table.

## What

- `scripts/release/soak_charts.py`, standard library only, collects the series
  and draws up to four SVG charts: Bun RSS per node with the 512 MiB (node 1,
  which serves the API) and 384 MiB targets; open fds per node; CPU as a
  percentage of one core, measured between samples of the same Bun process;
  and accepted jobs a second in 10-minute windows against the target rate,
  when the soak ran jobs. Every chart marks faults, upgrade walks, specials
  and recorded failures along the time axis.
- Colours are CSS custom properties inside each SVG, with a dark-scheme
  override and the light value as a fallback for renderers without custom
  properties. The palette is the first three slots of the validated
  categorical palette; nodes beyond three share a neutral colour.
- Output is deterministic, so re-rendering the same evidence gives the same
  bytes.
- `sustained_check.py render` writes the charts to `<record>-charts/` beside
  the record and adds a Charts section linking them. `sustained_check.py
  render-charts EVIDENCE DIRECTORY` rebuilds them from a kept evidence
  directory and prints the section.
- The 0.2.0 records get their charts from the evidence directories kept on
  the release host: V02 fast and final, jobs fast and jobs final.

## Not in this part

- The landing page's "How a release is tested" section (part 3, with #799)
  embeds the 0.2.0 charts once it has merged.
- Interactive charts. The SVGs are shown as images in Markdown, where scripts
  and hover don't run; each line and mark carries a `<title>` for when the SVG
  is inlined.

## Tests

`scripts/release/test_soak_charts.py` (`make test-ci-scripts`): series come
from the snapshots that carry inventories; CPU only between samples of the same
process; job rates from cumulative successes; marks from events; each chart
parses as SVG, uses the CSS variables and draws its targets and marks; output
is byte-for-byte deterministic; an app-only soak has no jobs chart; a single
sample draws nothing; `render` writes and links the charts; `render-charts`
rebuilds them.
