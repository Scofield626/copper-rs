# cu-anytime-rrt-star

Example Copper app that drives an anytime RRT\* motion planner through the
`cu29::AnytimeTask<A>` adapter. It demonstrates:

- The `Anytime` trait — `base` / `refine` / `write_best` / `progress` — used
  by a real algorithm rather than a toy counter.
- The `AnytimeTask` adapter's per-cycle budget + `OnOverload::ReuseLast`
  behaviour under an 8 ms deadline at 100 Hz.
- Deterministic randomised search via the `CuRng` resource, so recorded runs
  replay byte-identically.

## Graph

```
QuerySource ── PlanQuery ──► AnytimeTask<RrtStarPlanner> ── AnytimeOutput<PlannedPath> ──► PathSink
                                       ▲
                            planner_rng: CuRngBundle
```

`QuerySource` emits a fixed `(start, goal)` read from `copperconfig.ron`.
`AnytimeTask<RrtStarPlanner>` runs one RRT\* iteration per `refine` call —
sample, nearest, steer, collision-check against the occupancy grid, near
neighbours, choose-parent, insert, rewire, goal-check — until either the 8 ms
budget expires, the tree hits `max_tree_nodes`, or the best-so-far cost
drops below `PlanQuery::max_cost` (when set). `PathSink` logs one line per
cycle: `progress`, `cost`, `iter`, `tree_size`, `waypoints`.

## Running

```
cargo run -p cu-anytime-rrt-star
```

The binary anchors CWD at its crate directory before building the app, so
the `maps/depot.pgm` path and the `logs/` directory in `copperconfig.ron`
resolve regardless of where the command is invoked from.

Typical steady-state output for the default `(1.5, 1.5) → (30.5, 30.5)`
query on the demo map (straight-line distance ≈ 41.0):

```
planner: progress=passes cost=42.4 iter=690 tree_size=627 waypoints=21
planner: progress=passes cost=42.2 iter=695 tree_size=629 waypoints=19
planner: progress=passes cost=43.5 iter=671 tree_size=629 waypoints=22
...
```

Each cycle rebuilds the tree from scratch in `base` and runs the refine
loop until the 8 ms budget expires; `passes` means the loop was cut by
the deadline, `converged` means `max_tree_nodes` was hit first.

## The demo map

`maps/depot.pgm` is a 32×32 P5 PGM (world resolution 1.0 m/cell) with two
rectangular obstacles that force the planner to route around them:

- one obstacle at columns 10–13, rows 6–17
- one at columns 18–21, rows 14–25

Regenerate it after edits with:

```
python3 examples/cu_anytime_rrt_star/maps/gen_demo_map.py
```

The loader reads any 8-bit P5 PGM; cell values `>= 128` are free (Nav2's
trinary threshold default), values below are treated as occupied. Points
off the grid are also occupied, so a segment that leaves the map is
rejected.

## Config knobs (planner node)

| Key                  | Meaning                                                |
| :------------------- | :----------------------------------------------------- |
| `budget_us`          | Adapter refine budget per cycle (µs).                  |
| `on_overload`        | `reuse_last` (default) or `fail`.                      |
| `step_size`          | Max distance from the nearest node to the new sample.  |
| `goal_threshold`     | Distance at which a new node is spliced to the goal.   |
| `goal_bias`          | Probability of sampling the goal directly (in `[0,1]`).|
| `neighbor_radius`    | Radius for the near-neighbour set (choose-parent + rewire). |
| `max_tree_nodes`     | Hard cap; refine reports `Converged` at this limit.    |
| `map_pgm_path`       | P5 PGM occupancy grid (path relative to CWD).          |
| `map_resolution_m`   | Metres per grid cell.                                  |
| `map_origin_x` / `_y`| World-frame origin of grid cell (0, 0).                |
| `occupied_threshold` | Optional; defaults to 128.                             |

## Replay determinism test

```
cargo test -p cu-anytime-rrt-star --features determinism_ci
```

Records the pipeline twice against the same `CuRng` seed and mock clock,
then asserts the encoded copperlist streams are byte-identical. Because
`AnytimeOutput<PlannedPath>` is a normal `CuMsgPayload`, equal copperlist
bytes across the two runs prove the planner's paths replay deterministically.

The test uses `config/copperconfig_determinism.ron`, which is the same
graph but with `max_tree_nodes = 120` so each run finishes in fractions of
a second.
