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

// Selecting `kind: stateless_task` requires `CuStatelessTask`; an ordinary
// `CuTask` does not satisfy it.
#[derive(Reflect)]
struct OrderedFeatures;

impl Freezable for OrderedFeatures {}

impl CuTask for OrderedFeatures {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(i32);
    type Output<'m> = output_msg!(i32);

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(
        &mut self,
        _ctx: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        output.set_payload(input.payload().copied().unwrap_or_default());
        Ok(())
    }
}

#[copper_runtime(config = "config/stateless_kind_on_cutask.ron", sim_mode = true, ignore_resources = true)]
struct App {}

fn main() {}
