#![no_main]
//! Does `AprilTags::process` trust image geometry it did not validate?
//!
//! `CuImage::new` asserts `format.is_valid()` and that the buffer holds
//! `format.required_bytes()`. `CuImage`'s `Decode` impl (image.rs:202) performs
//! neither check, so a `CuImage` restored from a `.copper` log, a resim keyframe,
//! or a bridge payload can carry geometry that does not match its buffer.
//!
//! `cu_apriltag::image_from_cuimage` then hands `width`/`height`/`stride`
//! verbatim to the C detector along with a raw pointer to the buffer, and the C
//! code reads `height * stride` bytes. This target builds exactly that message
//! and runs it under AddressSanitizer.

use arbitrary::Arbitrary;
use cu_apriltag::AprilTags;
use cu_sensor_payloads::{CuImage, CuImageBufferFormat};
use cu29::prelude::*;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Scenario {
    declared_width: u8,
    declared_height: u8,
    declared_stride: u8,
    actual_pixels: Vec<u8>,
}

fuzz_target!(|scenario: Scenario| {
    let width = (scenario.declared_width as u32 % 96) + 8;
    let height = (scenario.declared_height as u32 % 96) + 8;
    let stride = width + (scenario.declared_stride as u32 % 16);
    if scenario.actual_pixels.len() > 16 * 1024 {
        return;
    }

    let cfg = cu29::bincode::config::standard();

    // Serialise a CuImage whose declared geometry does not match its buffer.
    // This is the exact field order CuImage::decode reads.
    let declared = CuImageBufferFormat {
        width,
        height,
        stride,
        pixel_format: *b"GRAY",
    };
    let handle = CuHandle::new_detached(scenario.actual_pixels);
    let Ok(bytes) = cu29::bincode::encode_to_vec((0u64, declared, handle), cfg) else {
        return;
    };

    // Decode it back the way the replay/bridge path does.
    let Ok((cuimage, _)) = cu29::bincode::decode_from_slice::<CuImage<Vec<u8>>, _>(&bytes, cfg)
    else {
        return;
    };

    // Sanity: the decode really did skip the checks `new` enforces.
    let buffer_len = cuimage.buffer_handle.with_inner(|i| i.len());
    let undersized = cuimage.format.required_bytes() > buffer_len;

    let mut config = ComponentConfig::default();
    config.set("family", "tag16h5".to_string());
    let Ok(mut task) = AprilTags::new(Some(&config), ()) else {
        return;
    };

    let input = CuMsg::<CuImage<Vec<u8>>>::new(Some(cuimage));
    let mut output = CuMsg::<cu_apriltag::AprilTagDetections>::default();
    let ctx = CuContext::new_with_clock();

    // If this reads past the buffer, ASan reports it here.
    task.process(&ctx, &input, &mut output)
        .expect("process must not fail");

    // ASan is the oracle for the read itself. `undersized` is kept only so the
    // fuzzer's coverage feedback distinguishes the two shapes.
    if undersized {
        assert!(buffer_len < usize::MAX);
    }
});
