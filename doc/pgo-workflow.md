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

- `Lag(d)`: the result of the compute dispatched for CL `n − d`; the gateway waits for
  it. Content is then independent of scheduling. The proposer picks `d` from the profiled
  compute cost and the CL period. The executor implements `d = 1` with `max_running: 1`
  and rejects other values at build time.
- `Sampled`: the newest completed result, whatever it is; never waits. This is today's
  `CuAsyncTask` behaviour. Which CL a result lands in then depends on scheduling; replay
  stays exact because the recorded result is reinjected where it entered. Validation
  lists sampled tasks (`CuMissionPlan::nondeterminism`) and the report carries them.

### 2.5 Capacity and commit

```ron
copperlists_per_cycle: 2,
max_in_flight: 3,
```

`max_in_flight` bounds the CLs admitted but not yet committed; it is at least `k`, so a
worker can never wait for a CL of a cycle that is only partly admitted, and the runtime
must preallocate at least that many (`logging.copperlist_count >= max_in_flight`,
validated; the logger may hold committed CLs on top). CLs commit in
CL id order once every occurrence for that CL has completed and the previous CL has
committed. Logging, monitoring and keyframe capture happen at commit, so they see the same
order as a serial run. `k`, `max_in_flight` and the number of lanes are independent:
`k = 1, max_in_flight = 3` overlaps three cycles.

### 2.6 Determinism

Two properties are distinct. **Replay determinism** always holds: every CL is logged
with its content, and replay reinjects recorded results (including sampled background
results) where they entered, so a replay reproduces the run whatever the scheduler did.
**Schedule independence** is the plan's property: the content of every CL depends only
on the plan and the inputs, never on when workers ran. With the rules above it holds
because:

1. an operation reads only its own CL's slots, after their producers completed;
2. a stateful component's calls are totally ordered across CLs;
3. shared resources are ordered unless declared concurrent-safe;
4. background results are bound to a fixed CL lag; `Sampled` gives this up knowingly,
   and only this;
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

**Keyframes are captured in the flow, never by draining.** A keyframe for CL `K` is the
state of every component at the `K` boundary. Each component freezes its own state inside
its own step for `K`, before that step runs, on whichever lane runs it; the state edges
order the component's `K − 1` step before its `K` step, so what it freezes is exactly its
state after `K − 1`, whatever the other lanes are doing at that moment. No CL waits for
another to commit, the pipeline keeps `max_in_flight` CLs in flight across a keyframe,
and latency does not change at keyframe CLs. This is the same mechanism `parallel-rt`
uses. The keyframe manager captures one CL at a time, so `keyframe_interval` must be at
least `max_in_flight` for a keyframe never to delay an admission; the proposer keeps
that, and the executor reports the case where it does not hold.

**Background compute** keeps the current pools. `Lag(1)` is a wait in the gateway for the
running job before publishing; `Sampled` is the current wrapper. The serial executor
honours `Lag(1)` the same way, so a serial and a multicore plan of one graph record the
same CLs.

**Errors** that a monitor turns into a shutdown set a flag that every wait checks, so no
lane blocks forever. A worker whose CPU or policy cannot be applied fails startup under
`on_error: Strict`, which is what plans emit. `App::request_stop()` (or Ctrl-C, which
stops every application in the process) ends the run at the next cycle boundary, so
every admitted cycle completes and commits.

**Tests:** `core/cu29_runtime/tests/lane_plan.rs` runs one graph (fork/join, a stateful
chain, a stateless task split across two workers with `k = 2`, a background task with
`Lag(1)`, keyframes on) under a serial plan and under a three-worker plan with four CLs
in flight, and requires the recorded CLs to be equal, keyed by producing task.

## 4. Profile

The profile is extracted from a log by the application's logreader
(`<logreader> <log> pgo-profile --contract pgo.ron --output profile.ron`), because reading
the log needs the generated message types. It contains, keyed by operation key:

- cost statistics (min, p50, mean, p95, p99, max) split by *fired* (an output payload was
  produced) and *skipped* CLs;
- the firing rate of each operation over the window, which gives its period;
- the wall span of each CL's steps;
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
worker. An occurrence's expected cost is its profiled fired cost weighted by its firing
probability per CL, plus its skipped cost otherwise; a background gateway costs nothing
(its compute runs on its pool). Lane load, response and rate use the expected cost; a
chain is timed with the fired costs, since its latency is paid in the cycles it runs in. With `C_i` the cost of lane `i` per cycle, `T_i` its
period (the CL period times `k`), `g` the dispatch granularity, and `hp(i)` the
higher-priority lanes on the same CPU, the response time is the paper's fixpoint

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

