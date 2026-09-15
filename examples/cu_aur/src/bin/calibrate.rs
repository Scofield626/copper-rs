//! Writes this host's crunch unit to `calibration.ron`, which every replay converts its
//! target durations with.

use chrono::Utc;
use clap::Parser;
use cu_aur::calib::measure_unit_cost;
use cu_aur::costs::{Calibration, write_calibration};
use cu_aur::{CALIBRATION_FILENAME, crate_path};
use std::fs;
use std::path::PathBuf;

/// The fit is written only when every point sits within this far of it.
const FIT_BAR: f64 = 5.0;

#[derive(Parser)]
#[command(about = "Measure this host's crunch unit for the Autoware Universe replay")]
struct Args {
    /// Where to write the calibration. Defaults to the crate's calibration.ron.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Print the fit without writing it.
    #[arg(long)]
    dry_run: bool,
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.trim().to_string())
        })
        .unwrap_or_else(|| "unknown CPU".to_string())
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("calibrate measures the release workload: cargo run --release --bin calibrate");
        std::process::exit(1);
    }
    let args = Args::parse();
    let (points, k_ns_per_unit, worst) = measure_unit_cost();
    println!("     units   median_ns   fitted_ns    err%");
    for (units, measured) in &points {
        let fitted = k_ns_per_unit * units;
        let err = (fitted - measured) / measured * 100.0;
        println!("{units:10.0}  {measured:10.0}  {fitted:10.0}  {err:+6.2}");
    }
    println!("k = {k_ns_per_unit:.4} ns per unit, worst point {worst:.2}%");

    if args.dry_run {
        return;
    }
    if worst > FIT_BAR {
        eprintln!(
            "the fit misses a point by {worst:.2}%, over the {FIT_BAR}% bar. Nothing written."
        );
        std::process::exit(1);
    }
    let calibration = Calibration {
        k_ns_per_unit,
        fit_error_pct: worst,
        cpu: cpu_model(),
        measured_utc: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    let path = args
        .output
        .unwrap_or_else(|| crate_path(CALIBRATION_FILENAME));
    write_calibration(&path, &calibration).expect("calibration output must be writable");
    println!("wrote {}", path.display());
}
