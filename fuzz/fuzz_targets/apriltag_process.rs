#![no_main]
//! Fuzz `AprilTags::process` with well-formed images of arbitrary geometry and content.
//!
//! Images here always satisfy `CuImage::new`'s asserts (stride >= width, buffer big
//! enough), so this target explores the detector and the pose/array bookkeeping
//! rather than the buffer-bounds question. The bounds question is
//! `apriltag_image_decode`.

use arbitrary::Arbitrary;
use cu_apriltag::AprilTags;
use cu_sensor_payloads::{CuImage, CuImageBufferFormat};
use cu29::prelude::*;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Scenario {
    width: u8,
    height: u8,
    extra_stride: u8,
    pixels: Vec<u8>,
    family_idx: u8,
    bits_corrected: u8,
    tag_size: f64,
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
}

const FAMILIES: [&str; 6] = [
    "tag16h5",
    "tag25h9",
    "tag36h11",
    "tagCircle21h7",
    "tagStandard41h12",
    "tagCustom48h12",
];

fuzz_target!(|scenario: Scenario| {
    // Bound the detector's work: it is O(width * height) with a large constant.
    let width = (scenario.width as u32 % 96) + 8;
    let height = (scenario.height as u32 % 96) + 8;
    let stride = width + scenario.extra_stride as u32 % 16;

    let mut config = ComponentConfig::default();
    config.set(
        "family",
        FAMILIES[scenario.family_idx as usize % FAMILIES.len()].to_string(),
    );
    // Cap at 1: per finding 7, bits_corrected=3 costs gigabytes and seconds just
    // to build the detector, which would OOM-abort this target for reasons that
    // have nothing to do with the code under test. Finding 7 is measured by
    // `bits_corrected_probe`, not here.
    config.set("bits_corrected", (scenario.bits_corrected % 2) as u32);
    config.set("tag_size", scenario.tag_size);
    config.set("fx", scenario.fx);
    config.set("fy", scenario.fy);
    config.set("cx", scenario.cx);
    config.set("cy", scenario.cy);

    let Ok(mut task) = AprilTags::new(Some(&config), ()) else {
        return;
    };

    let format = CuImageBufferFormat {
        width,
        height,
        stride,
        pixel_format: *b"GRAY",
    };
    // Fill exactly what the format declares, cycling the fuzzer's bytes.
    let needed = format.required_bytes();
    let mut pixels = vec![0u8; needed];
    if !scenario.pixels.is_empty() {
        for (i, slot) in pixels.iter_mut().enumerate() {
            *slot = scenario.pixels[i % scenario.pixels.len()];
        }
    }

    let cuimage = CuImage::new(format, CuHandle::new_detached(pixels));
    let input = CuMsg::<CuImage<Vec<u8>>>::new(Some(cuimage));
    let mut output = CuMsg::<cu_apriltag::AprilTagDetections>::default();
    let ctx = CuContext::new_with_clock();

    task.process(&ctx, &input, &mut output)
        .expect("process must not fail");

    if let Some(detections) = output.payload() {
        assert!(detections.ids.0.len() <= 16, "ids exceeded MAX_DETECTIONS");
        assert_eq!(
            detections.ids.0.len(),
            detections.poses.0.len(),
            "ids and poses are out of sync"
        );
        assert_eq!(
            detections.ids.0.len(),
            detections.decision_margins.0.len(),
            "ids and decision_margins are out of sync"
        );
        for pose in detections.poses.0.iter() {
            let _ = format!("{pose:?}");
        }
    }
});
