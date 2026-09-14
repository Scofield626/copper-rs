# Autoware Universe callback trace

`graph.json` and `costs.json` are derived from the measurement artifacts of:

> Atsushi Yano and Takuya Azumi, *Work-in-Progress: Response Time Analysis for
> Region-Based Scheduling of Autoware*, arXiv:2505.06780.

Repository: [`atsushi421/2024_RTSS_WiP_Evaluation`](https://github.com/atsushi421/2024_RTSS_WiP_Evaluation)
at commit `c6b9d453f313e81d9a5b89401a4435ca18ab8a99`. The upstream repository
carries no license file; the data is cited here as a measurement source and is
not claimed as this project's work.

## graph.json

- `nodes`: the 86 callbacks. `dag` names the timer-rooted sub-DAG, `period_ms`
  is set on the 11 roots, `deadline_ms` on the 18 sinks, `preds`/`succs` give
  the edges, `median_ms`/`p99_ms`/`samples` summarize the measured execution
  times.
- `edges`: the 83 producer/consumer pairs.
- `chains`: the 18 deadline chains, each a root, a sink, a deadline and the
  path between them.
- `alpha`: the scale factor that brings the total median utilization to a given
  number of cores, per utilization level.
- `latest_read`, `cut_edges`, `regions`, `rm_ranks`, `dm_ranks`, `e2e`:
  artifacts of the region-based design the data was measured for. This port
  does not use them.

## costs.json

`order` is the callback order `cost_index` indexes in `copperconfig.ron`.
`samples_ms` holds the first 1500 recorded execution times per callback, in
milliseconds and in recorded order; a callback with fewer samples wraps around
when the run outlives its sequence.
