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

/// `tail` has no outgoing connection, so its output slot type is inferred
/// through `CuStatelessTask`.
#[derive(Reflect)]
struct Features {
    offset: i32,
}

impl Freezable for Features {}

impl CuStatelessTask for Features {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(i32);
    type Output<'m> = output_msg!(i32);

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self { offset: 1 })
    }

    fn start(&mut self, _ctx: &CuContext) -> CuResult<()> {
        self.offset = 2;
        Ok(())
    }

    fn preprocess(&self, _ctx: &CuContext) -> CuResult<()> {
        Ok(())
    }

    fn process(
        &self,
        _ctx: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        output.set_payload(input.payload().copied().unwrap_or_default() + self.offset);
        Ok(())
    }

    fn postprocess(&self, _ctx: &CuContext) -> CuResult<()> {
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

#[copper_runtime(
    config = "config/stateless_task_valid.ron",
    sim_mode = true,
    ignore_resources = true
)]
struct App {}

fn main() {}
