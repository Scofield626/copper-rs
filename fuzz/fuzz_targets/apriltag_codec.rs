#![no_main]
//! Fuzz the `AprilTagDetections` codecs.
//!
//! `AprilTagDetections` has a hand-written `Decode`, a hand-written `Serialize`
//! and a hand-written `Deserialize`. All three are reachable from untrusted data
//! (a replayed `.copper` log, a bridge payload). They must reject bad input
//! instead of panicking, and must agree with each other on round-trip.

use cu_apriltag::AprilTagDetections;
use cu29::bincode::{Decode, Encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `CuArrayVec::decode` (cu29_runtime/src/payload.rs:143) decodes a full
    // `Vec<T>` before checking the `N` bound. Under the default byte-limited
    // config that finding stays out of the way; build with
    // `--features unbounded_decode` to reproduce it.
    #[cfg(feature = "unbounded_decode")]
    let cfg = cu29::bincode::config::standard();
    #[cfg(not(feature = "unbounded_decode"))]
    let cfg = cu29::bincode::config::standard().with_limit::<1_000_000>();

    // 1. bincode Decode from arbitrary bytes must not panic and must respect the
    //    MAX_DETECTIONS bound on every array.
    if let Ok((decoded, _)) = cu29::bincode::decode_from_slice::<AprilTagDetections, _>(data, cfg) {
        assert!(decoded.ids.0.len() <= 16, "ids exceeded MAX_DETECTIONS");
        assert!(decoded.poses.0.len() <= 16, "poses exceeded MAX_DETECTIONS");
        assert!(
            decoded.decision_margins.0.len() <= 16,
            "decision_margins exceeded MAX_DETECTIONS"
        );

        // The three arrays are parallel: entry i of each describes one detection.
        // Every consumer relies on it -- `filtered_by_decision_margin` zips them,
        // and `Serialize` declares `ids.len()` as the element count. `Decode`
        // reads them as three independent arrays and never checks.
        let (n_ids, n_poses, n_margins) = (
            decoded.ids.0.len(),
            decoded.poses.0.len(),
            decoded.decision_margins.0.len(),
        );
        assert!(
            n_ids == n_poses && n_poses == n_margins,
            "decode produced arrays of different lengths: \
             ids={n_ids} poses={n_poses} margins={n_margins}"
        );

        // 2. Re-encoding and decoding again must reproduce the same value.
        let bytes = cu29::bincode::encode_to_vec(&decoded, cfg).expect("encode must not fail");
        let (again, _) = cu29::bincode::decode_from_slice::<AprilTagDetections, _>(&bytes, cfg)
            .expect("our own encoding must decode");
        assert_eq!(decoded.ids.0, again.ids.0, "ids did not round-trip");
        assert_eq!(
            decoded.decision_margins.0.len(),
            again.decision_margins.0.len(),
            "decision_margins length did not round-trip"
        );

        // 3. The serde path must survive the same value. `Serialize` unwraps
        //    internally, so any serializer error there is a panic.
        let ser = cu29::bincode::serde::encode_to_vec(&decoded, cfg);
        if let Ok(ser_bytes) = ser {
            let _ =
                cu29::bincode::serde::decode_from_slice::<AprilTagDetections, _>(&ser_bytes, cfg);
        }
    }

    // 4. The serde Deserialize path fed directly with arbitrary bytes.
    let _ = cu29::bincode::serde::decode_from_slice::<AprilTagDetections, _>(data, cfg);

    // Keep the trait imports live for the no-decode path.
    let _ = (
        AprilTagDetections::default().ids.0.len(),
        std::mem::size_of::<AprilTagDetections>(),
    );
    fn _assert_traits<T: Encode + Decode<()>>() {}
    _assert_traits::<AprilTagDetections>();
});
