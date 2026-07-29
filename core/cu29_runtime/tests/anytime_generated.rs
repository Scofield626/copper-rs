#![cfg(all(test, feature = "std"))]

use cu29::cutask_anytime::{AnytimeStatus, CuAnytimeTask};
use cu29::prelude::copper_runtime;
use cu29::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

static BASE_CALLS: AtomicUsize = AtomicUsize::new(0);
static REFINE_CALLS: AtomicUsize = AtomicUsize::new(0);
static SINK_CALLS: AtomicUsize = AtomicUsize::new(0);
static FIRST_PAYLOAD: AtomicU32 = AtomicU32::new(u32::MAX);
static FIRST_STOPPED_AT_MAX: AtomicBool = AtomicBool::new(false);
static SECOND_HAS_PAYLOAD: AtomicBool = AtomicBool::new(true);
static SECOND_SKIPPED_STALE: AtomicBool = AtomicBool::new(false);

#[derive(Reflect)]
struct RangeSource {
    iteration: usize,
}

impl Freezable for RangeSource {}

impl CuSrcTask for RangeSource {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(u32);

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self { iteration: 0 })
    }

    fn process(&mut self, ctx: &CuContext, output: &mut Self::Output<'_>) -> CuResult<()> {
        let now = ctx.clock.now();
        let start = if self.iteration == 0 {
            now - CuDuration::from_millis(20)
        } else {
            CuTime::default()
        };
        self.iteration += 1;

        output.set_payload(1);
        output.tov = Tov::Range(CuTimeRange { start, end: now });
        Ok(())
    }
}

#[derive(Reflect)]
struct Refiner;

impl Freezable for Refiner {}

impl CuAnytimeTask for Refiner {
    type Input<'m> = input_msg!(u32);
    type Output<'m> = output_msg!(u32);
    type Resources<'r> = ();
    type Quality = ();

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn base(
        &mut self,
        _ctx: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<AnytimeStatus<()>> {
        BASE_CALLS.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input.payload(), Some(&1));
        output.set_payload(0);
        Ok(AnytimeStatus::Improved(()))
    }

    fn refine(
        &mut self,
        _ctx: &CuContext,
        output: &mut Self::Output<'_>,
    ) -> CuResult<AnytimeStatus<()>> {
        REFINE_CALLS.fetch_add(1, Ordering::SeqCst);
        let next = output.payload().copied().unwrap_or_default() + 1;
        output.set_payload(next);
        Ok(AnytimeStatus::Improved(()))
    }
}

#[derive(Reflect)]
struct RecordingSink;

impl Freezable for RecordingSink {}

impl CuSinkTask for RecordingSink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u32);

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, _ctx: &CuContext, input: &Self::Input<'_>) -> CuResult<()> {
        let call = SINK_CALLS.fetch_add(1, Ordering::SeqCst);
        match call {
            0 => {
                FIRST_PAYLOAD.store(
                    input.payload().copied().unwrap_or(u32::MAX),
                    Ordering::SeqCst,
                );
                FIRST_STOPPED_AT_MAX.store(
                    input.metadata.status_txt.0.as_str() == "any:3it max",
                    Ordering::SeqCst,
                );
            }
            1 => {
                SECOND_HAS_PAYLOAD.store(input.payload().is_some(), Ordering::SeqCst);
                SECOND_SKIPPED_STALE.store(
                    input.metadata.status_txt.0.as_str() == "any:0it stale!",
                    Ordering::SeqCst,
                );
            }
            _ => panic!("unexpected sink invocation {call}"),
        }
        Ok(())
    }
}

#[copper_runtime(config = "tests/anytime_generated_config.ron")]
struct AnytimeGeneratedApp {}

#[test]
fn generated_anytime_runtime_refines_and_uses_earliest_range_tov() -> CuResult<()> {
    BASE_CALLS.store(0, Ordering::SeqCst);
    REFINE_CALLS.store(0, Ordering::SeqCst);
    SINK_CALLS.store(0, Ordering::SeqCst);
    FIRST_PAYLOAD.store(u32::MAX, Ordering::SeqCst);
    FIRST_STOPPED_AT_MAX.store(false, Ordering::SeqCst);
    SECOND_HAS_PAYLOAD.store(true, Ordering::SeqCst);
    SECOND_SKIPPED_STALE.store(false, Ordering::SeqCst);

    let (clock, clock_mock) = RobotClock::mock();
    let mut app = AnytimeGeneratedApp::builder().with_clock(clock).build()?;

    app.start_all_tasks()?;

    clock_mock.set_value(CuDuration::from_millis(40).as_nanos());
    app.run_one_iteration()?;

    // The range's end is current, but its earliest Tov is 100 ms old and
    // therefore exceeds max_age_ms.
    clock_mock.set_value(CuDuration::from_millis(100).as_nanos());
    app.run_one_iteration()?;

    app.stop_all_tasks()?;

    assert_eq!(BASE_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(REFINE_CALLS.load(Ordering::SeqCst), 3);
    assert_eq!(SINK_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(FIRST_PAYLOAD.load(Ordering::SeqCst), 3);
    assert!(FIRST_STOPPED_AT_MAX.load(Ordering::SeqCst));
    assert!(!SECOND_HAS_PAYLOAD.load(Ordering::SeqCst));
    assert!(SECOND_SKIPPED_STALE.load(Ordering::SeqCst));
    Ok(())
}
