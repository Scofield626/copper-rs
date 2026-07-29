# Pluggable execution-plan scheduling — design (v0)

`compute_runtime_plan()` turns the task graph into the fixed step sequence the
`#[copper_runtime]` macro bakes into the generated loop. Today the ordering
heuristic is hard-coded (a BFS from the sources) and carries a
`TODO(gbin): Make that heuristic pluggable`. This design makes the heuristic a
config-selected policy, and lays the groundwork for a profile-guided policy.

## The core invariant

Any topological order of the (per-mission, bridge-expanded) task graph is a
*correct* plan: a step only needs every producer of its inputs to run earlier
in the same iteration. Everything else — copperlist slot assignment, input
wiring, validation — is mechanical and identical for every order.

So a scheduling policy answers exactly one question: **which topological order
do we emit?** That keeps every policy small and safe: a policy cannot corrupt
message wiring, it can only pick a better or worse order.

## The split

`compute_runtime_plan(graph, policy)` becomes two phases:

| phase | function | role |
|---|---|---|
| order | `topo_bfs_order(graph)` (one per policy) | pick the step order: `Vec<NodeId>` |
| build | `build_plan_from_order(graph, &order)` | assign copperlist slots in order, wire inputs, validate |

The build phase rejects an order where an input's producer does not appear
earlier — a buggy policy fails the build with a clear error instead of
generating a broken runtime.

## Config surface