**Objective**, lexicographic, smallest first: sources below their expected rate (a rate
within 0.5% of nominal counts as kept); chains with `L_c` over their deadline; chains
over `(1 − margin)` of it; sum of `L_c / D_c`; largest worker load. `objective: (kind:
sum)` drops the two count tiers. `objective: (kind: miss_rate)` replaces them with the
worst chain's predicted miss rate, then the mean: a chain over its deadline with mean
costs counts 50%, over it only with p95 costs 5%, with p99 costs 1%, with worst costs
0.1%. The same tiers, from measured p99 latencies, miss counts and delivered rates, rank
the candidates after measurement.

**Search.** `cu29-plan <config> --propose pgo.ron --profile profile.ron --cycle k` starts
from the exported `k`-CopperList inventory (every required edge present, so any
acyclic placement is valid). Workers form a pipeline: no zero-lag edge may lead from a
later worker back to an earlier one, because two workers waiting on each other inside a
CopperList finish every CopperList together and never run ahead, whatever `max_in_flight`
allows. Measured on the Autoware replica at four cores, such plans kept two CopperLists in
flight of forty and idled 36–45% of every lane. Under a real-time `worker_policy` each CPU
gets two workers, the base one and one a priority above it; the higher one may only hold
units on chains due within their source's period, which is what it preempts for, and such
a chain stays whole within one tier, because the base workers run whole cycles behind the
higher ones. A chain segment on a base worker is stretched by the share its CPU's higher
worker takes. Rates and chain latencies come from a two-window timeline of the lanes in
steady state, cycles released on the grid and held by the ring: a lane's rate is the
window over its cycle time in the second window, waits included, and a chain across lanes
pays the cycles one lane runs behind the other. A worker counts as full past `(1 −
margin)` of the window, the room the costs' tails need. The start is connected components
in deadline order, each in earliest-start order, cut into one stage per CPU by load; a
component of short-deadline work goes whole to the least loaded higher worker; a component
that mixes short and long work stays on the base workers, and the search never changes a
unit's tier. The search unit is one occurrence, or an anytime base
occurrence with its refinements, which the executor runs together on one worker. The
profile must carry the config's graph signature. Moves: move an occurrence to another worker of its tier and position;
swap two neighbours on a worker. A move that closes a cycle with the required edges, or
between workers, is rejected before scoring. Seeded, fixed budget, restarts; the best `N`
plans with distinct scores are written as `plan-<n>.ron` with `predictions.ron`, each
raising `logging.copperlist_count` to its `max_in_flight` on import. With `k = 2` a
stateless task's two occurrences may land on different workers. `max_in_flight` comes
from the contract and must cover the pipeline's depth (a chain's latency over the
CopperList period); it is not searched. The proposer never claims optimality; the
measurement step decides.

## 7. Measuring and selecting

For each candidate: `Fixed::apply` into a config, build the application against it, run
for the contract's duration, extract chains and rates with the same logreader subcommand
as the profile. `cu29-plan <config> --score pgo.ron --predictions predictions.ron
--measured plan-1=pgo/runs/plan-1.ron ...` writes one table: per candidate and chain,
predicted and measured p50 / p99 / misses, delivered rates, and the objective, then
names the best measured plan. The
baseline (the profiled run) is one row of the same table. Predictions are reported beside
measurements and never replace them.

## 8. Workflow

Shared recipes in `support/just/plan.just`, run from an example directory:

```sh
just pgo-profile <log> <logreader-bin> [pgo.ron] [pgo/profile.ron]   # measure a recorded run
just pgo-propose [config] [pgo.ron] [profile] [out-dir] [cycle] [n]    # candidates + predictions
just pgo-import  [config] [plan] [copperconfig-pgo.ron]              # embed one candidate
```

Measuring a candidate is a rebuild of the application against the embedded config
(the plan is compiled in), a run, and `pgo-profile` on its log; `cu29-plan --score`
lays the candidates' profiles beside their predictions and names the best measured one.
An example wraps these into its own `just profile`, `just propose`, `just measure`.
Artifacts of one pass live under `pgo/` in the example: profile, candidates, predictions,
the candidates' profiles, and the score. A pass is repeatable from its directory.

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
