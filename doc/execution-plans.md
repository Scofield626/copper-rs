# Saving and enforcing an execution plan

The experimental `CuPlan` format represents a repeating execution schedule for
each mission. It supports multiple workers, several CopperLists per cycle,
and precedence constraints within and across cycles. Planning and validation
happen at compile time or in offline tools.

`Linearity` chooses a node order; `Pinned` accepts a task order and lets Copper
place bridge stages and foreground anytime refinements. `Fixed` consumes an
already scheduled plan. A serial plan (one main-thread worker, one CopperList
per cycle, one in flight) runs on the main thread. A multicore plan runs on the
lane executor, which the `parallel-rt` feature provides: one thread per worker
with the plan's CPU and policy, CopperLists admitted up to `max_in_flight` and
committed in id order, keyframes captured by each component inside its own
step. Without the feature a multicore plan fails compilation; placement and
dependencies are never silently discarded. `App::request_stop()` ends a run at
the next cycle boundary.

## Workflow

From an example directory that imports the root justfile:

```bash
just plan-export
# Edit plan.ron, or let an offline scheduling tool write it.
just plan-validate
just plan-import
```

Defaults read `copperconfig.ron`, export `plan.ron`, and import into
`copperconfig-fixed.ron`. Explicit positional arguments work too:

```bash
just plan-export copperconfig.ron candidate.ron
just plan-validate copperconfig.ron candidate.ron
just plan-import copperconfig.ron candidate.ron copperconfig-fixed.ron
```

Import validates and embeds the supplied plan as the `plan` parameter of
`runtime.planner` with `type: "cu29::planner::Fixed"`. To execute the plan,
point the runtime macro, logreader's `gen_cumsgs!`, and replay macro at the
resulting config and rebuild (with the `parallel-rt` feature for a multicore
plan). Alternatively, replace the original config with the reviewed result. A
startup config change cannot change compiled execution.
`just plan config=copperconfig-fixed.ron` renders the plan's slot layout as a
serial sequence; it does not draw workers.

Import flattens the effective configuration, including selected fragments.
Pass the active Cargo features as the recipe's final argument when needed.
Existing out-of-tree planners continue to use `CuPlanner` and `emit_plan`;
export their baked effective config to capture the resolved schedule.

## Multicore representation

A mission plan is a repeating execution graph indexed by CopperList id. Each
mission contains:

- `copperlists_per_cycle`: the number of consecutive CLs in one repeating cycle.
- `max_in_flight`: the largest number of CLs admitted but not yet committed;
  `logging.copperlist_count` must preallocate at least that many.
- `steps`: an inventory of `(key, copperlist)` occurrences. Keys are opaque
  process-step identities exported from Copper, including bridge stages and
  numbered anytime refinements; `copperlist` is an offset within the cycle.
- `workers`: an id, a placement, and an ordered list of inventory indices.
  Moving work between workers does not renumber the inventory or its
  dependencies. `placement: (kind: "main")` is the application's main thread;
  `(kind: "thread", cpu: Some(n), policy: Fifo(priority: p))` is a dedicated
  thread with an optional CPU pin and a `runtime.thread_pools` scheduling
  policy. Two threads may share a CPU; their policies then decide preemption.
- `dispatcher`: the CPU pin and policy of the thread that admits and commits
  CLs in a multicore plan.
- `background`: one entry per `background:` task with its pool, `max_running`,
  and what its gateway publishes into a CL: `(kind: "sampled")`, the newest
  completed result, or `(kind: "lag", lag: d)`, the result of the compute
  dispatched for CL `n - d`. Sampled results make content depend on timing;
  `CuMissionPlan::nondeterminism` lists them.
- `dependencies`: `(from, to, cycle_lag)` precedence edges. For schedule cycle
  `n`, `to(n)` waits for `from(n - cycle_lag)`. These are completion constraints,
  not changes to payload wiring.

The plan-level `concurrent_resources` lists resources (`bundle.resource`) that
several components may use at the same time. Every other resource bound by more
than one component orders those components like shared mutable state.

For example, this two-worker schedule pipelines two CopperLists per cycle:

