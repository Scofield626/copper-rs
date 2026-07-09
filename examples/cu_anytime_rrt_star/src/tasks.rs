//! Example source/sink around the anytime planner.
//!
//! `QuerySource` emits a fixed `(start, goal)` read from RON config once per
//! cycle. `PathSink` receives the planner's `AnytimeOutput<PlannedPath>` and
//! logs a one-line summary per cycle.

use cu29::prelude::*;

use crate::{PlanQuery, PlannedPath, Point2D};

/// Reads start/goal (and optional `max_cost`) from RON, emits them every cycle.
#[derive(Reflect)]
pub struct QuerySource {
    #[reflect(ignore)]
    query: PlanQuery,
}

impl Freezable for QuerySource {}

impl CuSrcTask for QuerySource {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(PlanQuery);

    fn new(cfg: Option<&ComponentConfig>, _res: Self::Resources<'_>) -> CuResult<Self> {
        let cfg = cfg.ok_or_else(|| CuError::from("QuerySource requires a config"))?;
        let start = Point2D {
            x: read_f32(cfg, "start_x")?,
            y: read_f32(cfg, "start_y")?,
        };
        let goal = Point2D {
            x: read_f32(cfg, "goal_x")?,
            y: read_f32(cfg, "goal_y")?,
        };
        let max_cost = cfg
            .get::<f64>("max_cost")
            .map_err(|e| CuError::from(format!("QuerySource 'max_cost': {e}")))?
            .map(|v| v as f32);
        Ok(Self {
            query: PlanQuery {
                start,
                goal,
                max_cost,
            },
        })
    }

    fn process(&mut self, ctx: &CuContext, new_msg: &mut Self::Output<'_>) -> CuResult<()> {
        new_msg.set_payload(self.query.clone());
        new_msg.tov = Tov::Time(ctx.now());
        Ok(())
    }
}

/// One-line-per-cycle summary sink for `AnytimeOutput<PlannedPath>`.
#[derive(Default, Reflect)]
pub struct PathSink;

impl Freezable for PathSink {}

impl CuSinkTask for PathSink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(AnytimeOutput<PlannedPath>);

    fn new(_cfg: Option<&ComponentConfig>, _res: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, ctx: &CuContext, input: &Self::Input<'_>) -> CuResult<()> {
        let Some(msg) = input.payload() else {
            return Ok(());
        };
        let path = &msg.value;
        let progress_tag = progress_tag(&msg.progress);
        info!(
            ctx,
            "planner: progress={} cost={} iter={} tree_size={} waypoints={}",
            progress_tag,
            path.cost,
            path.iterations,
            path.tree_size,
            path.waypoints.len() as u32
        );
        Ok(())
    }
}

fn progress_tag(p: &Progress) -> &'static str {
    match p {
        Progress::Passes { .. } => "passes",
        Progress::Converged => "converged",
        Progress::Satisfied => "satisfied",
        Progress::Skipped { reason } => match reason {
            SkipReason::NoInput => "skipped_no_input",
            SkipReason::ReusedLast => "skipped_reused_last",
        },
    }
}

fn read_f32(cfg: &ComponentConfig, key: &str) -> CuResult<f32> {
    let v = cfg
        .get::<f64>(key)
        .map_err(|e| CuError::from(format!("QuerySource '{key}': {e}")))?
        .ok_or_else(|| CuError::from(format!("QuerySource: missing required '{key}'")))?;
    Ok(v as f32)
}
