//! The generated graph matches its data, and the replay costs what the data says.

use cu29::planner::CuContract;
use cu29::prelude::*;
use cu_aur::calib::measure_unit_cost;
use cu_aur::check::replay_rows;
use cu_aur::costs::{CostTable, read_cost_file};
use cu_aur::payload;
use cu_aur::{COSTS_FILENAME, CONFIG_FILENAME, DEFAULT_ALPHA, crate_path};
use std::process::Command;

gen_cumsgs!("copperconfig.ron");

/// Long enough for every root but the 1000ms GNSS one to fire several times.
const CHECK_SECONDS: u64 = 2;

fn config() -> CuConfig {
    read_configuration(crate_path(CONFIG_FILENAME).to_str().expect("UTF-8 path"))
        .expect("copperconfig.ron must parse")
}

#[test]
fn test_the_committed_config_and_contract_are_what_the_generator_writes() {
    let output = Command::new("python3")
        .arg(crate_path("gen_config.py"))
        .arg("--check")
        .output()
        .expect("python3 must be available");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_every_chain_of_the_contract_reaches_its_sink() {
    let contract = CuContract::read(&crate_path("pgo.ron")).expect("pgo.ron must parse");
    // `validate` checks that every chain's sink is reachable from its source and that
    // every named task exists in the graph.
    contract
        .validate(&config(), None)
        .expect("the contract must hold on the generated graph");
    assert_eq!(contract.chains.len(), 18);
    assert_eq!(contract.sources.len(), 11);
}

/// Runs the graph for a short window and requires every callback to land on its recorded
/// execution times. Calibrated in-process so the check holds in a debug build too, where
/// the crunch costs several times what it does in release.
#[test]
fn test_every_callback_replays_the_recorded_execution_times() {
    let (_, k_ns_per_unit, _) = measure_unit_cost();
    let costs = read_cost_file(&crate_path(COSTS_FILENAME)).expect("costs.json must parse");
    let table =
        CostTable::build(&costs, DEFAULT_ALPHA, k_ns_per_unit).expect("the table must build");
    let log_base = crate_path("logs").join("replay-check-test.copper");
    cu_aur::install_costs_table(table).expect("the table must install");
    cu_aur::run(CHECK_SECONDS, Some(log_base.clone())).expect("the run must succeed");

    let table = cu_aur::costs::table().expect("an installed table");
    let rows = replay_rows::<CuMsgs>(&log_base, &config(), table).expect("the log must be readable");
    assert!(rows.len() > 60, "only {} callbacks fired", rows.len());
    let over: Vec<_> = rows.iter().filter(|row| !row.within_bar()).collect();
    assert!(
        over.is_empty(),
        "{} callback(s) off their targets: {:?}",
        over.len(),
        over
            .iter()
            .map(|row| (row.task.as_str(), row.error_pct))
            .collect::<Vec<_>>()
    );
}
