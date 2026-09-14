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

/// Period gating for the timer roots. Copper has no per-task period, so the whole
/// multi-rate mechanism is confined here.
///
/// Clock-based pacing, equivalent to an rclcpp timer. The next deadline is rescheduled
/// as `due + period`, and resynced to `now + period` once a whole period has been
/// missed, so a lagging loop never fires a catch-up burst. Replay is therefore only
/// deterministic at CopperList granularity.
#[derive(Reflect)]
pub struct Pacer {
    period: CuDuration,
    due: Option<CuTime>,
}

impl Pacer {
    pub fn new(config: Option<&ComponentConfig>, task: &str) -> CuResult<Self> {
        Ok(Self {
            period: CuDuration::from_millis(cfg_u64(config, task, "period_ms")?),
            due: None,
        })
    }

    /// True on the CopperLists where the root is due to fire.
    pub fn fire(&mut self, now: CuTime) -> bool {
        let Some(due) = self.due else {
            self.due = Some(now + self.period);
            return true;
        };
        if now < due {
            return false;
        }
        let next = due + self.period;
        self.due = Some(if next > now { next } else { now + self.period });
        true
    }
}

impl Freezable for Pacer {
    fn freeze<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        Encode::encode(&self.due, encoder)
    }

    fn thaw<D: Decoder>(&mut self, decoder: &mut D) -> Result<(), DecodeError> {
        self.due = Decode::decode(decoder)?;
        Ok(())
    }
}

/// The runtime reuses the CopperList slots, so a suppressed output has to drop the
/// previous CopperList's time of validity along with its payload.
fn suppress(output: &mut CuMsg<AurMsg>) {
    output.clear_payload();
    output.tov = Tov::default();
}

/// A timer root: fires its sub-DAG on its own period and stamps the firing.
#[derive(Reflect)]
pub struct AurRoot {
    pacer: Pacer,
    replay: Replay,
    seq: u64,
}

impl Freezable for AurRoot {
    fn freeze<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        Encode::encode(&self.seq, encoder)?;
        self.pacer.freeze(encoder)
    }

    fn thaw<D: Decoder>(&mut self, decoder: &mut D) -> Result<(), DecodeError> {
        self.seq = Decode::decode(decoder)?;
        self.pacer.thaw(decoder)
    }
}

impl CuSrcTask for AurRoot {
    type Resources<'r> = ();
    type Output<'m> = output_msg!(AurMsg);

    fn new(config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self> {
        Ok(Self {
            pacer: Pacer::new(config, "AurRoot")?,
            replay: Replay::new(config, "AurRoot")?,
            seq: 0,
        })
    }

    fn process(&mut self, ctx: &CuContext, output: &mut Self::Output<'_>) -> CuResult<()> {
        let now = ctx.now();
        if !self.pacer.fire(now) {
            suppress(output);
            return Ok(());
        }
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

    fn config(entries: &[(&str, u64)]) -> ComponentConfig {
        let mut config = ComponentConfig::default();
        for (key, value) in entries {
            config.set(key, *value);
        }
        config
    }

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

    #[test]
    fn test_a_pacer_fires_on_its_first_call_then_on_its_period() {
        let mut pacer = Pacer::new(Some(&config(&[("period_ms", 200)])), "t").unwrap();
        let start = CuTime::from(0u64);
        assert!(pacer.fire(start));
        assert!(!pacer.fire(start + CuDuration::from_millis(199)));
        assert!(pacer.fire(start + CuDuration::from_millis(200)));
    }

    #[test]
    fn test_a_lagging_pacer_resyncs_instead_of_bursting() {
        let mut pacer = Pacer::new(Some(&config(&[("period_ms", 200)])), "t").unwrap();
        let start = CuTime::from(0u64);
        assert!(pacer.fire(start));
        // A whole period late: the next deadline is one period from now, not from `due`.
        assert!(pacer.fire(start + CuDuration::from_millis(900)));
        assert!(!pacer.fire(start + CuDuration::from_millis(1099)));
        assert!(pacer.fire(start + CuDuration::from_millis(1100)));
    }
}
