//! The lane executor must record the same CopperLists as the serial executor.
#![cfg(all(feature = "std", feature = "parallel-rt"))]

use cu29::prelude::*;
use cu29_export::copperlists_reader;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// CopperLists each run records before it asks to stop.
const RECORDED: u64 = 200;

/// Every value that reached the sinks, per app, with its CopperList id.
static SINK: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static SINK2: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
/// Concurrent invocations of the stateless task, and the most seen at once.
static TRIPLE_ACTIVE: AtomicU64 = AtomicU64::new(0);
static TRIPLE_PEAK: AtomicU64 = AtomicU64::new(0);

#[derive(Reflect)]
pub struct Source {
    next: u64,
}

impl Freezable for Source {}

impl CuSrcTask for Source {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(u64);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self { next: 1 })
    }

    fn process(&mut self, _: &CuContext, output: &mut Self::Output<'_>) -> CuResult<()> {
        output.set_payload(self.next);
        self.next += 1;
        Ok(())
    }
}

#[derive(Reflect)]
pub struct RunningSum {
    total: u64,
}

impl Freezable for RunningSum {}

impl CuTask for RunningSum {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u64);
    type Output<'m> = output_msg!(u64);

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

#[derive(Reflect)]
pub struct Triple;

impl Freezable for Triple {}

impl CuStatelessTask for Triple {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u64);
    type Output<'m> = output_msg!(u64);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(
        &self,
        _: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        let active = TRIPLE_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        TRIPLE_PEAK.fetch_max(active, Ordering::SeqCst);
        // Long enough for the other worker's invocation to overlap.
        std::thread::sleep(std::time::Duration::from_micros(200));
        output.set_payload(input.payload().unwrap() * 3);
        TRIPLE_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Reflect)]
pub struct Join;

impl Freezable for Join {}

impl CuTask for Join {
    type Resources<'r> = ();
    type Input<'m> = input_msg!('m, u64, u64);
    type Output<'m> = output_msg!(u64);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(
        &mut self,
        _: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        output.set_payload(input.0.payload().unwrap() * 1000 + input.1.payload().unwrap());
        Ok(())
    }
}

#[derive(Reflect)]
pub struct Sink {
    stop_lane: bool,
    stop_after: u64,
    seen: u64,
}

impl Freezable for Sink {}

impl CuSinkTask for Sink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u64);

    fn new(config: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        let config = config.ok_or_else(|| CuError::from("sink needs a config"))?;
        Ok(Self {
            stop_lane: config.get::<String>("app")?.as_deref() == Some("lane"),
            stop_after: config.get::<u64>("stop_after")?.unwrap_or(u64::MAX),
            seen: 0,
        })
    }

    fn process(&mut self, ctx: &CuContext, input: &Self::Input<'_>) -> CuResult<()> {
        SINK.lock()
            .unwrap()
            .push((ctx.cl_id(), *input.payload().unwrap()));
        self.seen += 1;
        if self.seen == self.stop_after {
            if self.stop_lane {
                lane::request_stop();
            } else {
                serial::request_stop();
            }
        }
        Ok(())
    }
}

/// Runs on the background pool; the plan binds its result to the previous CL.
#[derive(Reflect)]
pub struct Offset;

impl Freezable for Offset {}

impl CuTask for Offset {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u64);
    type Output<'m> = output_msg!(u64);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(
        &mut self,
        _: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        output.set_payload(input.payload().unwrap() + 100);
        Ok(())
    }
}

#[derive(Reflect)]
pub struct Sink2;

impl Freezable for Sink2 {}

impl CuSinkTask for Sink2 {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(u64);

    fn new(_: Option<&ComponentConfig>, _: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self)
    }

    fn process(&mut self, ctx: &CuContext, input: &Self::Input<'_>) -> CuResult<()> {
        if let Some(value) = input.payload() {
            SINK2.lock().unwrap().push((ctx.cl_id(), *value));
        }
        Ok(())
    }
}

