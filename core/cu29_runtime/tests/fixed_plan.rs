//! Verify that runtime generation enforces saved order and preserves wiring.
#![cfg(all(feature = "std", not(feature = "parallel-rt")))]

use cu29::prelude::*;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

static SEQUENCE: AtomicU32 = AtomicU32::new(0);
static RESULT: AtomicU32 = AtomicU32::new(0);

#[derive(Reflect)]
struct Source;

impl Freezable for Source {}

impl CuSrcTask for Source {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(u32);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _: &CuContext, output: &mut Self::Output<'_>) -> CuResult<()> {
        output.set_payload(SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1);
        Ok(())
    }
}

#[derive(Reflect)]
struct Join;

impl Freezable for Join {}

impl CuTask for Join {
    type Resources<'r> = ();
    type Input<'m> = input_msg!('m, u32, u32);
    type Output<'m> = output_msg!(u32);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(
        &mut self,
        _: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        output.set_payload(input.0.payload().unwrap() * 10 + input.1.payload().unwrap());
        Ok(())
    }
}

#[derive(Reflect)]
struct Sink;

impl Freezable for Sink {}

impl CuSinkTask for Sink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u32);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _: &CuContext, input: &Self::Input<'_>) -> CuResult<()> {
        RESULT.store(*input.payload().unwrap(), Ordering::Relaxed);
        Ok(())
    }
}

#[copper_runtime(config = "tests/fixed_plan_config.ron")]
struct App {}

#[test]
fn generated_runtime_uses_fixed_order_and_original_input_positions() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::builder()
        .with_log_path(dir.path().join("fixed.copper"), Some(16 * 1024 * 1024))
        .unwrap()
        .build()
        .unwrap();
    let mut running = app.start().unwrap();
    running.run_one_iteration().unwrap();
    running.stop().unwrap();
    // Fixed runs right first, but join's RON input tuple remains (left, right).
    assert_eq!(RESULT.load(Ordering::Relaxed), 21);
}
