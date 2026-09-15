# cu_aur — the Autoware Universe replica

A trace-derived copy of Autoware Universe's callback graph as a Copper application:
86 callbacks in 11 timer-rooted sub-DAGs, each replaying the execution times measured
on the real system. It is the workload the profile-guided scheduling workflow
(`doc/pgo-workflow.md`) is exercised on: a graph large enough that where the work runs
decides whether its 18 chains meet their deadlines.

The measurements come from Yano & Azumi, arXiv:2505.06780; see `data/README.md` for the
citation and the shape of the two data files.

## How the graph maps onto Copper

- **One task per callback.** `gen_config.py` writes `copperconfig.ron` from
  `data/graph.json`: 86 tasks, 83 connections, task ids equal to callback ids. The
  message is one small payload, `AurMsg { seq, root_ns }`.
- **Roots** (`tasks::AurRoot`, 11 of them) are `CuSrcTask`s paced by the CopperList
  grid. The application runs at 200Hz, so one CopperList is 5ms, and a root fires on the
  first CopperList of each `period_ms` window: `(cl_id * 5) % period_ms < 5`. It stamps
  the CopperList's time of validity and publishes its firing count as `seq`; on the
  other CopperLists it clears its output. A period that is not a multiple of the grid
  alternates window lengths — 33ms gives 7 and 6 CopperList gaps — and does it the same
  way in every run.
- **Callbacks** (`tasks::AurCallback`, `tasks::AurJoin`) are `kind: stateless_task`:
  `process` takes `&self`, so the planner may run different CopperLists' invocations on
  different workers with nothing ordering them. A callback runs when its input carries a
  payload and forwards that input's stamps; a two-input callback (8, all in perception)
  is triggered by its first connection and forwards its stamps, whatever the second
  input carries that CopperList.
- **Sinks** (`tasks::AurSink`, 18) end the dataset's deadline chains and produce no
  output.
- **Cost replay.** Each task carries a `cost_index` into `data/costs.json` and replays
  the sample its incoming `seq` selects, wrapping when a run outlives the sequence.
  Indexing on `seq` rather than on a counter is what keeps the callbacks stateless: the
  same CopperList always costs the same, whichever worker runs it. The sequences are
  scaled by `--alpha` and converted to crunch units with the host's `k_ns_per_unit`
  before the application is built, so a firing only indexes an array.
- **Chains** are measured per CopperList, as the whole workflow measures them: the
  sink's process end minus the root's time of validity, for every CopperList in which
  the sink ran.

Roots fire on the CopperList grid, so a run's content is a function of its CopperList
ids: nothing branches on the clock and nothing is random. A serial run and a multicore
run of the same graph therefore process identical inputs and record identical payloads,
and only their timing differs. `root_ns` rides in the payload as timing data.

The dataset's three `latest_read` edges (the side and top LiDAR concatenate sinks into
perception's root, prediction's sink into planning's root, planning's validator into
control's root) were a stamp-only mechanism of the region-based design they were
measured for. They are not graph edges here, so the 11 sub-DAGs are independent.

## Calibration

The replay charges a target duration as `crunch(t_ns / k)` trial divisions. `k` is
host-specific, so the workload has to be measured before it can be replayed:

```bash
just calibrate        # writes calibration.ron
just replay-check     # every callback within 5% of its targets, or 1µs
```

`calibrate` fits one cost per crunch unit over five unit counts spanning the replayed
range and refuses to write a fit any point misses by more than 5%. `replay-check` runs
the graph and compares each callback's recorded `process_time` against the samples it
was handed. The absolute allowance covers the callbacks of a few microseconds, where
Copper's own per-step bookkeeping is a sizeable part of what `process_time` measures.

## Scale

`--alpha` scales every recorded execution time. `data/graph.json` carries the factors
that put the total median utilization at a given number of cores; the binary's default,
`0.457811`, is the 2.2-core point.

A sub-DAG's whole chain runs inside one CopperList, so a serial run holds every root's
period only while the worst coincidence of the 11 sub-DAGs still fits inside the
shortest of them. The sub-DAGs sum to 422ms of median work and the shortest period is
20ms, which is why the recipes below record at `0.04`: `just run 5 0.457811` is the
2.2-core point, and its roots then deliver at a fraction of their rate under the serial
executor.

## Workflow

```bash
just gen                   # regenerate copperconfig.ron and pgo.ron from data/
just calibrate             # measure this host's crunch unit
just profile 10            # record a serial run, extract pgo/profile.ron
just propose               # pgo/candidates/plan-*.ron + predictions.ron
just import pgo/candidates/plan-1.ron     # -> copperconfig-pgo.ron
just measure 10            # run that plan on the lane executor, extract its profile
```

Because the roots fire on the grid, a source's delivered rate measures whether the
executor held the grid. The serial executor does not at this scale — a CopperList
carrying a whole sub-DAG chain overruns the 5ms slot — and the lane executor is what
restores it.

`pgo.ron` is the scheduling contract: the 18 chains with their deadlines, the 11 roots
with their periods, the CPUs a candidate may use, and how candidates are ranked. The
runtime macro reads its config at compile time, so running a candidate is a rebuild:
the `pgo-plan` feature points the application and the logreader at
`copperconfig-pgo.ron`, and `parallel-rt` provides the lane executor a multicore plan
needs.

## Tests

```bash
cargo test -p cu-aur --release
```

`--release` matters: the replay-accuracy bar is a property of the optimized build, and a
debug run checks only that every callback fired once per firing of its root.

## Binaries

| binary | what it does |
|---|---|
| `cu-aur` | records `--seconds` of the graph into `--log-base` |
| `calibrate` | measures this host's crunch unit into `calibration.ron` |
| `replay-check` | runs the graph and checks each callback against its targets |
| `cu-aur-logreader` | the export CLI: `fsck`, `extract-copperlists`, `log-stats`, `pgo-profile` |
