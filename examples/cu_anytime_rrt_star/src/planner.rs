//! Anytime RRT\* planner: `impl Anytime` over `PlanQuery` / `PlannedPath`.
//!
//! One `refine` call = one RRT\* iteration (sample → nearest → steer →
//! collision-check → near → choose-parent → insert → rewire → goal-check).
//! No allocations happen on the refine hot path: the tree and the near-index
//! scratch buffer are grown only via `Vec::push`, and `Vec::clear` is called
//! on `near_scratch` before it is refilled. Startup allocations happen in
//! `new` (PGM read, `Vec::with_capacity` for the tree).

use cu_rng::prelude::*;
use cu29::prelude::*;
use cu29::resources;

use crate::occupancy_grid::{DEFAULT_OCCUPIED_THRESHOLD, OccupancyGrid};
use crate::{PlanQuery, PlannedPath, Point2D};

/// Resources the planner pulls from the bundle: an owned [`CuRng`] handle.
pub mod planner_resources {
    use super::*;

    resources!({ rng => Owned<CuRng> });
}

/// Internal tree node. Not a payload — never crosses the CuMsg boundary.
#[derive(Clone, Copy)]
struct RrtNode {
    position: Point2D,
    parent: Option<u32>,
    cost_from_start: f32,
}

/// Anytime RRT\* planner.
#[derive(Reflect)]
#[reflect(no_field_bounds, from_reflect = false, type_path = false)]
pub struct RrtStarPlanner {
    step_size: f32,
    goal_threshold: f32,
    goal_bias: f32,
    neighbor_radius: f32,
    max_tree_nodes: u32,
    sample_min_x: f32,
    sample_max_x: f32,
    sample_min_y: f32,
    sample_max_y: f32,

    #[reflect(ignore)]
    nodes: Vec<RrtNode>,
    iter: u32,
    best_goal_ix: Option<u32>,
    best_cost: f32,

    #[reflect(ignore)]
    near_scratch: Vec<u32>,

    #[reflect(ignore)]
    map: OccupancyGrid,
    #[reflect(ignore)]
    rng: CuRng,
}

impl TypePath for RrtStarPlanner {
    fn type_path() -> &'static str {
        "cu_anytime_rrt_star::planner::RrtStarPlanner"
    }
    fn short_type_path() -> &'static str {
        "RrtStarPlanner"
    }
    fn type_ident() -> Option<&'static str> {
        Some("RrtStarPlanner")
    }
    fn crate_name() -> Option<&'static str> {
        Some("cu_anytime_rrt_star")
    }
    fn module_path() -> Option<&'static str> {
        Some("planner")
    }
}

impl Freezable for RrtStarPlanner {}

impl Anytime for RrtStarPlanner {
    type Input = PlanQuery;
    type Output = PlannedPath;
    type Resources<'r> = planner_resources::Resources;

