//! The four callback shapes of the Autoware Universe replica.
//!
//! Every node is parameterized from its RON `config:` block: `cost_index` selects the
//! recorded execution-time sequence it replays, `period_ms` the tick of the 11 timer
//! roots. Timing constants are load-bearing here, so a missing key is an error rather
//! than a default.

use crate::costs;
use crate::payload::AurMsg;
use bincode::de::Decoder;
use bincode::enc::Encoder;
use bincode::error::{DecodeError, EncodeError};
use bincode::{Decode, Encode};
use cu29::prelude::*;
use std::hint::black_box;

fn cfg_u64(config: Option<&ComponentConfig>, task: &str, key: &str) -> CuResult<u64> {
    config
        .map(|c| c.get::<u64>(key))
        .transpose()
        .map_err(|e| CuError::from(format!("{task}: config key '{key}': {e}")))?
        .flatten()
        .ok_or_else(|| CuError::from(format!("{task}: missing required config key '{key}'")))
}

/// The candidate window the prime test cycles through. It sits high and is narrow on
/// purpose: every candidate in it has 242 to 254 trial divisors, so the outer step's cost
/// (a square root and two increments) is amortized over about the same number of units
/// wherever a call starts. A window starting at 9 makes the first thousand units of every
/// call a third more expensive than the rest, which is a nonlinearity `k_ns_per_unit`
/// cannot express.
const CRUNCH_FIRST: u64 = 60_000;
const CRUNCH_WINDOW: u64 = 1 << 16;

/// The reference system's prime test, charged by the trial division rather than by the
/// bound.
///
/// `cu_autoware`'s `crunch(limit)` passes a bound to `number_cruncher` and costs
/// O(limit^1.5). The replay needs a cost linear in its argument — calibration fits one
/// `k_ns_per_unit` and charges a `t` ms callback `crunch(t / k)` — so the unit here is one
/// inner trial division and the candidate wraps inside a fixed window. The early exit on
/// a composite is dropped for the same reason: a unit has to cost the same every time.
///
/// Never inlined: the app would otherwise get one copy per call site and `calibrate`
/// another, and the same unit count then costs a few percent more in one binary than in
/// the other.
#[inline(never)]
pub fn crunch(units: u64) {
    let mut primes = 0u64;
    let mut candidate = CRUNCH_FIRST;
    let mut left = units;
    while left > 0 {
        let bound = (candidate as f64).sqrt() as u64;
        let mut is_prime = true;
        let mut divisor = 2;
        while divisor < bound && left > 0 {
            is_prime &= !candidate.is_multiple_of(divisor);
            divisor += 1;
            left -= 1;
        }
        primes += u64::from(is_prime);
        candidate += 1;
        if candidate >= CRUNCH_WINDOW {
            candidate = CRUNCH_FIRST;
        }
    }
    black_box(primes);
}

/// One callback's replay, resolved from the RON `cost_index` when the task is built.
#[derive(Clone, Copy, Reflect)]
pub struct Replay {
    cost_index: u32,
    #[reflect(ignore)]
    units: &'static [u64],
}

impl Replay {
    fn new(config: Option<&ComponentConfig>, task: &str) -> CuResult<Self> {
        let cost_index = u32::try_from(cfg_u64(config, task, "cost_index")?)
            .map_err(|_| CuError::from(format!("{task}: cost_index does not fit in u32")))?;
        Ok(Self {
            cost_index,
            units: &costs::table()?.sequence(cost_index)?.units,
        })
    }

    /// Runs the sample the `seq`-th firing replays, wrapping at the end of the sequence.
    #[inline]
    fn fire(&self, seq: u64) {
        let index = (seq.saturating_sub(1) % self.units.len() as u64) as usize;
        crunch(self.units[index]);
    }
}

/// One CopperList of the grid `runtime.rate_target_hz` sets, in milliseconds.
pub const GRID_MS: u64 = 5;

/// Period gating for the timer roots, on the CopperList grid rather than on the clock.
///
/// A root fires on the first CopperList of each period window, so which CopperLists
/// carry a firing is a function of the CopperList id alone: a serial run and a multicore
/// run of the same graph see the same inputs in the same CopperLists. Periods that are
/// not a multiple of the grid alternate window lengths deterministically — 33ms gives
/// 7 and 6 CopperList gaps.
#[inline]
pub fn fires_on(cl_id: u64, period_ms: u64) -> bool {
    (cl_id * GRID_MS) % period_ms < GRID_MS
}

/// The runtime reuses the CopperList slots, so a suppressed output has to drop the
/// previous CopperList's time of validity along with its payload.
fn suppress(output: &mut CuMsg<AurMsg>) {
    output.clear_payload();
    output.tov = Tov::default();
}

/// A timer root: fires its sub-DAG on its own period and stamps the firing.
///
/// Its firing count is its whole state; whether a CopperList carries a firing follows
/// from that CopperList's id.
#[derive(Reflect)]
pub struct AurRoot {
    period_ms: u64,
    replay: Replay,
    seq: u64,
}