```ron
(
    version: 2,
    missions: {
        "default": (
            copperlists_per_cycle: 2,
            max_in_flight: 2,
            steps: [
                (key: "mission:default|task:src|phase:whole", copperlist: 0),
                (key: "mission:default|task:sink|phase:whole", copperlist: 0),
                (key: "mission:default|task:src|phase:whole", copperlist: 1),
                (key: "mission:default|task:sink|phase:whole", copperlist: 1),
            ],
            workers: [
                (id: "w0", placement: (kind: "thread", cpu: Some(1), policy: Fifo(priority: 60)), steps: [0, 2]),
                (id: "w1", placement: (kind: "thread", cpu: Some(2), policy: Fifo(priority: 60)), steps: [1, 3]),
            ],
            dispatcher: Some((cpu: Some(0), policy: Fifo(priority: 70))),
            dependencies: [
                (from: 0, to: 1, cycle_lag: 0),
                (from: 2, to: 3, cycle_lag: 0),
            ],
        ),
    },
)
```

The corresponding config declares a `src -> sink` graph and
`logging: (copperlist_count: 2)`. Each worker repeats in order, so source and
sink state each advance in CL order. A worker's final event in cycle `n`
precedes its first event in cycle `n+1`. There is **no global cycle barrier**.
Dependencies on cycles before the initial cycle are initially satisfied.
Explicit positive-lag edges can order a component whose occurrences move across
workers between cycles. The executor must honor worker order and all dependency
edges; worker threads take their CPU and policy from the plan.

## Validation and execution boundary

Validation requires every process step exactly once per CL offset, every
occurrence assigned to exactly one worker, distinct worker ids and valid
policies, `max_in_flight` at least `copperlists_per_cycle` and within the
preallocated CLs, one `background` entry
per background task with a known pool, and no cycle among same-cycle
dependency and worker-order edges. Message
producers must finish before consumers; anytime base and refinement phases
must stay ordered. Calls sharing a mutable task or bridge instance, or a
resource not listed in `concurrent_resources`, must be ordered within a CL and
across consecutive CLs, including the cycle boundary.
Stateless tasks are the exception, as described below.
Dependencies may satisfy these constraints transitively. The exporter includes
required data/phase/state edges so an optimizer can move independent work to
separate workers without reconstructing the graph's constraints.

## Stateless tasks

A transform whose invocations are independent implements `CuStatelessTask`
and declares `kind: stateless_task`:

```ron
(id: "features", type: "tasks::Features", kind: stateless_task),
```

`preprocess`, `process`, and `postprocess` take `&self`, and the task is
`Send + Sync`, so one instance can serve several workers. Construction,
`start`, `stop`, and `thaw` keep exclusive access. Implementing the trait also
declares that an invocation does not depend on earlier invocations and has no
effect whose meaning depends on invocation order. Configuring an ordinary
`CuTask` with this kind fails compilation. Stateless tasks cannot be
`background` or `anytime`.

A plan need not order a stateless task's occurrences across CopperLists.
The exporter omits those state edges, and the validator accepts, for example,
`features` for CL 0 and CL 1 on two workers with nothing between them. Each
occurrence still waits for its own CL's inputs, and every consumer still waits
for its own CL's output. The serial executor calls stateless tasks in plan
order like any other task.

This structural validator does not prove timing/deadline feasibility, bounded
buffer feasibility, OS CPU availability, or contention safety for arbitrary
shared resources and asynchronous background jobs. Background steps describe
poll/dispatch gateways; completion of the external job retains its existing
semantics. The executor checks at build time that `logging.keyframe_interval`
is at least `max_in_flight`, so a keyframe never delays an admission.

Copper regenerates typed message slots from the plan (a serial plan's order, or
a multicore plan's topological order) while preserving RON connection order
within each input tuple.
Existing error handling and anytime budget/skip behavior still apply; fixing
phase order does not force optional refinements to run.
No plan parsing, lookup, or serialization is added to the execution path.
Unsupported versions and fields are rejected.

Rust tools use `cu29::planner::CuPlan::{from_config, validate, read, write}` and
`Fixed::new(plan)?.apply(&mut config)`. `serialize_ron`, `deserialize_ron`, and
validation also compile without `std`; file helpers require `std`.
Run `just plan-check` for focused tests, replay, lint, and no_std checks.
