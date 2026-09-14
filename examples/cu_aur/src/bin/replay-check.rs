//! Runs the graph for a short window and checks that every callback replayed the
//! execution times the dataset recorded for it.

use clap::Parser;
use cu_aur::check::{BAR_NS, BAR_PCT, replay_rows};
use cu_aur::costs::table;
use cu_aur::payload;
use cu_aur::{CALIBRATION_FILENAME, CONFIG_FILENAME, COSTS_FILENAME, DEFAULT_ALPHA, crate_path};
use cu29::prelude::*;
use std::path::PathBuf;

#[cfg(feature = "pgo-plan")]
gen_cumsgs!("copperconfig-pgo.ron");
#[cfg(not(feature = "pgo-plan"))]
gen_cumsgs!("copperconfig.ron");

#[derive(Parser)]
#[command(about = "Check the Autoware Universe cost replay against its targets")]
struct Args {
    /// Measurement window.
    #[arg(long, default_value_t = 5)]
    seconds: u64,
    /// Scale applied to every recorded execution time.
    #[arg(long, default_value_t = DEFAULT_ALPHA)]
    alpha: f64,
    /// The recorded per-callback execution-time sequences.
    #[arg(long)]
    costs: Option<PathBuf>,
    /// This host's crunch unit, from the calibrate binary.
    #[arg(long)]
    calibration: Option<PathBuf>,
    /// Unified Copper log base for the check's own run.
    #[arg(long)]
    log_base: Option<PathBuf>,
    /// Report without failing on a callback over the bar.
    #[arg(long)]
    no_bar: bool,
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!(
            "replay-check measures the release workload: cargo run --release --bin replay-check"
        );
        std::process::exit(1);
    }
    let args = Args::parse();
    let costs = args.costs.unwrap_or_else(|| crate_path(COSTS_FILENAME));
    let calibration = args
        .calibration
        .unwrap_or_else(|| crate_path(CALIBRATION_FILENAME));
    let log_base = args
        .log_base
        .unwrap_or_else(|| crate_path("logs").join("replay-check.copper"));
    cu_aur::install_costs(&costs, args.alpha, &calibration).expect("the cost table must install");
    cu_aur::run(args.seconds, Some(log_base.clone())).expect("the check's own run must succeed");

    let config = read_configuration(crate_path(CONFIG_FILENAME).to_str().expect("UTF-8 path"))
        .expect("the configuration must be readable");
    let rows = replay_rows::<CuMsgs>(&log_base, &config, table().expect("an installed table"))
        .expect("the check's own log must be readable");

    println!("replay check — {}s at alpha {}", args.seconds, args.alpha);
    println!(
        "\ncallback                                                n   measured     target      err%"
    );
    let mut over = Vec::new();
    for row in &rows {
        println!(
            "{:<52}{:>5}{:>11.4}{:>11.4}{:>+10.2}",
            row.task,
            row.firings,
            row.measured_ns / 1e6,
            row.target_ns / 1e6,
            row.error_pct
        );
        if !row.within_bar() {
            over.push(row);
        }
    }
    println!(
        "\n{} callbacks, bar {BAR_PCT}% of target or {BAR_NS}ns absolute",
        rows.len()
    );
    if over.is_empty() {
        println!("every callback is within the bar at the median");
        return;
    }
    println!("{} callback(s) over the bar:", over.len());
    for row in &over {
        println!("  {} {:+.2}%", row.task, row.error_pct);
    }
    if !args.no_bar {
        std::process::exit(1);
    }
}
