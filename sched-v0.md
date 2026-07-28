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

`expand_anytime_steps()` stays a separate pass after the build, as today.

## Config surface

A new enum next to the other runtime policies in `config.rs`:

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanPolicy {
    /// The historical source-BFS order. The default.
    #[default]
    TopoBfs,
}
```

RON:

```ron
runtime: (
    plan_policy: TopoBfs,
),
```

The field is optional and defaults to `TopoBfs`; v0 output is byte-identical
to the current planner (same order, same copperlist indices).

**Why the RON config and not a macro attribute:** the unified log embeds the
config. Offline tools (`cu29_export` logstats) recompute the plan from that
embedded config to map copperlist slots back to tasks. If the policy lived
outside the config, an offline reader could reconstruct the wrong slot layout.
Policy-in-config keeps one source of truth for everything that derives from
the plan.

## Profile-guided ordering (v1, not in this change)

This is a chicken-egg problem only in the way classic PGO is: the first build
cannot have a profile. The loop that resolves it:

```
build (TopoBfs) → run robot or resim → export profile
      → set Profiled policy in RON → rebuild → compare logstats → repeat
```

- **No new instrumentation.** Every `CuMsg` already records the
  before/after `process()` window in its metadata; the unified log has the
  per-task durations for every recorded cycle.
- **Exporter.** A `cu29_export` subcommand reads a `.copper` log and writes
  `schedule_profile.ron`: per-task duration stats (mean/p99) and per-chain
  end-to-end latency.
- **Policy.** `Profiled` orders steps by critical-path-first list scheduling
  over the measured durations. The macro reads the profile at build time, the
  same way it reads the RON config. A missing profile file is a build error,
  not a silent fallback — the profile is committed next to the config, like a
  lockfile, so CI reproduces the build.

What a better order can and cannot buy on the single-threaded runtime: total
work per cycle is fixed; the order only moves *latency* — it shortens the
sensor→actuator path of the chains it favors and reduces input staleness.
Later consumers of the same profile data are worth more: placing anytime
refine quanta into measured gaps (`expand_anytime_steps`), and core packing
for `parallel-rt`.

## Caveats

- A different order changes the copperlist slot layout, hence the generated
  types. Logs recorded under one plan do not resim under another. The policy
  is part of the embedded config, so a mismatch is detectable.
- `Profiled` in the embedded config must stay self-sufficient for offline
  readers: either inline the profile values into the config at build time, or
  record the resolved order. Decided in v1.
- A profile change requires a rebuild. Inherent to compile-time planning; the
  determinism and zero-alloc properties of the generated loop depend on it.

## Out of scope for v0

`Profiled` policy, the profile exporter, and refine-quantum placement. v0 is
the mechanical split, the `PlanPolicy` config surface, and golden tests that
pin the default order.