Two independent fields in `config.rs`: *which algorithm* and *what it
measured*. A variant names an algorithm and nothing else; measurement is a
separate struct, because the same numbers feed more than one consumer.

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanPolicy {
    /// The historical source-BFS order. The default. Ignores the profile.
    #[default]
    TopoBfs,
    /// Longest-remaining-critical-path first. Needs a profile.
    CriticalPathFirst,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanProfile {
    pub task_duration_ns: BTreeMap<String, u64>,
}
```

RON:

```ron
runtime: (
    plan_policy: TopoBfs,
    plan_profile: (task_duration_ns: {"cam": 1200, "detect": 8400}),
),
```

**Why the profile is not a variant field.** Folding the measurement into
`CriticalPathFirst(task_duration_ns: ...)` names the *input* where a variant
should name the *algorithm*, and it forces every future profile-guided
algorithm to repeat the same field. Keeping them apart means adding
`LongestTaskFirst` or `MinSlack` costs one unit variant, switching policy
needs no re-measurement, and `parallel-rt` placement can read the same
`PlanProfile` without going through a policy at all.

Both fields are optional. `plan_policy` defaults to `TopoBfs`, whose output is
byte-identical to the historical planner. A policy with `needs_profile()` and
an empty `plan_profile` fails the build with a message pointing at
`schedule-profile` — a config mistake, not a silent fallback to another order.

The field is optional and defaults to `TopoBfs`; v0 output is byte-identical
to the current planner (same order, same copperlist indices).

**Why the RON config and not a macro attribute:** the unified log embeds the
config. Offline tools (`cu29_export` logstats) recompute the plan from that
embedded config to map copperlist slots back to tasks. If the policy lived
outside the config, an offline reader could reconstruct the wrong slot layout.
Policy-in-config keeps one source of truth for everything that derives from
the plan.

## Profile-guided ordering (v1)

This is a chicken-egg problem only in the way classic PGO is: the first build
cannot have a profile. The loop that resolves it:

```
build (TopoBfs) → run robot or resim → export profile
      → paste plan_profile into RON, set plan_policy → rebuild
      → compare logstats → repeat
```

- **No new instrumentation.** Every `CuMsg` already records the
  before/after `process()` window in its metadata; the unified log has the
  per-task durations for every recorded cycle.
- **Exporter.** `cu29_export <log> schedule-profile [--config copperconfig.ron]
  [--mission M] [--stat mean|p99|max]` reads the log and writes
  `schedule_profile.ron`: the exact RON value of the config's
  `runtime.plan_profile` field. It writes a profile, never a policy — picking
  the algorithm stays the user's decision.
- **Policy.** `CriticalPathFirst` is critical-path-first list scheduling:
  among the ready nodes, always order the one with the longest remaining
  critical path, weighing each node with `plan_profile.task_duration_ns`. Ties
  break on the smaller node id, so the order is deterministic. Tasks absent
  from the map (including generated bridge channel nodes) weigh zero, which
  keeps a partial profile usable.
- **Durations live inline in the config**, not in a separate build-time file.

Inlining the profile is what keeps offline readers exact: the unified log
embeds the config, so logstats recomputes the same plan from the same data.
The pasted snippet is committed with the config, so CI reproduces the build.

What a better order can and cannot buy on the single-threaded runtime: total
work per cycle is fixed; the order only moves *latency* — it shortens the
sensor→actuator path of the chains it favors and reduces input staleness.
Later consumers of the same profile data are worth more: placing anytime
refine quanta into measured gaps (once anytime tasks land), and core packing
for `parallel-rt`.

## What the profile buys `parallel-rt` (v2)

`parallel-rt` is a stage-affine pipeline: one worker thread per plan step
(`cu29_derive/src/lib.rs`), each pinned to `cores[stage_index % cores.len()]`
(`thread_pool.rs`). Two consequences decide what the profile is worth there:

- Pipeline throughput is the duration of the **slowest single step**, not the
  sum. Reordering steps cannot change a maximum, so `CriticalPathFirst` buys
  `parallel-rt` essentially nothing. Ordering is a latency tool; parallel-rt
  needs a *balance* tool.
- Worker count equals step count, independent of core count. A 20-step graph
  on 4 cores spawns 20 threads.

Three uses of the same `PlanProfile`, cheapest first. None of them belongs in
`PlanPolicy` — they are placement, not ordering.

1. **Report the ceiling (done).** `cu29_export <log> log-stats` now emits a
   `pipeline` section: per-step duration stats, `serial_cycle_ns` (the serial
   engine's cycle), `bottleneck` (the slowest step, i.e. the `parallel-rt`
   cycle), and `max_pipeline_speedup = serial_cycle_ns / bottleneck.mean_ns`.
   The CLI prints the bottleneck line to stdout. This is diagnosis, not
   scheduling: it says whether steps 2 and 3 are worth doing at all. A graph
   whose `max_pipeline_speedup` is 1.2x will not repay a pipelining engine.
2. **Profile-driven core packing.** Replace the `stage_index % cores.len()`
   round-robin with an LPT bin-pack over `task_duration_ns`, so per-core load
   is balanced when steps outnumber cores. Self-contained in `thread_pool.rs`.
3. **Stage fusion.** Merge cheap adjacent steps into one worker until the step
   count is near the core count: fewer queue hops, lower latency, less
   oversubscription. This breaks the stage-index = plan-index identity in
   `build_parallel_rt_stage_entries`, so it is the largest of the three.

Note the histogram behind `CuDurationStatistics` is 1024 linear buckets over
its configured max, so its `percentile()` is useless at microsecond scale. The
`pipeline` section therefore reports only exact stats (min/max/mean/stddev);
an exact percentile comes from `schedule-profile --stat p99`, which keeps the
raw samples.

## Caveats

- A different order changes the copperlist slot layout, hence the generated
  types. Logs recorded under one plan do not resim under another. The policy
  is part of the embedded config, so a mismatch is detectable.
- A profile change requires a rebuild. Inherent to compile-time planning; the
  determinism and zero-alloc properties of the generated loop depend on it.

## Status

- v0 (done): the order/build split, the `PlanPolicy` config surface, golden
  tests pinning the default order.
- v1 (done): the `CriticalPathFirst` policy and the `schedule-profile` exporter.
- v2 (partial): the `pipeline` section of logstats reports the bottleneck and
  the ceiling. Core packing and stage fusion for `parallel-rt` are still open.
- Later: profile-driven placement of anytime refine quanta, once anytime tasks
  land.
