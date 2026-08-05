//! Not a fuzz target: a probe for finding 7 in README.md.
//!
//! `cu_apriltag` reads `bits_corrected` from the RON config and passes it to
//! `add_family_bits` without any bound. The AprilTag quick-decode table grows
//! combinatorially in that value. This prints the resident-memory cost per
//! (family, bits_corrected) so the blow-up is easy to see.
//!
//! Each measurement runs in a fresh child process, because the detector leaks
//! (finding 6) and `VmHWM` is a monotonic high-water mark: measuring every
//! configuration in one process makes each one after the first large one report
//! zero growth.
//!
//! Run with: cargo run --release --bin bits_corrected_probe

use cu_apriltag::AprilTags;
use cu29::prelude::*;

const FAMILIES: [&str; 6] = [
    "tag16h5",
    "tag25h9",
    "tag36h11",
    "tagCircle21h7",
    "tagStandard41h12",
    "tagCustom48h12",
];

/// Peak resident set size of this process, in kB.
fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next().map(str::to_string))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// One measurement, in its own process. Prints `<peak_kb> <baseline_kb> <ms>`.
fn measure(family: &str, bits: u32) {
    let baseline = peak_rss_kb();
    let start = std::time::Instant::now();

    let mut config = ComponentConfig::default();
    config.set("family", family.to_string());
    config.set("bits_corrected", bits);

    match AprilTags::new(Some(&config), ()) {
        Ok(task) => {
            let peak = peak_rss_kb();
            println!("{peak} {baseline} {}", start.elapsed().as_millis());
            // Keep the detector alive until after the measurement.
            drop(task);
        }
        Err(e) => println!("error {e}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Child mode: `<exe> --measure <family> <bits>`.
    if args.len() == 4 && args[1] == "--measure" {
        let bits = args[3].parse().unwrap_or(0);
        measure(&args[2], bits);
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    println!(
        "{:<18} {:>5} {:>12} {:>12} {:>9}",
        "family", "bits", "peak_rss_kB", "growth_kB", "time_ms"
    );

    for family in FAMILIES {
        for bits in 0..=3u32 {
            let out = std::process::Command::new(&exe)
                .args(["--measure", family, &bits.to_string()])
                .output();

            match out {
                Ok(out) if out.status.success() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let fields: Vec<&str> = text.split_whitespace().collect();
                    if let [peak, baseline, ms] = fields.as_slice() {
                        let peak: u64 = peak.parse().unwrap_or(0);
                        let baseline: u64 = baseline.parse().unwrap_or(0);
                        println!(
                            "{family:<18} {bits:>5} {peak:>12} {:>12} {ms:>9}",
                            peak.saturating_sub(baseline)
                        );
                    } else {
                        println!("{family:<18} {bits:>5}  {}", text.trim());
                    }
                }
                // A child that dies (OOM killer, abort) is itself the finding.
                Ok(out) => println!(
                    "{family:<18} {bits:>5}  child died: {} (this is the defect)",
                    out.status
                ),
                Err(e) => println!("{family:<18} {bits:>5}  spawn failed: {e}"),
            }
        }
    }
}