/// Runs one app until its sink asks to stop; returns what the sinks saw and
/// the payloads it recorded.
macro_rules! app_module {
    ($name:ident, $config:literal) => {
        mod $name {
            use super::*;

            #[copper_runtime(config = $config)]
            struct App {}

            pub fn request_stop() {
                App::request_stop();
            }

            #[allow(deprecated)] // `run()` until the sink stops it has no typed transition
            pub fn run(
                dir: &Path,
            ) -> (
                Vec<(u64, u64)>,
                Vec<(u64, u64)>,
                Vec<(u64, Vec<(String, String)>)>,
            ) {
                use cu29::prelude::app::CuApplication;
                SINK.lock().unwrap().clear();
                SINK2.lock().unwrap().clear();
                let log_base = dir.join(concat!(stringify!($name), ".copper"));
                let mut app = App::builder()
                    .with_log_path(&log_base, Some(16 * 1024 * 1024))
                    .unwrap()
                    .build()
                    .unwrap();
                app.start_all_tasks().unwrap();
                app.run().unwrap();
                app.stop_all_tasks().unwrap();
                drop(app);
                let sink = std::mem::take(&mut *SINK.lock().unwrap());
                let sink2 = std::mem::take(&mut *SINK2.lock().unwrap());
                let recorded = recorded_payloads::<default::CuStampedDataSet>(&log_base);
                (sink, sink2, recorded)
            }
        }
    };
}

app_module!(serial, "tests/lane_plan_serial.ron");
app_module!(lane, "tests/lane_plan_multicore.ron");

/// The payloads of the first `RECORDED` recorded CopperLists, keyed by the
/// task that produced each slot: the two plans lay their slots out in
/// different orders.
fn recorded_payloads<P>(log_base: &Path) -> Vec<(u64, Vec<(String, String)>)>
where
    P: CopperListTuple + 'static,
{
    let logger = UnifiedLoggerBuilder::new()
        .file_base_name(log_base)
        .build()
        .expect("open log");
    let UnifiedLogger::Read(logger) = logger else {
        panic!("expected a reader");
    };
    let mut reader = UnifiedLoggerIOReader::new(logger, UnifiedLogType::CopperList);
    let mut lists: Vec<(u64, Vec<(String, String)>)> = copperlists_reader::<P>(&mut reader)
        .map(|culist| {
            let mut slots: Vec<(String, String)> = culist
                .msgs
                .cumsgs()
                .iter()
                .zip(P::get_all_task_ids())
                .map(|(msg, origin)| {
                    let payload = msg
                        .payload()
                        .map(|p| ron::to_string(p).unwrap())
                        .unwrap_or_default();
                    (origin.to_string(), payload)
                })
                .collect();
            slots.sort();
            (culist.id, slots)
        })
        .collect();
    lists.sort_by_key(|(id, _)| *id);
    lists.truncate(RECORDED as usize);
    lists
}

#[test]
fn lane_executor_records_the_serial_copperlists() {
    let dir = tempfile::tempdir().unwrap();
    let (serial_sink, serial_sink2, serial) = serial::run(dir.path());
    let (lane_sink, lane_sink2, lane) = lane::run(dir.path());

    // sink(n) = (1 + .. + n) * 1000 + 3n, in CopperList order.
    assert_eq!(serial_sink.len() as u64, RECORDED);
    for (index, (clid, value)) in serial_sink.iter().enumerate() {
        let n = index as u64 + 1;
        assert_eq!(
            (*clid, *value),
            (index as u64, n * (n + 1) / 2 * 1000 + 3 * n)
        );
    }
    assert!(lane_sink.len() as u64 >= RECORDED);
    assert_eq!(&lane_sink[..RECORDED as usize], &serial_sink[..]);
    // sink2 sees the background result of the previous CopperList.
    assert_eq!(&serial_sink2[..3], &[(1, 101), (2, 102), (3, 103)]);
    assert_eq!(
        &lane_sink2[..RECORDED as usize - 1],
        &serial_sink2[..RECORDED as usize - 1]
    );
    assert!(
        TRIPLE_PEAK.load(Ordering::SeqCst) >= 2,
        "the stateless task's two workers never overlapped"
    );

    assert_eq!(serial.len() as u64, RECORDED);
    assert_eq!(serial, lane);
}
