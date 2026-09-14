# Profile-guided scheduling: design

Status: proposal, 2026-09-14. Nothing here is implemented yet; `doc/execution-plans.md`
describes what exists today (the current plan format and its serial execution).

## 1. Goal

Pick, for one application on one machine, a multicore execution plan by measurement:

```text
record a log  ->  profile  ->  propose candidate plans  ->  build each  ->  measure  ->  select
```

Every candidate is a complete, validated plan that the runtime executes exactly. Message
content never depends on timing: the same plan and the same inputs give the same
CopperLists, on one core or on many. Timing is what the plan changes and what the loop
measures.

Three constraints shape the design:

- A plan is a **repeating execution graph indexed by CopperList id**, not a task order and
  not a CPU assignment of callbacks. It states, per CL, what runs after what; across CLs,
  what must stay ordered; and on the machine, who runs what with which priority.
- The runtime enforces the plan with no scheduler of its own: no work queue, no dynamic
  choice, no sampling of "the latest" result unless the plan says so explicitly.
- Planning tools live in this repository (Rust), share the plan validator, and are run
  through `just`. Executor baselines from other systems stay in the artifact repository.

## 2. The plan: a repeating execution graph

### 2.1 Operations and occurrences

An **operation** is one unit the executor runs. Its key is the existing process-step key
(`mission:m|task:t|phase:p`, bridge channel keys, anytime `phase:refine:n`). Kinds:

| kind | what it does | runs on |
|---|---|---|
| task step | `process` (or anytime `base` / `refine`) of a foreground task for one CL | a lane worker |
| bridge step | one rx or tx channel of a bridge for one CL | a lane worker |
| background gateway | the in-CL call of a `background:` task: publish a result into this CL's output slot, then dispatch this CL's input | a lane worker |
| background compute | the job the gateway dispatched | a queue worker of a pool |

A **cycle** covers `copperlists_per_cycle = k` consecutive CLs. An **occurrence** is
`(operation, offset)` with `offset < k`; in cycle `c` it is the operation applied to CL
`c*k + offset`. Every operation has exactly one occurrence per offset. A cycle of `k = 1`
is the common case; `k = 2` lets a stateless task's even and odd CLs live on different
workers.

### 2.2 Edges

An edge `(from, to, cycle_lag)` means: occurrence `to` in cycle `c` starts only after
occurrence `from` in cycle `c - cycle_lag` has completed. Edges with `cycle_lag = 0` must
be acyclic together with lane order. Cycles before the first are treated as complete.

The exporter writes every edge the graph requires, so a proposer moves work without
reconstructing the rules:

| source of the edge | within a CL | across CLs |
|---|---|---|
| data: producer slot to consumer | producer → consumer, lag 0 | — |
| fork/join | the same: a join waits for every producer | — |
| anytime phases | base → refine 1 → refine 2 … | — |
| stateful task or bridge instance (`&mut self`) | its steps in one order | last step in CL n → first step in CL n+1 (through the cycle boundary with lag 1) |
| shared resource bound by two components (`Borrowed`) | the two components in one order | as for state |
| background result | gateway(n) → compute(n); compute(n−d) → gateway(n) when `result: Lag(d)` | via `d` |

Stateless tasks (`kind: stateless_task`) have no state edges. `Owned` resources belong to
one component and add nothing. A shared resource that is safe to use concurrently can be
declared so in the contract (§5) and then adds nothing either; the default is ordered.

Two operations with no path between them may overlap. There is no "mutually exclusive but
in either order" constraint: an order that is not fixed would make content depend on
timing, so the proposer must choose one.

### 2.3 Workers

The plan owns its workers. The executor is generated from the plan, so it applies each
worker's CPU and policy itself with the runtime's existing thread setup; nothing is
duplicated into `runtime.thread_pools`, which keeps only background pools.

```ron
workers: [
    (id: "w0", placement: (kind: "thread", cpu: Some(1), policy: Fifo(priority: 70)), steps: [0, 2, 5]),
    (id: "w1", placement: (kind: "thread", cpu: Some(2), policy: Fifo(priority: 70)), steps: [1, 3]),
    (id: "w2", placement: (kind: "thread", cpu: Some(2), policy: Fifo(priority: 40)), steps: [4]),
],
dispatcher: Some((cpu: Some(0), policy: Fifo(priority: 80))),
```