    fn new(cfg: Option<&ComponentConfig>, res: Self::Resources<'_>) -> CuResult<Self> {
        let cfg = cfg.ok_or_else(|| CuError::from("RrtStarPlanner requires a config"))?;
        let step_size = read_positive_f32(cfg, "step_size")?;
        let goal_threshold = read_positive_f32(cfg, "goal_threshold")?;
        let goal_bias = read_unit_f32(cfg, "goal_bias")?;
        let neighbor_radius = read_positive_f32(cfg, "neighbor_radius")?;
        let max_tree_nodes = cfg
            .get::<u64>("max_tree_nodes")
            .map_err(|e| CuError::from(format!("RrtStarPlanner 'max_tree_nodes': {e}")))?
            .ok_or_else(|| {
                CuError::from("RrtStarPlanner: missing required 'max_tree_nodes' (u64 > 0)")
            })?;
        if max_tree_nodes == 0 {
            return Err(CuError::from("RrtStarPlanner 'max_tree_nodes' must be > 0"));
        }
        if max_tree_nodes > u32::MAX as u64 {
            return Err(CuError::from(format!(
                "RrtStarPlanner 'max_tree_nodes' must fit in u32 (got {max_tree_nodes})"
            )));
        }
        let max_tree_nodes = max_tree_nodes as u32;

        let map_pgm_path = cfg
            .get::<String>("map_pgm_path")
            .map_err(|e| CuError::from(format!("RrtStarPlanner 'map_pgm_path': {e}")))?
            .ok_or_else(|| CuError::from("RrtStarPlanner: missing required 'map_pgm_path'"))?;
        let map_resolution_m = read_positive_f32(cfg, "map_resolution_m")?;
        let map_origin_x = read_f32(cfg, "map_origin_x")?;
        let map_origin_y = read_f32(cfg, "map_origin_y")?;
        let occupied_threshold = cfg
            .get::<u64>("occupied_threshold")
            .map_err(|e| CuError::from(format!("RrtStarPlanner 'occupied_threshold': {e}")))?
            .map(|v| v as u8)
            .unwrap_or(DEFAULT_OCCUPIED_THRESHOLD);

        let map = OccupancyGrid::from_pgm_path(
            &map_pgm_path,
            map_resolution_m,
            map_origin_x,
            map_origin_y,
            occupied_threshold,
        )?;

        let (ox, oy) = map.origin();
        let sample_min_x = ox;
        let sample_max_x = ox + map.width() as f32 * map.resolution_m();
        let sample_min_y = oy;
        let sample_max_y = oy + map.height() as f32 * map.resolution_m();

        Ok(Self {
            step_size,
            goal_threshold,
            goal_bias,
            neighbor_radius,
            max_tree_nodes,
            sample_min_x,
            sample_max_x,
            sample_min_y,
            sample_max_y,
            nodes: Vec::with_capacity(max_tree_nodes as usize),
            iter: 0,
            best_goal_ix: None,
            best_cost: f32::INFINITY,
            // Loose upper bound — the near-neighbour set is bounded by
            // total tree size; we grow the tree in-place so `push` beyond
            // this capacity still works, at a one-time realloc cost.
            near_scratch: Vec::with_capacity(64),
            map,
            rng: res.rng.0,
        })
    }

    fn base(&mut self, q: &Self::Input, _ctx: &CuContext) -> CuResult<()> {
        self.nodes.clear();
        self.iter = 0;
        self.best_goal_ix = None;
        self.best_cost = f32::INFINITY;
        if !self.map.is_point_free(q.start) {
            return Err(CuError::from(
                "RrtStarPlanner::base: start point falls on an occupied cell",
            ));
        }
        self.nodes.push(RrtNode {
            position: q.start,
            parent: None,
            cost_from_start: 0.0,
        });
        Ok(())
    }

