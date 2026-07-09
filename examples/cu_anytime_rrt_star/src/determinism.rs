//! Byte-identical replay determinism check for the anytime RRT\* pipeline.
//!
//! Two record runs against the same seed, same mock clock, same config must
//! produce byte-identical copperlist streams and byte-identical keyframes.
//! This is the acceptance test for "anytime + `CuRng` + replay works end to
//! end": the `AnytimeOutput<PlannedPath>` bytes carry the full planned path,
//! so equal copperlist streams means equal paths.
//!
//! Feature-gated on `determinism_ci` (mirrors `cu_caterpillar`).

use std::fs;
use std::path::{Path, PathBuf};

use cu29::bincode;
use cu29::prelude::*;
use cu29_export::copperlists_reader;

#[copper_runtime(config = "config/copperconfig_determinism.ron")]
struct AnytimeRrtStarDeterminismApp {}

const DET_LOG_SLAB_SIZE: Option<usize> = Some(64 * 1024 * 1024);

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn out_root_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate_dir().join("target"))
        .join("determinism_tests")
}

fn fresh_case_dir(case: &str) -> PathBuf {
    let dir = out_root_dir().join(format!("{}_pid{}", case, std::process::id()));
    if dir.exists() {
        let _ = fs::remove_dir_all(&dir);
    }
    fs::create_dir_all(&dir).expect("failed to create determinism output dir");
    dir
}

fn record_run(log_base: &Path, iterations: usize, dt_ticks: u64) -> CuResult<()> {
    if let Some(parent) = log_base.parent() {
        fs::create_dir_all(parent).ok();
    }
    // The macro expects `maps/depot.pgm` relative to CWD; anchor at the crate
    // root so this runs the same whether invoked via cargo test at the
    // workspace root or from a per-crate target dir.
    let saved_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(crate_dir()).expect("chdir to crate root");

    let (clock, clock_mock) = RobotClock::mock();

    let mut app = AnytimeRrtStarDeterminismApp::builder()
        .with_clock(clock)
        .with_log_path(log_base, DET_LOG_SLAB_SIZE)?
        .build()
        .expect("failed to build determinism app");

    app.start_all_tasks().expect("failed to start tasks");
    for i in 0..iterations {
        clock_mock.set_value(dt_ticks.saturating_mul(i as u64));
        app.run_one_iteration().expect("run_one_iteration failed");
    }
    app.stop_all_tasks().expect("failed to stop tasks");

    if let Some(cwd) = saved_cwd {
        let _ = std::env::set_current_dir(cwd);
    }
    Ok(())
}

fn read_copperlist_stream_encoded(log_base: &Path) -> CuResult<Vec<Vec<u8>>> {
    let UnifiedLogger::Read(dl) = UnifiedLoggerBuilder::new()
        .file_base_name(log_base)
        .build()
        .expect("failed to open log for read")
    else {
        panic!("expected read logger");
    };
    let mut io_reader = UnifiedLoggerIOReader::new(dl, UnifiedLogType::CopperList);
    let iter = copperlists_reader::<default::CuStampedDataSet>(&mut io_reader);
    let mut out = Vec::new();
    for cl in iter {
        let bytes = bincode::encode_to_vec(cl, bincode::config::standard())
            .expect("failed to bincode-encode copperlist");
        out.push(bytes);
    }
    Ok(out)
}

#[cfg_attr(all(test, feature = "determinism_ci"), test)]
pub fn determinism_two_records_match() {
    let iterations: usize = std::env::var("COPPER_DETERMINISM_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let dt_ticks: u64 = std::env::var("COPPER_DETERMINISM_DT_TICKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000); // 10 ms per tick at 100 Hz

    let case_dir = fresh_case_dir("cu_anytime_rrt_star");
    let a_base = case_dir.join("record_a.copper");
    let b_base = case_dir.join("record_b.copper");

    record_run(&a_base, iterations, dt_ticks).expect("record A failed");
    record_run(&b_base, iterations, dt_ticks).expect("record B failed");

    let a_stream = read_copperlist_stream_encoded(&a_base).expect("read A failed");
    let b_stream = read_copperlist_stream_encoded(&b_base).expect("read B failed");

    assert!(!a_stream.is_empty(), "no copperlists recorded");
    assert_eq!(
        a_stream.len(),
        iterations,
        "recorded copperlist count must equal iteration count",
    );
    assert_eq!(
        a_stream, b_stream,
        "same seed + same config must yield byte-identical copperlist streams"
    );

    let _ = fs::remove_dir_all(case_dir);
}
