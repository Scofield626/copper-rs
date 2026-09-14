//! The Autoware Universe replica as a Copper application.
//!
//! The graph lives here rather than in `main.rs` so the `calibrate` and `replay-check`
//! binaries link the same `tasks::crunch` the application runs.

pub mod calib;
pub mod check;
pub mod costs;
pub mod payload;
pub mod tasks;

use cu29::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg_attr(feature = "pgo-plan", copper_runtime(config = "copperconfig-pgo.ron"))]
#[cfg_attr(not(feature = "pgo-plan"), copper_runtime(config = "copperconfig.ron"))]
struct App {}

const SLAB_SIZE: Option<usize> = Some(64 * 1024 * 1024);
/// How often the stop thread checks whether the run already ended.
const STOP_POLL: Duration = Duration::from_millis(20);

/// The dataset's scale factor for a total median utilisation of 2.2 cores, from
/// `data/graph.json`'s `alpha` table. The replay multiplies every recorded execution
/// time by it.
pub const DEFAULT_ALPHA: f64 = 0.457811;
pub const CALIBRATION_FILENAME: &str = "calibration.ron";
pub const COSTS_FILENAME: &str = "data/costs.json";
pub const CONFIG_FILENAME: &str = "copperconfig.ron";
pub const DEFAULT_LOG_BASE: &str = "logs/aur.copper";

/// A path inside the crate, so a binary behaves the same from any working directory.
pub fn crate_path(relative: impl AsRef<Path>) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Loads `costs`, scales it by `alpha` and this host's crunch unit, and installs it.
pub fn install_costs(costs: &Path, alpha: f64, calibration: &Path) -> CuResult<()> {
    costs::install_from(costs, alpha, calibration)
}

/// Installs an already built replay table, for a caller that measured its own crunch unit.
pub fn install_costs_table(table: costs::CostTable) -> CuResult<()> {
    costs::install(table)
}

/// Records `seconds` of the graph into `log_base`, stopping at the next cycle boundary.
pub fn run(seconds: u64, log_base: Option<PathBuf>) -> CuResult<()> {
    let logger_path = log_base.unwrap_or_else(|| crate_path(DEFAULT_LOG_BASE));
    if let Some(parent) = logger_path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .map_err(|e| CuError::new_with_cause("Failed to create the logs directory", e))?;
    }
    let application = App::builder()
        .with_log_path(&logger_path, SLAB_SIZE)?
        .build()?;

    let ended = Arc::new(AtomicBool::new(false));
    let stopper = {
        let ended = Arc::clone(&ended);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        thread::spawn(move || {
            while !ended.load(Ordering::Relaxed) && Instant::now() < deadline {
                thread::sleep(STOP_POLL);
            }
            // The stop flag is process-wide and never cleared, so a run that ended on
            // its own must not leave it set for the next one.
            if !ended.load(Ordering::Relaxed) {
                App::request_stop();
            }
        })
    };
    let outcome = application
        .run_until_shutdown()
        .map(|_| ())
        .map_err(|e| e.error);
    ended.store(true, Ordering::Relaxed);
    let _ = stopper.join();
    outcome
}