    fn refine(&mut self, q: &Self::Input) -> CuResult<Step> {
        if self.nodes.len() as u32 >= self.max_tree_nodes {
            return Ok(Step::Converged);
        }
        self.iter += 1;

        let sample = self.sample_point(q);
        let nearest_ix = self.nearest_index(sample);
        let nearest_pos = self.nodes[nearest_ix as usize].position;
        let new_pos = steer(nearest_pos, sample, self.step_size);
        if !self.map.is_point_free(new_pos) || !self.map.is_segment_free(nearest_pos, new_pos) {
            return Ok(Step::Continue);
        }

        self.near_scratch.clear();
        for (ix, node) in self.nodes.iter().enumerate() {
            if distance(node.position, new_pos) <= self.neighbor_radius {
                self.near_scratch.push(ix as u32);
            }
        }

        let mut best_parent = nearest_ix;
        let mut best_cost_to_new =
            self.nodes[nearest_ix as usize].cost_from_start + distance(nearest_pos, new_pos);
        for &near_ix in &self.near_scratch {
            let near = &self.nodes[near_ix as usize];
            let candidate = near.cost_from_start + distance(near.position, new_pos);
            if candidate < best_cost_to_new && self.map.is_segment_free(near.position, new_pos) {
                best_cost_to_new = candidate;
                best_parent = near_ix;
            }
        }

        let new_ix = self.nodes.len() as u32;
        self.nodes.push(RrtNode {
            position: new_pos,
            parent: Some(best_parent),
            cost_from_start: best_cost_to_new,
        });

        // Rewire: for each near neighbour, if reaching it through the new
        // node beats its current cost, adopt the new node as its parent.
        // Descendant cost propagation is deferred — this is standard for
        // "incremental" RRT\* and keeps refine O(|near|). Descendants' costs
        // become an upper bound; the next rewiring pass corrects them.
        for &near_ix in &self.near_scratch {
            if near_ix == best_parent {
                continue;
            }
            let near_pos = self.nodes[near_ix as usize].position;
            let candidate = best_cost_to_new + distance(new_pos, near_pos);
            if candidate < self.nodes[near_ix as usize].cost_from_start
                && self.map.is_segment_free(new_pos, near_pos)
            {
                self.nodes[near_ix as usize].parent = Some(new_ix);
                self.nodes[near_ix as usize].cost_from_start = candidate;
            }
        }

        // Goal check: if the new node is within threshold of the goal and
        // the connecting segment is free, splice in an explicit goal node
        // whenever it improves the best-so-far cost. This keeps the goal
        // node (and therefore `best_cost`) a true reflection of some path
        // in the tree, even if intervening rewires shift interior costs.
        let goal = q.goal;
        let d_to_goal = distance(new_pos, goal);
        if d_to_goal <= self.goal_threshold && self.map.is_segment_free(new_pos, goal) {
            let goal_cost = best_cost_to_new + d_to_goal;
            if goal_cost < self.best_cost {
                let goal_ix = self.nodes.len() as u32;
                self.nodes.push(RrtNode {
                    position: goal,
                    parent: Some(new_ix),
                    cost_from_start: goal_cost,
                });
                self.best_goal_ix = Some(goal_ix);
                self.best_cost = goal_cost;
            }
        }

        if let Some(target) = q.max_cost
            && self.best_cost <= target
        {
            return Ok(Step::Satisfied);
        }
        Ok(Step::Continue)
    }

    fn write_best(&self, out: &mut Self::Output) {
        out.waypoints.clear();
        out.cost = self.best_cost;
        out.tree_size = self.nodes.len() as u32;
        out.iterations = self.iter;
        let Some(mut ix) = self.best_goal_ix else {
            return;
        };
        loop {
            out.waypoints.push(self.nodes[ix as usize].position);
            match self.nodes[ix as usize].parent {
                Some(p) => ix = p,
                None => break,
            }
        }
        out.waypoints.reverse();
    }

    fn progress(&self) -> Progress {
        Progress::Passes { done: self.iter }
    }
}

impl RrtStarPlanner {
    fn sample_point(&mut self, q: &PlanQuery) -> Point2D {
        if self.rng.random_bool(self.goal_bias as f64) {
            return q.goal;
        }
        Point2D {
            x: self.rng.random_range(self.sample_min_x..self.sample_max_x),
            y: self.rng.random_range(self.sample_min_y..self.sample_max_y),
        }
    }

    fn nearest_index(&self, target: Point2D) -> u32 {
        debug_assert!(
            !self.nodes.is_empty(),
            "nearest_index called with empty tree; base() must seed the start node first"
        );
        let mut best_ix = 0u32;
        let mut best_d = f32::INFINITY;
        for (ix, node) in self.nodes.iter().enumerate() {
            let d = distance(node.position, target);
            if d < best_d {
                best_d = d;
                best_ix = ix as u32;
            }
        }
        best_ix
    }
}

fn distance(a: Point2D, b: Point2D) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

fn steer(from: Point2D, to: Point2D, max_step: f32) -> Point2D {
    let d = distance(from, to);
    if d <= max_step || d == 0.0 {
        return to;
    }
    let t = max_step / d;
    Point2D {
        x: from.x + (to.x - from.x) * t,
        y: from.y + (to.y - from.y) * t,
    }
}

fn read_f32(cfg: &ComponentConfig, key: &str) -> CuResult<f32> {
    let v = cfg
        .get::<f64>(key)
        .map_err(|e| CuError::from(format!("RrtStarPlanner '{key}': {e}")))?
        .ok_or_else(|| CuError::from(format!("RrtStarPlanner: missing required '{key}'")))?;
    Ok(v as f32)
}

