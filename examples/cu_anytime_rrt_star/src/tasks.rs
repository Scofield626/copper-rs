//! Scaffold source/sink stubs. The real query source and path sink land with
//! step 5 of the delivery plan.

use cu29::prelude::*;

use crate::PlanQuery;

/// Placeholder source emitting a default [`PlanQuery`] every cycle.
#[derive(Default, Reflect)]
pub struct QuerySource;

impl Freezable for QuerySource {}

impl CuSrcTask for QuerySource {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(PlanQuery);

    fn new(_cfg: Option<&ComponentConfig>, _res: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _ctx: &CuContext, new_msg: &mut Self::Output<'_>) -> CuResult<()> {
        new_msg.set_payload(PlanQuery::default());
        Ok(())
    }
}

/// Placeholder sink that drops incoming queries. Kept trivial so the scaffold
/// example builds against a valid graph before the planner is wired in.
#[derive(Default, Reflect)]
pub struct PathSink;

impl Freezable for PathSink {}

impl CuSinkTask for PathSink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(PlanQuery);

    fn new(_cfg: Option<&ComponentConfig>, _res: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _ctx: &CuContext, _input: &Self::Input<'_>) -> CuResult<()> {
        Ok(())
    }
}
