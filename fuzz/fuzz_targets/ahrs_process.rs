#![no_main]
//! Fuzz `CuAhrs::process` over arbitrary configs and IMU/magnetometer sample streams.
//!
//! Oracle: when the config and every sample are finite, the fused pose and the
//! internal filter state must stay finite. An AHRS that emits NaN poisons every
//! downstream controller, so NaN-out-of-finite-in is a real defect, not noise.

use arbitrary::Arbitrary;
use cu_ahrs::{AhrsPose, CuAhrs};
use cu_sensor_payloads::{ImuPayload, MagnetometerPayload};
use cu29::prelude::*;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum FuzzTov {
    None,
    Time(u64),
    Range(u64, u64),
}

impl FuzzTov {
    fn to_tov(&self) -> Tov {
        match self {
            FuzzTov::None => Tov::None,
            FuzzTov::Time(t) => Tov::Time(CuTime::from(*t)),
            FuzzTov::Range(a, b) => Tov::Range(CuTimeRange {
                start: CuTime::from(*a),
                end: CuTime::from(*b),
            }),
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Sample {
    accel: [f32; 3],
    gyro: [f32; 3],
    temperature: f32,
    mag: Option<[f32; 3]>,
    imu_present: bool,
    tov: FuzzTov,
}

#[derive(Arbitrary, Debug)]
struct Scenario {
    mahony_kp: Option<f64>,
    mahony_ki: Option<f64>,
    sample_hz: Option<f64>,
    samples: Vec<Sample>,
}

fn finite3(v: &[f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

fuzz_target!(|scenario: Scenario| {
    // Keep the run bounded so the fuzzer explores shapes, not sample counts.
    if scenario.samples.len() > 512 {
        return;
    }

    let mut config = ComponentConfig::default();
    let mut config_finite = true;
    if let Some(kp) = scenario.mahony_kp {
        config_finite &= kp.is_finite();
        config.set("mahony_kp", kp);
    }
    if let Some(ki) = scenario.mahony_ki {
        config_finite &= ki.is_finite();
        config.set("mahony_ki", ki);
    }
    if let Some(hz) = scenario.sample_hz {
        config_finite &= hz.is_finite();
        config.set("sample_hz", hz);
    }

    let Ok(mut task) = CuAhrs::new(Some(&config), ()) else {
        return;
    };
    let ctx = CuContext::new_with_clock();

    // Track whether everything fed so far was finite; only then do we demand
    // a finite output.
    let mut all_finite = config_finite;

    for sample in &scenario.samples {
        let tov = sample.tov.to_tov();

        let imu_payload = sample
            .imu_present
            .then(|| ImuPayload::from_raw(sample.accel, sample.gyro, sample.temperature));
        if sample.imu_present {
            all_finite &= finite3(&sample.accel) && finite3(&sample.gyro);
        }
        let mag_payload = sample.mag.map(|m| {
            all_finite &= finite3(&m);
            MagnetometerPayload::from_raw(m)
        });

        let mut imu_msg = CuMsg::new(imu_payload);
        imu_msg.tov = tov;
        let mut mag_msg = CuMsg::new(mag_payload);
        mag_msg.tov = tov;
        let mut output: CuMsg<AhrsPose> = CuMsg::new(None);

        let input = (&imu_msg, &mag_msg);
        task.process(&ctx, &input, &mut output)
            .expect("CuAhrs::process must not fail");

        // The debug-state projection is reachable from the remote debugger at any
        // time; it must never panic on live filter state.
        let state = task.debug_state();

        if all_finite {
            assert!(
                state.orientation_w.value.is_finite()
                    && state.orientation_i.value.is_finite()
                    && state.orientation_j.value.is_finite()
                    && state.orientation_k.value.is_finite(),
                "filter orientation went non-finite on finite input: {state:?}"
            );
            assert!(
                state.gyro_bias_x.value.is_finite()
                    && state.gyro_bias_y.value.is_finite()
                    && state.gyro_bias_z.value.is_finite(),
                "gyro bias went non-finite on finite input: {state:?}"
            );
            if let Some(pose) = output.payload() {
                assert!(
                    pose.roll.value.is_finite()
                        && pose.pitch.value.is_finite()
                        && pose.yaw.value.is_finite(),
                    "pose went non-finite on finite input: {pose:?}"
                );
            }
        }
    }
});