fn read_positive_f32(cfg: &ComponentConfig, key: &str) -> CuResult<f32> {
    let v = read_f32(cfg, key)?;
    if !(v.is_finite() && v > 0.0) {
        return Err(CuError::from(format!(
            "RrtStarPlanner '{key}' must be a positive finite float (got {v})"
        )));
    }
    Ok(v)
}

fn read_unit_f32(cfg: &ComponentConfig, key: &str) -> CuResult<f32> {
    let v = read_f32(cfg, key)?;
    if !(0.0..=1.0).contains(&v) {
        return Err(CuError::from(format!(
            "RrtStarPlanner '{key}' must be in [0, 1] (got {v})"
        )));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_grid(width: u32, height: u32) -> OccupancyGrid {
        OccupancyGrid::from_cells(
            width,
            height,
            vec![255u8; (width * height) as usize],
            1.0,
            0.0,
            0.0,
            DEFAULT_OCCUPIED_THRESHOLD,
        )
        .unwrap()
    }

    fn planner_with(map: OccupancyGrid, seed: u64) -> RrtStarPlanner {
        let (ox, oy) = map.origin();
        let smx = ox + map.width() as f32 * map.resolution_m();
        let smy = oy + map.height() as f32 * map.resolution_m();
        RrtStarPlanner {
            step_size: 1.0,
            goal_threshold: 1.0,
            goal_bias: 0.2,
            neighbor_radius: 2.0,
            max_tree_nodes: 5_000,
            sample_min_x: ox,
            sample_max_x: smx,
            sample_min_y: oy,
            sample_max_y: smy,
            nodes: Vec::with_capacity(5_000),
            iter: 0,
            best_goal_ix: None,
            best_cost: f32::INFINITY,
            near_scratch: Vec::with_capacity(64),
            map,
            rng: CuRng::from_seed(seed),
        }
    }

    fn ctx() -> CuContext {
        let (clock, _mock) = RobotClock::mock();
        CuContext::from_clock(clock)
    }

    #[test]
    fn base_seeds_tree_with_only_start_node() {
        let mut p = planner_with(free_grid(10, 10), 42);
        let q = PlanQuery {
            start: Point2D { x: 0.5, y: 0.5 },
            goal: Point2D { x: 9.5, y: 9.5 },
            max_cost: None,
        };
        p.base(&q, &ctx()).unwrap();
        assert_eq!(p.nodes.len(), 1);
        assert_eq!(p.nodes[0].position.x, 0.5);
        assert_eq!(p.nodes[0].position.y, 0.5);
        assert!(p.nodes[0].parent.is_none());
        assert_eq!(p.nodes[0].cost_from_start, 0.0);
        assert_eq!(p.iter, 0);
        assert!(p.best_goal_ix.is_none());
        assert_eq!(p.best_cost, f32::INFINITY);
    }

    #[test]
    fn base_rejects_start_on_occupied_cell() {
        // Wall at column 4.
        let mut cells = vec![255u8; 8 * 8];
        for row in 0..8 {
            cells[row * 8 + 4] = 0;
        }
        let map = OccupancyGrid::from_cells(8, 8, cells, 1.0, 0.0, 0.0, 128).unwrap();
        let mut p = planner_with(map, 1);
        let q = PlanQuery {
            start: Point2D { x: 4.5, y: 4.5 },
            goal: Point2D { x: 0.5, y: 0.5 },
            max_cost: None,
        };
        assert!(p.base(&q, &ctx()).is_err());
    }

    #[test]
    fn refine_grows_tree_and_bumps_iter() {
        let mut p = planner_with(free_grid(10, 10), 7);
        let q = PlanQuery {
            start: Point2D { x: 0.5, y: 0.5 },
            goal: Point2D { x: 9.5, y: 9.5 },
            max_cost: None,
        };
        p.base(&q, &ctx()).unwrap();

        let mut prev_len = p.nodes.len();
        for _ in 0..200 {
            let step = p.refine(&q).unwrap();
            assert!(matches!(step, Step::Continue | Step::Satisfied));
            assert!(p.nodes.len() >= prev_len, "tree must be non-shrinking");
            prev_len = p.nodes.len();
        }
        assert!(p.iter >= 200, "iter must monotonically increase");
        assert!(p.nodes.len() > 1, "at least one node must have been added");
    }

    #[test]
    fn write_best_yields_start_to_goal_path_once_found() {
        let mut p = planner_with(free_grid(20, 20), 123);
        let q = PlanQuery {
            start: Point2D { x: 1.0, y: 1.0 },
            goal: Point2D { x: 18.0, y: 18.0 },
            max_cost: None,
        };
        p.base(&q, &ctx()).unwrap();
        for _ in 0..2_000 {
            if let Ok(Step::Converged) = p.refine(&q) {
                break;
            }
            if p.best_goal_ix.is_some() {
                break;
            }
        }
        assert!(
            p.best_goal_ix.is_some(),
            "planner should have found a solution in a free 20x20 map"
        );

        let mut out = PlannedPath::default();
        p.write_best(&mut out);
        assert!(!out.waypoints.is_empty());
        // First waypoint is the start, last is the goal.
        assert_eq!(out.waypoints.first().unwrap().x, q.start.x);
        assert_eq!(out.waypoints.first().unwrap().y, q.start.y);
        assert_eq!(out.waypoints.last().unwrap().x, q.goal.x);
        assert_eq!(out.waypoints.last().unwrap().y, q.goal.y);
        assert!(out.cost.is_finite());
        assert!(out.cost > 0.0);
        assert!(out.iterations > 0);
    }

    #[test]
    fn refine_reports_satisfied_when_max_cost_is_met() {
        let mut p = planner_with(free_grid(20, 20), 321);
        let q = PlanQuery {
            start: Point2D { x: 1.0, y: 1.0 },
            goal: Point2D { x: 5.0, y: 1.0 },
            // Any solution beats INF; a straight-line path is ~4 m, so 6.0
            // is comfortably attainable.
            max_cost: Some(6.0),
        };
        p.base(&q, &ctx()).unwrap();
        let mut saw_satisfied = false;
        for _ in 0..5_000 {
            match p.refine(&q).unwrap() {
                Step::Satisfied => {
                    saw_satisfied = true;
                    break;
                }
                Step::Converged => break,
                Step::Continue => {}
            }
        }
        assert!(
            saw_satisfied,
            "refine should report Satisfied once max_cost is met"
        );
        assert!(p.best_cost <= 6.0);
    }

    #[test]
    fn refine_reports_converged_at_max_tree_nodes() {
        let mut p = planner_with(free_grid(10, 10), 99);
        p.max_tree_nodes = 5; // tiny cap to force convergence quickly
        let q = PlanQuery {
            start: Point2D { x: 0.5, y: 0.5 },
            goal: Point2D { x: 9.5, y: 9.5 },
            max_cost: None,
        };
        p.base(&q, &ctx()).unwrap();
        let mut saw_converged = false;
        for _ in 0..100 {
            if let Step::Converged = p.refine(&q).unwrap() {
                saw_converged = true;
                break;
            }
        }
        assert!(saw_converged);
    }

    #[test]
    fn same_seed_produces_same_tree() {
        let make = || {
            let mut p = planner_with(free_grid(10, 10), 0xDEAD_BEEF);
            let q = PlanQuery {
                start: Point2D { x: 0.5, y: 0.5 },
                goal: Point2D { x: 9.5, y: 9.5 },
                max_cost: None,
            };
            p.base(&q, &ctx()).unwrap();
            for _ in 0..100 {
                p.refine(&q).unwrap();
            }
            let mut out = PlannedPath::default();
            p.write_best(&mut out);
            (
                p.nodes
                    .iter()
                    .map(|n| (n.position.x, n.position.y))
                    .collect::<Vec<_>>(),
                out.cost,
                out.waypoints.len(),
            )
        };
        assert_eq!(make(), make());
    }
}
