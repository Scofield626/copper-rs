#![no_main]
//! Fuzz `CuAhrs`'s `Freezable` implementation with attacker-controlled snapshot bytes.
//!
//! A `.copper` log or a resim keyframe is untrusted input from the runtime's point
//! of view. `thaw` must never panic, must leave the task in a usable state, and
//! re-freezing must be stable (freeze -> thaw -> freeze is idempotent).

use cu_ahrs::CuAhrs;
use cu_sensor_payloads::ImuPayload;
use cu29::bincode::de::{Decode, Decoder};
use cu29::bincode::error::DecodeError;
use cu29::prelude::*;
use libfuzzer_sys::fuzz_target;

/// Decode wrapper so we can drive `Freezable::thaw` through the normal bincode entry point.
struct ThawedAhrs(CuAhrs);

impl Decode<()> for ThawedAhrs {
    fn decode<D: Decoder<Context = ()>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let mut task = CuAhrs::new_filter();
        task.thaw(decoder)?;
        Ok(ThawedAhrs(task))
    }
}

fn freeze_bytes(task: &CuAhrs) -> Vec<u8> {
    cu29::bincode::encode_to_vec(BincodeAdapter(task), cu29::bincode::config::standard())
        .expect("freeze must not fail")
}

fuzz_target!(|data: &[u8]| {
    let cfg = cu29::bincode::config::standard();
    let Ok((ThawedAhrs(mut task), _)) =
        cu29::bincode::decode_from_slice::<ThawedAhrs, _>(data, cfg)
    else {
        return;
    };

    // Post-thaw invariants the thaw code claims to enforce by clamping.
    let state = task.debug_state();
    let period = state.sample_period.value;
    assert!(
        period.is_finite() && (1.0e-5..=1.0).contains(&period),
        "thaw left sample_period out of range: {period}"
    );
    assert!(
        state.mahony_kp.value.is_finite() && (0.0..=10.0).contains(&state.mahony_kp.value),
        "thaw left mahony_kp out of range: {}",
        state.mahony_kp.value
    );
    assert!(
        state.mahony_ki.value.is_finite() && (0.0..=10.0).contains(&state.mahony_ki.value),
        "thaw left mahony_ki out of range: {}",
        state.mahony_ki.value
    );
    assert!(
        state.orientation_w.value.is_finite()
            && state.orientation_i.value.is_finite()
            && state.orientation_j.value.is_finite()
            && state.orientation_k.value.is_finite(),
        "thaw left a non-finite orientation: {state:?}"
    );
    assert!(
        state.gyro_bias_x.value.is_finite()
            && state.gyro_bias_y.value.is_finite()
            && state.gyro_bias_z.value.is_finite(),
        "thaw left a non-finite gyro bias: {state:?}"
    );
    // `UnitQuaternion` promises unit norm. `new_normalize` only delivers that if
    // the norm computation itself did not overflow.
    let norm_sq = state.orientation_w.value * state.orientation_w.value
        + state.orientation_i.value * state.orientation_i.value
        + state.orientation_j.value * state.orientation_j.value
        + state.orientation_k.value * state.orientation_k.value;
    assert!(
        (norm_sq - 1.0).abs() < 1.0e-3,
        "thaw left a non-unit orientation (norm^2 = {norm_sq}): {state:?}"
    );

    // A thawed task must be immediately usable: a normal finite sample must not
    // produce a non-finite pose.
    let ctx = CuContext::new_with_clock();
    let payload = ImuPayload::from_raw([0.0, 0.0, 9.81], [0.0, 0.0, 0.0], 25.0);
    let mut imu_msg = CuMsg::new(Some(payload));
    imu_msg.tov = Tov::Time(CuTime::from(1_000_000u64));
    let mag_msg: CuMsg<cu_sensor_payloads::MagnetometerPayload> = CuMsg::new(None);
    let mut output = CuMsg::new(None);
    task.process(&ctx, &(&imu_msg, &mag_msg), &mut output)
        .expect("process after thaw must not fail");
    if let Some(pose) = output.payload() {
        assert!(
            pose.roll.value.is_finite()
                && pose.pitch.value.is_finite()
                && pose.yaw.value.is_finite(),
            "thawed task produced a non-finite pose: {pose:?}"
        );
    }

    // freeze -> thaw must round-trip: our own output has to be decodable, and the
    // state has to survive the trip (up to float renormalisation noise).
    let before = task.debug_state();
    let once = freeze_bytes(&task);
    let Ok((ThawedAhrs(again), _)) = cu29::bincode::decode_from_slice::<ThawedAhrs, _>(&once, cfg)
    else {
        panic!("freeze output must be thawable");
    };
    let after = again.debug_state();

    let close = |a: f32, b: f32| (a - b).abs() <= 1.0e-5 * (1.0 + a.abs().max(b.abs()));
    assert!(
        close(before.sample_period.value, after.sample_period.value)
            && close(before.mahony_kp.value, after.mahony_kp.value)
            && close(before.mahony_ki.value, after.mahony_ki.value)
            && close(before.gyro_bias_x.value, after.gyro_bias_x.value)
            && close(before.gyro_bias_y.value, after.gyro_bias_y.value)
            && close(before.gyro_bias_z.value, after.gyro_bias_z.value)
            && close(before.roll.value, after.roll.value)
            && close(before.pitch.value, after.pitch.value)
            && close(before.yaw.value, after.yaw.value),
        "freeze/thaw did not round-trip: {before:?} vs {after:?}"
    );
    assert_eq!(
        before.last_tov, after.last_tov,
        "freeze/thaw lost the last time-of-validity"
    );
});