impl Freezable for AurRoot {
    fn freeze<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        Encode::encode(&self.seq, encoder)
    }

    fn thaw<D: Decoder>(&mut self, decoder: &mut D) -> Result<(), DecodeError> {
        self.seq = Decode::decode(decoder)?;
        Ok(())
    }
}

impl CuSrcTask for AurRoot {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(AurMsg);

    fn new(config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        let period_ms = cfg_u64(config, "AurRoot", "period_ms")?;
        if period_ms == 0 {
            return Err(CuError::from("AurRoot: period_ms must be positive"));
        }
        Ok(Self {
            period_ms,
            replay: Replay::new(config, "AurRoot")?,
            seq: 0,
        })
    }

    fn process(&mut self, ctx: &CuContext, output: &mut Self::Output<'_>) -> CuResult<()> {
        if !fires_on(ctx.cl_id(), self.period_ms) {
            suppress(output);
            return Ok(());
        }
        let now = ctx.now();
        self.seq += 1;
        self.replay.fire(self.seq);
        output.tov = Tov::Time(now);
        output.set_payload(AurMsg {
            seq: self.seq,
            root_ns: now.as_nanos(),
        });
        Ok(())
    }
}

/// One in, one out: replays its sample on a fresh input and forwards its stamps.
#[derive(Reflect)]
pub struct AurCallback {
    replay: Replay,
}

impl Freezable for AurCallback {}

impl CuStatelessTask for AurCallback {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(AurMsg);
    type Output<'m> = output_msg!(AurMsg);

    fn new(config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self {
            replay: Replay::new(config, "AurCallback")?,
        })
    }

    fn process(
        &self,
        _ctx: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        let Some(msg) = input.payload() else {
            suppress(output);
            return Ok(());
        };
        self.replay.fire(msg.seq);
        output.tov = input.tov;
        output.set_payload(msg.clone());
        Ok(())
    }
}

/// Two in, one out. The first connection in the RON is the designated trigger: it
/// decides whether the callback fires and whose stamps it forwards, as the dataset's
/// two-input perception callbacks do.
#[derive(Reflect)]
pub struct AurJoin {
    replay: Replay,
}

impl Freezable for AurJoin {}

impl CuStatelessTask for AurJoin {
    type Resources<'r> = ();
    type Input<'m> = input_msg!('m, AurMsg, AurMsg);
    type Output<'m> = output_msg!(AurMsg);

    fn new(config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self {
            replay: Replay::new(config, "AurJoin")?,
        })
    }

    fn process(
        &self,
        _ctx: &CuContext,
        input: &Self::Input<'_>,
        output: &mut Self::Output<'_>,
    ) -> CuResult<()> {
        let Some(msg) = input.0.payload() else {
            suppress(output);
            return Ok(());
        };
        self.replay.fire(msg.seq);
        output.tov = input.0.tov;
        output.set_payload(msg.clone());
        Ok(())
    }
}

/// A deadline sink: the end of one of the dataset's 18 chains.
#[derive(Reflect)]
pub struct AurSink {
    replay: Replay,
}

impl Freezable for AurSink {}

impl CuSinkTask for AurSink {
    type Resources<'r> = ();
    type Input<'m> = input_msg!(AurMsg);

    fn new(config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self {
            replay: Replay::new(config, "AurSink")?,
        })
    }

    fn process(&mut self, _ctx: &CuContext, input: &Self::Input<'_>) -> CuResult<()> {
        if let Some(msg) = input.payload() {
            self.replay.fire(msg.seq);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cost has to be proportional to the unit count, or one `k_ns_per_unit` cannot
    /// carry a replay spanning four orders of magnitude.
    #[test]
    fn test_the_crunch_is_linear_in_its_units() {
        let time = |units| {
            let start = std::time::Instant::now();
            crunch(units);
            start.elapsed().as_secs_f64()
        };
        crunch(10_000_000);
        let ratio = time(10_000_000) / time(1_000_000);
        assert!((5.0..20.0).contains(&ratio), "{ratio}");
    }

    fn firings(period_ms: u64, copperlists: u64) -> Vec<u64> {
        (0..copperlists)
            .filter(|cl_id| fires_on(*cl_id, period_ms))
            .collect()
    }

    #[test]
    fn test_a_root_fires_on_the_first_copperlist_of_each_period() {
        assert_eq!(firings(20, 20), [0, 4, 8, 12, 16]);
        assert_eq!(firings(100, 60), [0, 20, 40]);
        assert_eq!(firings(1000, 600), [0, 200, 400]);
    }

    /// A period that is not a multiple of the grid alternates window lengths, and does
    /// it the same way in every run.
    #[test]
    fn test_a_period_off_the_grid_alternates_deterministically() {
        assert_eq!(firings(33, 40), [0, 7, 14, 20, 27, 33]);
    }
}
