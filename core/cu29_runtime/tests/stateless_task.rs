//! Verify that `kind: stateless_task` generates calls through `CuStatelessTask`.
#![cfg(feature = "std")]

use cu29::prelude::*;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

static SEQUENCE: AtomicU32 = AtomicU32::new(0);
static FEATURE_HOOKS: AtomicU32 = AtomicU32::new(0);
static RESULTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

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

/// Ordered state: a running total over CopperLists.
#[derive(Reflect)]
struct Filter {
    total: u32,
}

impl Freezable for Filter {}

impl CuTask for Filter {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u32);
    type Output<'m> = output_msg!(u32);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self { total: 0 })
    }

    fn process(
        &mut self,
        _: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        self.total += input.payload().unwrap();
        output.set_payload(self.total);
        Ok(())
    }
}

/// Independent per-CopperList work using immutable configuration.
#[derive(Reflect)]
struct Features {
    scale: u32,
    started: bool,
}

impl Freezable for Features {}

impl CuStatelessTask for Features {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u32);
    type Output<'m> = output_msg!(u32);

    fn new(config: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        let scale = config
            .and_then(|config| config.get::<u32>("scale").transpose())
            .transpose()?
            .ok_or_else(|| CuError::from("features needs a scale"))?;
        Ok(Self {
            scale,
            started: false,
        })
    }

    fn start(&mut self, _: &CuContext) -> CuResult<()> {
        self.started = true;
        Ok(())
    }

    fn preprocess(&self, _: &CuContext) -> CuResult<()> {
        FEATURE_HOOKS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn process(
        &self,
        _: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        assert!(self.started, "start must run before process");
        output.set_payload(input.payload().unwrap() * self.scale);
        Ok(())
    }

    fn postprocess(&self, _: &CuContext) -> CuResult<()> {
        FEATURE_HOOKS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Reflect)]
struct Fuse;

impl Freezable for Fuse {}

impl CuTask for Fuse {
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
        output.set_payload(input.0.payload().unwrap() + input.1.payload().unwrap());
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
        RESULTS.lock().unwrap().push(*input.payload().unwrap());
        Ok(())
    }
}

#[copper_runtime(config = "tests/stateless_task_config.ron")]
struct App {}

#[test]
fn stateless_task_runs_through_shared_calls_and_joins_its_own_copperlist() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::builder()
        .with_log_path(dir.path().join("stateless.copper"), Some(16 * 1024 * 1024))
        .unwrap()
        .build()
        .unwrap();
    let mut running = app.start().unwrap();
    for _ in 0..3 {
        running.run_one_iteration().unwrap();
    }
    running.stop().unwrap();
    // fuse(CL n) = running total of 1..=n + n * scale.
    assert_eq!(*RESULTS.lock().unwrap(), [101, 203, 306]);
    assert_eq!(FEATURE_HOOKS.load(Ordering::Relaxed), 6);
}
