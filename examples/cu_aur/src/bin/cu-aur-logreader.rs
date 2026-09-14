//! Standard export CLI over this app's `.copper` logs: fsck, extract-copperlists,
//! log-stats, and the `pgo-profile` command the scheduling workflow reads.

use cu_aur::payload;
use cu29::prelude::*;
use cu29_export::run_cli;

#[cfg(feature = "pgo-plan")]
gen_cumsgs!("copperconfig-pgo.ron");
#[cfg(not(feature = "pgo-plan"))]
gen_cumsgs!("copperconfig.ron");

fn main() {
    run_cli::<CuMsgs>().expect("Failed to run the export CLI");
}