- A **lane worker** runs its `steps` (occurrence indices) in that order once per cycle and
  then starts the next cycle. Two workers may share a CPU; then their priorities decide
  who preempts whom, which is how a low-priority long job and a short periodic one share a
  core.
- A **queue worker** belongs to a background pool and runs compute jobs in dispatch order,
  at most `max_running` per task. Its placement is independent of the gateway's lane.
- The **dispatcher** is the main thread: it admits CLs, commits them in order, and drives
  keyframes. It runs no operation. Its own CPU and policy are part of the plan because a
  dispatcher on `SCHED_OTHER` next to FIFO workers slips releases.
- `placement: (kind: "main")` for a single worker with `k = 1` and `max_in_flight = 1`
  is the serial subset: the current executor, unchanged.

### 2.4 Background work

```ron
background: [
    (task: "detector", pool: "vision", max_running: 1, result: Lag(1)),
],
```

`result` states what the gateway publishes into CL `n`:

- `Lag(d)`: the result of the compute dispatched for CL `n − d`; the gateway waits for it.
  Deterministic. The proposer picks `d` from the profiled compute cost and the CL period.
- `Sampled`: the newest completed result, whatever it is; never waits. This is today's
  `CuAsyncTask` behaviour. Content then depends on timing, and a plan that uses it is
  marked non-deterministic in its metadata and in the report.

### 2.5 Capacity and commit

```ron
copperlists_per_cycle: 2,
max_in_flight: 3,
```

`max_in_flight` bounds the CLs admitted but not yet committed; the runtime must
preallocate at least that many (`logging.copperlist_count >= max_in_flight`, validated;
the logger may hold committed CLs on top). CLs commit in
CL id order once every occurrence for that CL has completed and the previous CL has
committed. Logging, monitoring and keyframe capture happen at commit, so they see the same
order as a serial run. `k`, `max_in_flight` and the number of lanes are independent:
`k = 1, max_in_flight = 3` overlaps three cycles.

### 2.6 Determinism

With the rules above, the content of every CL depends only on the plan and the inputs:

1. an operation reads only its own CL's slots, after their producers completed;
2. a stateful component's calls are totally ordered across CLs;
3. shared resources are ordered unless declared concurrent-safe;
4. background results are bound to a fixed CL lag, unless `Sampled` is chosen knowingly;
5. commit, logging and keyframes follow CL id order.

Tasks that read the clock (`ctx.now()`, pacers) are unchanged: their behaviour is the same
as under the serial executor, and replay stays deterministic at CL granularity as today.

### 2.7 Format and validation

The format replaces the current one in place; nothing released carries the old shape, so
there is no compatibility path. It keeps the inventory (`steps`), `dependencies` and
`copperlists_per_cycle`, replaces `lanes` by `workers`, and adds `max_in_flight`,
`dispatcher`, `background`, and the plan-level `concurrent_resources` list. Validation
reports the reasons a plan is not deterministic (§2.6). The serial subset re-exports
unchanged in meaning.

Validation adds to the current checks: every background task has one `background` entry
and its pool exists; `result: Lag(d)` has `d ≥ 1`; every worker has a distinct id;
`max_in_flight ≥ 1` and `logging.copperlist_count >= max_in_flight`; resource edges are
present or declared away; the plan's cycle graph is deadlock-free (lag-0 edges plus lane
order acyclic; each lane's last occurrence precedes its first with lag 1). Validation does
not check CPU existence, real-time permissions, or timing; the executor reports those.

## 3. The executor

The executor generalises `parallel-rt`: the same dispatcher loop, the same generated
per-step functions, but lanes with dependency waits instead of a fixed pipeline of stage
queues. It is selected when the config's `Fixed` plan is not the serial subset. It
requires `std`, is not used in `sim_mode`, and is emitted at compile time from the plan,
so a plan change is a rebuild.

**State, all preallocated:** a ring of `max_in_flight` boxed CLs; `admitted_clid`
(atomic); per occurrence `completed_cycles` (atomic, monotonic); per CL slot an
`aborted` flag; a global generation counter with a mutex/condvar for parking.

**Lane worker loop** for cycle `c`, occurrence `o` at offset `j`:

