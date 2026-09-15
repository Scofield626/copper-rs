use clap::Parser;
use cu_aur::{CALIBRATION_FILENAME, COSTS_FILENAME, DEFAULT_ALPHA, crate_path};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Run the Autoware Universe replica")]
struct Args {
    /// Measurement window.
    #[arg(long, default_value_t = 10)]
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
    /// Unified Copper log base. Defaults to the crate's logs/aur.copper.
    #[arg(long)]
    log_base: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    let costs = args.costs.unwrap_or_else(|| crate_path(COSTS_FILENAME));
    let calibration = args
        .calibration
        .unwrap_or_else(|| crate_path(CALIBRATION_FILENAME));
    if let Err(error) = cu_aur::install_costs(&costs, args.alpha, &calibration) {
        eprintln!("cu-aur: {error}");
        std::process::exit(1);
    }
    if let Err(error) = cu_aur::run(args.seconds, args.log_base) {
        eprintln!("cu-aur: {error}");
        std::process::exit(1);
    }
}
