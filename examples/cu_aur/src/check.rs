//! Does the replay cost what the dataset says it costs?
//!
//! Reads a recorded log and reports, per callback, the median of its measured
//! `process_time` against the median of the targets it was given, plus the median of the
//! per-firing relative error.
//!
//! Pairing needs no extra bookkeeping: every callback of a sub-DAG fires exactly once
//! per firing of its root, in CopperList order, so a callback's n-th recorded firing
//! replays the n-th sample of its sequence.

use crate::costs::CostTable;
use cu29::prelude::*;
use cu29_export::copperlists_reader;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A callback whose median replay is further than this from its target is not replaying
/// it. Callbacks of a few microseconds are judged on the absolute gap instead: Copper's
/// own per-step bookkeeping lands in `process_time` and is around half a microsecond.
pub const BAR_PCT: f64 = 5.0;
pub const BAR_NS: f64 = 1_000.0;

/// One callback's replay accuracy over a run.
#[derive(Debug, Clone)]
pub struct ReplayRow {
    pub task: String,
    pub firings: usize,
    pub measured_ns: f64,
    pub target_ns: f64,
    /// Median of the per-firing relative error, in percent.
    pub error_pct: f64,
}

impl ReplayRow {
    /// Whether the callback replayed its sequence within the bar.
    pub fn within_bar(&self) -> bool {
        self.error_pct.abs() <= BAR_PCT || (self.measured_ns - self.target_ns).abs() <= BAR_NS
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Measures `log_base` against `table`, using `config` for the graph and each task's
/// `cost_index`.
pub fn replay_rows<P>(
    log_base: &Path,
    config: &CuConfig,
    table: &CostTable,
) -> CuResult<Vec<ReplayRow>>
where
    P: CopperListTuple + CuPayloadRawBytes,
{
    let graph = config.get_graph(None)?;
    let mut cost_index: HashMap<String, u32> = HashMap::new();
    for (_, node) in graph.get_all_nodes() {
        let index = node
            .get_instance_config()
            .and_then(|c| c.get::<u32>("cost_index").ok().flatten())
            .ok_or_else(|| {
                CuError::from(format!("task '{}' carries no cost_index", node.get_id()))
            })?;
        cost_index.insert(node.get_id(), index);
    }
    // A sink produces no payload, so it fires when the producer of its first input did.
    let mut first_producer: HashMap<String, String> = HashMap::new();
    let mut has_successor: HashSet<String> = HashSet::new();
    for edge in graph.edges() {
        has_successor.insert(edge.src.clone());
        first_producer
            .entry(edge.dst.clone())
            .or_insert_with(|| edge.src.clone());
    }

    let origins = P::get_all_task_ids();
    let mut measured: HashMap<&str, Vec<f64>> =
        origins.iter().map(|id| (*id, Vec::new())).collect();
    let reader = crate::check::log_reader(log_base)?;
    for culist in copperlists_reader::<P>(reader) {
        let slots = culist.msgs.cumsgs();
        let mut spans: HashMap<&str, (f64, bool)> = HashMap::new();
        for (msg, origin) in slots.iter().zip(origins.iter()) {
            let time = msg.metadata().process_time();
            let (Some(start), Some(end)) = (
                Option::<CuTime>::from(time.start),
                Option::<CuTime>::from(time.end),
            ) else {
                continue;
            };
            spans.insert(
                origin,
                (
                    end.as_nanos().saturating_sub(start.as_nanos()) as f64,
                    msg.payload().is_some(),
                ),
            );
        }
        for origin in origins.iter() {
            let Some((span, payload)) = spans.get(origin).copied() else {
                continue;
            };
            let fired = if has_successor.contains(*origin) {
                payload
            } else {
                first_producer
                    .get(*origin)
                    .and_then(|producer| spans.get(producer.as_str()))
                    .is_some_and(|(_, fired)| *fired)
            };
            if fired {
                measured.entry(origin).or_default().push(span);
            }
        }
    }

    let mut rows = Vec::new();
    for origin in origins.iter() {
        let mut samples = measured.remove(origin).unwrap_or_default();
        if samples.is_empty() {
            continue;
        }
        let index = *cost_index
            .get(*origin)
            .ok_or_else(|| CuError::from(format!("'{origin}' is not a task of the config")))?;
        let sequence = table.sequence(index)?;
        let mut targets: Vec<f64> = (1..=samples.len() as u64)
            .map(|seq| sequence.targets_ns[sequence.index_of(seq)] as f64)
            .collect();
        let mut errors: Vec<f64> = samples
            .iter()
            .zip(targets.iter())
            .filter(|(_, target)| **target > 0.0)
            .map(|(measured, target)| (measured - target) / target * 100.0)
            .collect();
        rows.push(ReplayRow {
            task: (*origin).to_string(),
            firings: samples.len(),
            measured_ns: median(&mut samples),
            target_ns: median(&mut targets),
            error_pct: median(&mut errors),
        });
    }
    rows.sort_by(|a, b| b.error_pct.abs().total_cmp(&a.error_pct.abs()));
    Ok(rows)
}

/// Opens a recorded log for reading.
pub fn log_reader(log_base: &Path) -> CuResult<UnifiedLoggerIOReader> {
    let logger = UnifiedLoggerBuilder::new()
        .file_base_name(log_base)
        .build()
        .map_err(|e| CuError::new_with_cause(&format!("{}", log_base.display()), e))?;
    let UnifiedLogger::Read(logger) = logger else {
        return Err(CuError::from(format!(
            "{}: opened for writing",
            log_base.display()
        )));
    };
    Ok(UnifiedLoggerIOReader::new(
        logger,
        UnifiedLogType::CopperList,
    ))
}
