# Saving and enforcing an execution plan

The experimental `CuPlan` format represents a repeating execution schedule for
each mission. It supports multiple worker lanes, several CopperLists per cycle,
and precedence constraints within and across cycles. Planning and validation
happen at compile time or in offline tools.

`Linearity` chooses a node order; `Pinned` accepts a task order and lets Copper
place bridge stages and foreground anytime refinements. `Fixed` consumes an
already scheduled plan. This PR supports **representation and validation** of
multicore plans; fixed execution supports one main lane and one CopperList per
cycle. Multicore execution and new scheduling heuristics are subsequent work.
Unsupported execution fails compilation, including use with `parallel-rt`;
placement and dependencies are never silently discarded.

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
`runtime.planner` with `type: "cu29::planner::Fixed"`. It supports saving valid
multicore configurations for the future executor. To execute a supported serial
plan, point the runtime macro, logreader's `gen_cumsgs!`, and replay macro at the
resulting config and rebuild. Alternatively, replace the original config with
the reviewed result. A startup config change cannot change compiled execution.
`just plan config=copperconfig-fixed.ron` renders a supported serial plan;
multicore visualization is not implemented by that serial visualizer.

Import flattens the effective configuration, including selected fragments.
Pass the active Cargo features as the recipe's final argument when needed.
Existing out-of-tree planners continue to use `CuPlanner` and `emit_plan`;
export their baked effective config to capture the resolved schedule.

## Multicore representation

Each mission contains:

- `copperlists_per_cycle`: the number of consecutive CLs in one repeating cycle.
- `steps`: an inventory of `(key, copperlist)` occurrences. Keys are opaque
  process-step identities exported from Copper, including bridge stages and
  numbered anytime refinements; `copperlist` is an offset within the cycle.
- `lanes`: placement plus an ordered list of inventory indices. Moving work
  between lanes does not renumber the inventory or its dependencies.
- `dependencies`: `(from, to, cycle_lag)` precedence edges. For schedule cycle
  `n`, `to(n)` waits for `from(n - cycle_lag)`. These are completion constraints,
  not changes to payload wiring.

For example, this two-worker schedule pipelines two CopperLists per cycle:

```ron
(
    version: 1,
    missions: {
        "default": (
            copperlists_per_cycle: 2,
            steps: [
                (key: "mission:default|task:src|phase:whole", copperlist: 0),
                (key: "mission:default|task:sink|phase:whole", copperlist: 0),
                (key: "mission:default|task:src|phase:whole", copperlist: 1),
                (key: "mission:default|task:sink|phase:whole", copperlist: 1),
            ],
            lanes: [
                (placement: (kind: "worker", pool: "rt", index: 0), steps: [0, 2]),
                (placement: (kind: "worker", pool: "rt", index: 1), steps: [1, 3]),
            ],
            dependencies: [
                (from: 0, to: 1, cycle_lag: 0),
                (from: 2, to: 3, cycle_lag: 0),
            ],
        ),
    },
)
```

The corresponding config must declare pool `rt` with at least two workers and
a `src -> sink` graph. Each lane repeats in order, so source and sink state each
advance in CL order. A lane's final event in cycle `n` precedes its first event
in cycle `n+1`. There is **no global cycle barrier**. Dependencies on cycles
before the initial cycle are initially satisfied. Explicit positive-lag edges
can order a component whose occurrences move across lanes between cycles.

A serial lane uses `placement: (kind: "main")`. Worker placement is logical:
`runtime.thread_pools` maps a worker to CPU affinity using the existing
`affinity[index % affinity.len()]` rule. This keeps plans portable; two workers
sharing one affinity CPU remain distinct lanes and acquire no implicit
precedence. The executor must honor lane order and all dependency edges.

## Validation and execution boundary

Validation requires every process step exactly once per CL offset, every
occurrence assigned to exactly one lane, valid and distinct worker placements,
and no cycle among same-cycle dependency and lane-order edges. Message
producers must finish before consumers; anytime base and refinement phases
must stay ordered. Calls sharing a mutable task or bridge instance must be
ordered within a CL and across consecutive CLs, including the cycle boundary.
Stateless tasks are the exception, as described below.
Dependencies may satisfy these constraints transitively. The exporter includes
required data/phase/state edges so an optimizer can move independent work to
separate lanes without reconstructing the graph's constraints.

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
semantics. Runtime memory sizing and execution support for multicore plans
belong with the future executor.

For supported serial plans, Copper regenerates typed message slots from the
chosen order while preserving RON connection order within each input tuple.
Existing error handling and anytime budget/skip behavior still apply; fixing
phase order does not force optional refinements to run.
No plan parsing, lookup, or serialization is added to the execution path.
Unsupported versions and fields are rejected.

Rust tools use `cu29::planner::CuPlan::{from_config, validate, read, write}` and
`Fixed::new(plan)?.apply(&mut config)`. `serialize_ron`, `deserialize_ron`, and
validation also compile without `std`; file helpers require `std`.
Run `just plan-check` for focused tests, replay, lint, and no_std checks.
