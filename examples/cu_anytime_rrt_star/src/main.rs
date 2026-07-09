use cu29::prelude::*;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[copper_runtime(config = "copperconfig.ron")]
struct AnytimeRrtStarApp {}

const SLAB_SIZE: Option<usize> = Some(64 * 1024 * 1024);

fn main() {
    // Anchor CWD at the crate root so `maps/` and `logs/` in the RON config
    // and the log path below resolve consistently regardless of where the
    // binary is invoked from.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    env::set_current_dir(&manifest_dir).expect("Failed to chdir to crate root");

    let logger_path = "logs/anytime_rrt_star.copper";
    if let Some(parent) = Path::new(logger_path).parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent).expect("Failed to create logs directory");
    }

    let mut app = AnytimeRrtStarApp::builder()
        .with_log_path(logger_path, SLAB_SIZE)
        .expect("Failed to setup logger.")
        .build()
        .expect("Failed to create application.");

    if let Err(error) = app.run() {
        debug!("Application ended: {}", error);
    }
}
