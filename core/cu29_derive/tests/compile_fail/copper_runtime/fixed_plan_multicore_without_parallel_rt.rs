use cu29::prelude::*;
use cu29_derive::copper_runtime;

#[derive(Reflect)]
struct IntSource;

impl Freezable for IntSource {}

impl CuSrcTask for IntSource {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(i32);

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _ctx: &CuContext, output: &mut Self::Output<'_>) -> CuResult<()> {
        output.set_payload(7);
        Ok(())
    }
}

#[derive(Reflect)]
struct IntSink;

impl Freezable for IntSink {}

impl CuSinkTask for IntSink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(i32);

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _ctx: &CuContext, _input: &Self::Input<'_>) -> CuResult<()> {
        Ok(())
    }
}

// A plan with worker threads needs the lane executor, which the parallel-rt
// feature provides; without it the placement must not be silently dropped.
#[copper_runtime(config = "config/fixed_plan_multicore.ron")]
struct App {}

fn main() {}