1. wait until `admitted_clid > c*k + j` (the CL exists) and, for every edge
   `(from, o, lag)`, `completed_cycles[from] > c - lag`; wait by spinning briefly, then
   parking on the generation counter;
2. if the CL is aborted, skip the call; else run the generated step function with a
   pointer to the CL and to the component (`&mut` for stateful, `&` for stateless);
   monitoring decisions are applied as in `parallel-rt` (`Ignore`, abort the CL, shutdown);
3. publish `completed_cycles[o] = c + 1`, bump the generation, notify.

No lock is held while an operation runs. Distinct occurrences never write the same slot
concurrently (one producer per slot, consumers ordered after it) and never hold `&mut` to
the same component concurrently (state edges), which is what validation guarantees.

**Dispatcher loop:** admit CL `n` when the rate limiter's tick is due, `in_flight <
max_in_flight`, a free CL box exists, and the keyframe rule allows; publish the CL pointer,
then `admitted_clid = n + 1`. Commit the smallest uncommitted CL whose occurrence counters
have all passed it: monitor, log, keyframe end, recycle the box. On `STOP_FLAG`, stop
admitting, wake every lane, let in-flight CLs commit, then `stop_all_tasks`.

**Keyframes:** a CL `K` that captures a keyframe is admitted only after every CL `< K` has
committed, and the next CL only after `K`'s snapshot is taken. That drains the pipeline at
every keyframe interval and makes the snapshot a consistent CL boundary, exactly as in the
serial runtime. The cost is one bubble per interval and is reported by the profile.

**Background compute** keeps the current pools. `Lag(d)` is a wait in the gateway before
publishing; `Sampled` is the current wrapper.

**Errors** that a monitor turns into a shutdown set a flag that every wait checks, so no
lane blocks forever. A worker whose CPU or policy cannot be applied fails startup under
`on_error: Strict`, which is what plans emit.

**Tests:** a synthetic graph with a fork/join, a stateful chain, a stateless task split
across two workers with `k = 2`, a background task with `Lag(1)`, and keyframes on; the
logged CL stream must equal the serial run's stream byte for byte (as
`cu_runtime_matrix` checks today), on a deterministic clock and on the real one.

## 4. Profile

The profile is extracted from a log by the application's logreader
(`extract-pgo-profile --contract pgo.ron --out profile.ron`), because reading the log needs
the generated message types. It contains, keyed by operation key:

- cost statistics (min, p50, mean, p95, p99, max) split by *fired* (an output payload was
  produced) and *skipped* CLs;
- the firing rate of each operation over the window, which gives its period;
- for background tasks, compute cost and gateway cost separately;
- per-CL dispatcher and commit overhead, and the keyframe bubble;
- the chain measurements of §7 for the profiled run, as a baseline;
- the config signature and the plan the run executed, so a profile is never applied to a
  graph it was not recorded on.

A profile recorded under one plan is valid input for any plan of the same graph: an
operation's cost is a property of the operation, not of where it ran. CPU-level effects
(cache, memory bandwidth, preemption cost) are not in the profile; they are what the
measurement step exists for.

## 5. Contract

`pgo.ron` next to `copperconfig.ron`, read only by the tools:

```ron
(
    chains: [
        (id: "hot", source: "front_lidar", sink: "object_collision_estimator", deadline_ms: 200),
        (id: "rt2", source: "behavior_planner", sink: "vehicle_dbw", deadline_ms: 100),
    ],
    sources: [
        (task: "front_lidar", period_ms: 200),   // expected delivery, for the rate criterion
    ],
    cpus: [0, 1, 2, 3],
    max_in_flight: 3,
    objective: (kind: deadline, margin: 0.2),
    concurrent_safe_resources: [],
    background_pools: [(id: "vision", cpus: [3], policy: Fifo(priority: 40))],
)
```

Priorities per task are not declared: the proposer derives worker priorities from chain
deadlines and the profile. A chain is measured inside one CL: the sink's process end minus
the source's time of validity, for every CL in which the sink produced a payload.

## 6. Proposing candidates

`cu29-plan --propose --contract pgo.ron --profile profile.ron --out candidates/` writes
`N` distinct plans plus `predictions.ron`.

**Model.** The paper's response-time analysis, with the lane as the unit. It answers
"what is the worst a chain can see under this placement and these priorities", from a
critical instant, independent of the phasing that happened to be recorded; it is closed
form and cheap, so it ranks every candidate the search visits. It is a prediction, not a
measurement, and is written beside every candidate and reported beside its measurement.

**Analytical model.** The unit is the lane (the paper's region): a fixed sequence on one
worker. With `C_i` the mean profiled cost of lane `i` per cycle, `T_i` its period (the CL
period times `k`), `g` the dispatch granularity, and `hp(i)` the higher-priority lanes on
the same CPU, the response time is the paper's fixpoint

    R_i = C_i + sum_{j in hp(i)} ceil((R_i + g) / T*_j) C_j,   T*_j = max(T_j, R_j + g/2)

and the delivered fraction is 1 when `R_i <= T_i`, else `T_i / (R_i + g/2)`. A chain is a
walk over lane segments; a segment costs the lane's prefix up to the chain's operation,
with the same interference term applied to that prefix; a cross-lane edge adds the wait
for the producing segment's completion; a `Lag(d)` background edge adds `d` CL periods.
Chain latency `L_c` is the sum along the walk from the source's start; `L_c / D_c`
normalises it. The typical estimate divides each segment by `1 − U_hp` instead of taking
the ceiling, as in the paper. The two changes from the region runtime are that a lane can
wait on another lane inside a CL (fork/join), and that lanes on distinct CPUs interfere
only through those waits.

**Objective**, lexicographic, smallest first: sources below their expected rate; chains
with `L_c` over their deadline; chains over `(1 − margin)` of it; sum of `L_c / D_c`;
largest worker load. `objective: (kind: sum)` drops the two count tiers. The chosen plan
is scored a second time with every operation at its p99 cost; that column is reported,
never optimised.

**Search.** Start from a list schedule on `cpus` (ready operations ordered by the slack of
the tightest chain through them, first-fit by load). Moves: move an occurrence to another
lane and position; swap two occurrences on a lane; change a worker's CPU or priority;
give a stateless task's offsets different workers (`k = 2`); change `max_in_flight`.
A move that breaks a required edge is rejected before scoring. Seeded, fixed budget,
restarts; the best `N` plans with distinct scores are written. The proposer never claims
optimality; the measurement step decides.

## 7. Measuring and selecting

For each candidate: `Fixed::apply` into a config, build the application against it, run
for the contract's duration, extract chains and rates with the same logreader subcommand
as the profile. `cu29-plan --score --contract pgo.ron --predictions predictions.ron
runs/*` writes one table: per candidate and chain, predicted and measured p50 / p99 /
miss rate, delivered rates, and the objective, then names the best measured plan. The
baseline (the profiled run) is one row of the same table. Predictions are reported beside
measurements and never replace them.

## 8. Workflow

```sh
just pgo-profile   [config] [seconds]     # run the current config, extract profile.ron
just pgo-propose   [n]                    # candidates/plan-*.ron + predictions.ron
just pgo-measure   [seconds]              # build + run each candidate, extract chains
just pgo-score                            # the comparison table, best plan named
just pgo-select    <plan>                 # Fixed::apply -> copperconfig-pgo.ron
```

Artifacts of one pass live under `pgo/<tag>/` in the example crate: profile, contract,
candidates, predictions, runs, score. A pass is repeatable from its directory.

## 9. Steps

1. This document, reviewed.
2. The plan format of §2: validation and export; the serial subset re-exports unchanged.
3. The executor (§3) with its determinism tests, then the same equality check on the
   flight controller example's replay tests.
4. Profile and chain extraction in `cu29_export` (§4, §7) and the logreader subcommand.
5. The proposer: the response-time model, objective and search (§6), with a test that
   reproduces a published placement from its profile.
6. Recipes (§8) and the Autoware Universe replica ported as an example (can start in
   parallel with steps 3 to 5); the first full pass runs on it.

Each step is a pull request on top of the previous one; none is measured before its tests
pass.

## 10. Out of scope

Refinement allocation for anytime tasks (the plan carries their phases as operations, but
no quality model); accelerators; changing a plan at run time; `Sampled` background results
as anything but an explicitly non-deterministic choice; a discrete-event simulation of a
plan on the recorded firing sequence (a possible later predictor of typical latency, put
aside for now).
