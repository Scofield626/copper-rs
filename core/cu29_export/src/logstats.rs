use crate::copperlists_reader;
use cu29::clock::{CuDuration, OptionCuTime};
use cu29::config::{CuConfig, CuGraph, Flavor, PlanPolicy, PlanProfile};
use cu29::curuntime::{CuExecutionLoop, CuExecutionUnit, compute_runtime_plan};
use cu29::monitoring::CuDurationStatistics;
use cu29::prelude::{CopperListTuple, CuMsgMetadataTrait, CuPayloadRawBytes};
use cu29::{CuError, CuResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const LOGSTATS_SCHEMA_VERSION: u32 = 2;
const MAX_LATENCY_NS: u64 = 10_000_000_000;

#[derive(Debug, Serialize, Deserialize)]
pub struct LogStats {
    pub schema_version: u32,
    pub config_signature: String,
    pub mission: Option<String>,
    pub edges: Vec<EdgeLogStats>,
    pub perf: PerfStats,
    pub pipeline: PipelineStats,
}

/// Per-plan-step cost, and the throughput ceiling each execution engine can
/// reach with it.
///
/// The serial engine runs every step of a CopperList back to back, so its
/// cycle is `serial_cycle_ns`. `parallel-rt` runs one worker per step and
/// pipelines CopperLists through them, so its cycle is the slowest single
/// step: `bottleneck`. `max_pipeline_speedup` is the ratio, i.e. the most
/// `parallel-rt` can buy on this recording before the bottleneck step has to
/// be split or made faster.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PipelineStats {
    /// One entry per plan step, in plan order.
    pub stages: Vec<StageStats>,
    /// Sum of the mean step durations: the serial engine's cycle.
    pub serial_cycle_ns: Option<f64>,
    /// The slowest step: the `parallel-rt` pipeline's cycle.
    pub bottleneck: Option<Bottleneck>,
    /// `serial_cycle_ns / bottleneck.mean_ns`, capped by the step count.
    pub max_pipeline_speedup: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageStats {
    /// Task or bridge id owning this plan step.
    pub task: String,
    /// Plan step index, which is also the `parallel-rt` worker index.
    pub index: usize,
    pub samples: u64,
    /// Measured `process()` window of this step: min/max/mean/stddev, all
    /// exact. For an exact percentile instead, use
    /// `cu29_export <log> schedule-profile --stat p99`, which keeps the raw
    /// samples; the live histogram behind these stats is too coarse for one.
    pub duration: DurationStats,
    /// Share of `serial_cycle_ns` spent in this step, in `0.0..=1.0`.
    pub share: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bottleneck {
    pub task: String,
    pub index: usize,
    pub mean_ns: f64,
    /// `1e9 / mean_ns`: the CopperList rate this step alone allows.
    pub max_rate_hz: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeLogStats {
    pub src: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_channel: Option<String>,
    pub dst: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst_channel: Option<String>,
    pub msg: String,
    pub samples: u64,
    pub none_samples: u64,
    pub valid_time_samples: u64,
    pub total_raw_bytes: u64,
    pub avg_raw_bytes: Option<f64>,
    pub rate_hz: Option<f64>,
    pub throughput_bytes_per_sec: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PerfStats {
    pub samples: u64,
    pub valid_time_samples: u64,
    pub end_to_end: DurationStats,
    pub jitter: DurationStats,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DurationStats {
    pub min_ns: Option<u64>,
    pub max_ns: Option<u64>,
    pub mean_ns: Option<f64>,
    pub stddev_ns: Option<f64>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct EdgeKey {
    src: String,
    src_channel: Option<String>,
    dst: String,
    dst_channel: Option<String>,
    msg: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SrcMsgKey {
    src: String,
    msg: String,
}

#[derive(Clone, Debug)]
struct OutputSlot {
    edges: Vec<EdgeKey>,
}

#[derive(Debug, Default, Clone)]
struct EdgeAccumulator {
    samples: u64,
    none_samples: u64,
    valid_time_samples: u64,
    total_raw_bytes: u64,
    min_end_ns: Option<u64>,
    max_end_ns: Option<u64>,
}

impl EdgeAccumulator {
    fn record_sample(&mut self, payload_bytes: Option<u64>, end_time_ns: Option<u64>) {
        self.samples = self.samples.saturating_add(1);
        if let Some(bytes) = payload_bytes {
            self.total_raw_bytes = self.total_raw_bytes.saturating_add(bytes);
        } else {
            self.none_samples = self.none_samples.saturating_add(1);
        }

        if let Some(end_ns) = end_time_ns {
            self.valid_time_samples = self.valid_time_samples.saturating_add(1);
            self.min_end_ns = Some(self.min_end_ns.map_or(end_ns, |min| min.min(end_ns)));
            self.max_end_ns = Some(self.max_end_ns.map_or(end_ns, |max| max.max(end_ns)));
        }
    }

    fn finalize(self, key: EdgeKey) -> EdgeLogStats {
        let payload_samples = self.samples.saturating_sub(self.none_samples);
        let avg_raw_bytes = if payload_samples > 0 {
            Some(self.total_raw_bytes as f64 / payload_samples as f64)
        } else {
            None
        };

        let (rate_hz, throughput_bytes_per_sec) = if self.valid_time_samples >= 2 {
            match (self.min_end_ns, self.max_end_ns) {
                (Some(min_ns), Some(max_ns)) if max_ns > min_ns => {
                    let duration_ns = max_ns - min_ns;
                    let duration_secs = duration_ns as f64 / 1_000_000_000.0;
                    let intervals = (self.valid_time_samples - 1) as f64;
                    (
                        Some(intervals / duration_secs),
                        Some(self.total_raw_bytes as f64 / duration_secs),
                    )
                }
                _ => (None, None),
            }
        } else {
            (None, None)
        };

        EdgeLogStats {
            src: key.src,
            src_channel: key.src_channel,
            dst: key.dst,
            dst_channel: key.dst_channel,
            msg: key.msg,
            samples: self.samples,
            none_samples: self.none_samples,
            valid_time_samples: self.valid_time_samples,
            total_raw_bytes: self.total_raw_bytes,
            avg_raw_bytes,
            rate_hz,
            throughput_bytes_per_sec,
        }
    }
}

/// One plan step's flattened slot range in the copperlist message vector.
pub(crate) struct PackRange {
    pub(crate) start: usize,
    pub(crate) len: usize,
    pub(crate) task: String,
}

/// The copperlist message vector flattens the output packs in slot order, so a
/// step owns one contiguous range of it.
pub(crate) fn build_pack_ranges(packs: &[OutputPackInfo]) -> Vec<PackRange> {
    let mut ranges = Vec::with_capacity(packs.len());
    let mut base = 0usize;
    for pack in packs {
        ranges.push(PackRange {
            start: base,
            len: pack.msg_types.len(),
            task: pack.src.clone(),
        });
        base += pack.msg_types.len();
    }
    ranges
}

/// One step's `process()` window in one copperlist: the widest start/end pair
/// over the slots it owns. `None` when the recording has neither bound.
pub(crate) fn sample_step_duration_ns(
    cumsgs: &[&dyn cu29::prelude::ErasedCuStampedData],
    range: &PackRange,
) -> Option<u64> {
    let end_slot = (range.start + range.len).min(cumsgs.len());
    let mut start_ns: Option<u64> = None;
    let mut end_ns: Option<u64> = None;
    for msg in &cumsgs[range.start.min(end_slot)..end_slot] {
        let meta = msg.metadata();
        if let Some(start) = extract_start_time_ns(meta) {
            start_ns = Some(start_ns.map_or(start, |current| current.min(start)));
        }
        if let Some(end) = extract_end_time_ns(meta) {
            end_ns = Some(end_ns.map_or(end, |current| current.max(end)));
        }
    }
    end_ns?.checked_sub(start_ns?)
}

/// Accumulates one plan step's `process()` durations across the recording.
#[derive(Debug)]
struct StageAccumulator {
    task: String,
    stats: CuDurationStatistics,
}

impl StageAccumulator {
    fn new(task: String) -> Self {
        Self {
            task,
            stats: CuDurationStatistics::new(CuDuration(MAX_LATENCY_NS)),
        }
    }

    fn record_sample(&mut self, duration_ns: u64) {
        self.stats.record(CuDuration(duration_ns));
    }

    fn finalize(&self, index: usize, serial_cycle_ns: Option<f64>) -> StageStats {
        let duration = duration_stats_from(&self.stats);
        let share = match (duration.mean_ns, serial_cycle_ns) {
            (Some(mean), Some(cycle)) if cycle > 0.0 => Some(mean / cycle),
            _ => None,
        };
        StageStats {
            task: self.task.clone(),
            index,
            samples: self.stats.len(),
            duration,
            share,
        }
    }
}

fn finalize_pipeline(accumulators: &[StageAccumulator]) -> PipelineStats {
    let means: Vec<Option<f64>> = accumulators
        .iter()
        .map(|acc| duration_stats_from(&acc.stats).mean_ns)
        .collect();

    // Steps the recording never sampled leave the cycle unknown rather than
    // making it look cheaper than it is.
    let serial_cycle_ns = means
        .iter()
        .copied()
        .try_fold(0.0, |total, mean| mean.map(|mean| total + mean));

    let bottleneck = means
        .iter()
        .enumerate()
        .filter_map(|(index, mean)| mean.map(|mean| (index, mean)))
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .filter(|(_, mean)| *mean > 0.0)
        .map(|(index, mean)| Bottleneck {
            task: accumulators[index].task.clone(),
            index,
            mean_ns: mean,
            max_rate_hz: 1e9 / mean,
        });

    let max_pipeline_speedup = match (serial_cycle_ns, &bottleneck) {
        (Some(cycle), Some(slowest)) if slowest.mean_ns > 0.0 => Some(cycle / slowest.mean_ns),
        _ => None,
    };

    PipelineStats {
        stages: accumulators
            .iter()
            .enumerate()
            .map(|(index, acc)| acc.finalize(index, serial_cycle_ns))
            .collect(),
        serial_cycle_ns,
        bottleneck,
        max_pipeline_speedup,
    }
}

#[derive(Debug)]
struct PerfAccumulator {
    stats: CuDurationStatistics,
    samples: u64,
    valid_time_samples: u64,
}

impl PerfAccumulator {
    fn new() -> Self {
        Self {
            stats: CuDurationStatistics::new(CuDuration(MAX_LATENCY_NS)),
            samples: 0,
            valid_time_samples: 0,
        }
    }

    fn record_sample(&mut self, latency: Option<CuDuration>) {
        self.samples = self.samples.saturating_add(1);
        if let Some(latency) = latency {
            self.stats.record(latency);
            self.valid_time_samples = self.valid_time_samples.saturating_add(1);
        }
    }

    fn finalize(&self) -> PerfStats {
        let end_to_end = duration_stats_from(&self.stats);
        let jitter = jitter_stats_from(&self.stats);

        PerfStats {
            samples: self.samples,
            valid_time_samples: self.valid_time_samples,
            end_to_end,
            jitter,
        }
    }
}

pub fn compute_logstats<P>(
    mut reader: impl Read,
    config: &CuConfig,
    mission: Option<&str>,
) -> CuResult<LogStats>
where
    P: CopperListTuple + CuPayloadRawBytes,
{
    let graph = config.get_graph(mission)?;
    let signature = build_graph_signature(graph, mission);
    let packs = collect_output_packs(graph, config.plan_policy(), &config.plan_profile())?;
    let stage_ranges = build_pack_ranges(&packs);
    let output_slots = build_output_slots(&packs, graph);
    let mut stage_accumulators: Vec<StageAccumulator> = stage_ranges
        .iter()
        .map(|range| StageAccumulator::new(range.task.clone()))
        .collect();
    let mut edge_accumulators = build_edge_accumulators(graph);
    let mut perf = PerfAccumulator::new();
    let mut warned_lengths = false;

    for culist in copperlists_reader::<P>(&mut reader) {
        let payload_sizes = culist.msgs.payload_raw_bytes();
        let cumsgs = culist.msgs.cumsgs();

        let payload_len = payload_sizes.len();
        let msg_len = cumsgs.len();
        let slot_len = output_slots.len();
        if !warned_lengths && (payload_len != msg_len || payload_len != slot_len) {
            eprintln!(
                "Warning: output mapping length mismatch (sizes={}, msgs={}, slots={})",
                payload_len, msg_len, slot_len
            );
            warned_lengths = true;
        }

        let count = payload_len.min(msg_len).min(slot_len);

        for idx in 0..count {
            let slot = &output_slots[idx];
            if slot.edges.is_empty() {
                continue;
            }
            let payload_bytes = payload_sizes[idx];
            let end_time_ns = extract_end_time_ns(cumsgs[idx].metadata());
            for edge in &slot.edges {
                if let Some(acc) = edge_accumulators.get_mut(edge) {
                    acc.record_sample(payload_bytes, end_time_ns);
                }
            }
        }

        for (range, acc) in stage_ranges.iter().zip(stage_accumulators.iter_mut()) {
            if let Some(duration_ns) = sample_step_duration_ns(&cumsgs, range) {
                acc.record_sample(duration_ns);
            }
        }

        perf.record_sample(compute_end_to_end_latency(&cumsgs));
    }

    let edges = edge_accumulators
        .into_iter()
        .map(|(key, acc)| acc.finalize(key))
        .collect();

    Ok(LogStats {
        schema_version: LOGSTATS_SCHEMA_VERSION,
        config_signature: signature,
        mission: mission.map(|value| value.to_string()),
        edges,
        perf: perf.finalize(),
        pipeline: finalize_pipeline(&stage_accumulators),
    })
}

/// One line naming the step that caps the CopperList rate, and what a wider
/// engine could still buy. Printed next to the JSON so the ceiling is visible
/// without opening the file.
pub fn format_bottleneck(pipeline: &PipelineStats) -> String {
    let Some(slowest) = &pipeline.bottleneck else {
        return "Bottleneck: unknown (no step had a recorded process_time window).".to_string();
    };
    let speedup = match pipeline.max_pipeline_speedup {
        Some(speedup) => format!("{speedup:.2}x"),
        None => "unknown".to_string(),
    };
    format!(
        "Bottleneck: step {} '{}' at {:.3} ms mean caps the rate at {:.1} Hz; \
         pipelining every step can buy at most {speedup}.",
        slowest.index,
        slowest.task,
        slowest.mean_ns / 1e6,
        slowest.max_rate_hz,
    )
}

pub fn write_logstats(stats: &LogStats, path: &Path) -> CuResult<()> {
    let file = File::create(path)
        .map_err(|e| CuError::new_with_cause("Failed to create logstats output", e))?;
    serde_json::to_writer_pretty(file, stats)
        .map_err(|e| CuError::new_with_cause("Failed to serialize logstats", e))?;
    Ok(())
}

fn build_output_slots(packs: &[OutputPackInfo], graph: &CuGraph) -> Vec<OutputSlot> {
    let edges_by_src = build_edges_by_src_msg(graph);
    let total_msgs: usize = packs.iter().map(|pack| pack.msg_types.len()).sum();
    let mut slots = Vec::with_capacity(total_msgs);

    for pack in packs {
        for msg in &pack.msg_types {
            let edges = edges_by_src
                .get(&SrcMsgKey {
                    src: pack.src.clone(),
                    msg: msg.clone(),
                })
                .cloned()
                .unwrap_or_default();
            slots.push(OutputSlot { edges });
        }
    }

    slots
}

fn build_edge_accumulators(graph: &CuGraph) -> HashMap<EdgeKey, EdgeAccumulator> {
    let mut acc = HashMap::new();
    for cnx in graph.edges() {
        let key = EdgeKey {
            src: cnx.src.clone(),
            src_channel: cnx.src_channel.clone(),
            dst: cnx.dst.clone(),
            dst_channel: cnx.dst_channel.clone(),
            msg: cnx.msg.clone(),
        };
        acc.entry(key).or_default();
    }
    acc
}

fn build_edges_by_src_msg(graph: &CuGraph) -> HashMap<SrcMsgKey, Vec<EdgeKey>> {
    let mut map: HashMap<SrcMsgKey, Vec<EdgeKey>> = HashMap::new();
    for cnx in graph.edges() {
        let key = SrcMsgKey {
            src: cnx.src.clone(),
            msg: cnx.msg.clone(),
        };
        let edge = EdgeKey {
            src: cnx.src.clone(),
            src_channel: cnx.src_channel.clone(),
            dst: cnx.dst.clone(),
            dst_channel: cnx.dst_channel.clone(),
            msg: cnx.msg.clone(),
        };
        map.entry(key).or_default().push(edge);
    }
    map
}

#[derive(Debug)]
pub(crate) struct OutputPackInfo {
    pub(crate) culist_index: u32,
    pub(crate) src: String,
    pub(crate) msg_types: Vec<String>,
}

pub(crate) fn collect_output_packs(
    graph: &CuGraph,
    plan_policy: PlanPolicy,
    plan_profile: &PlanProfile,
) -> CuResult<Vec<OutputPackInfo>> {
    let plan = compute_runtime_plan(graph, plan_policy, plan_profile)?;
    let mut packs = Vec::new();
    collect_output_packs_from_loop(&plan, graph, &mut packs)?;
    packs.sort_by_key(|pack| pack.culist_index);
    Ok(packs)
}

fn collect_output_packs_from_loop(
    loop_unit: &CuExecutionLoop,
    graph: &CuGraph,
    packs: &mut Vec<OutputPackInfo>,
) -> CuResult<()> {
    for step in &loop_unit.steps {
        match step {
            CuExecutionUnit::Step(step) => {
                if let Some(output_pack) = &step.output_msg_pack {
                    let node = graph
                        .get_node(step.node_id)
                        .ok_or_else(|| CuError::from("Missing node for output pack"))?;
                    packs.push(OutputPackInfo {
                        culist_index: output_pack.culist_index,
                        src: node.get_id(),
                        msg_types: output_pack.msg_types.clone(),
                    });
                }
            }
            CuExecutionUnit::Loop(inner) => {
                collect_output_packs_from_loop(inner, graph, packs)?;
            }
        }
    }
    Ok(())
}

fn compute_end_to_end_latency(
    msgs: &[&dyn cu29::prelude::ErasedCuStampedData],
) -> Option<CuDuration> {
    let start = msgs
        .first()
        .and_then(|msg| extract_start_time_ns(msg.metadata()))?;
    let end = msgs
        .last()
        .and_then(|msg| extract_end_time_ns(msg.metadata()))?;
    end.checked_sub(start).map(CuDuration::from_nanos)
}

pub(crate) fn extract_start_time_ns(meta: &dyn CuMsgMetadataTrait) -> Option<u64> {
    option_time_ns(meta.process_time().start)
}

pub(crate) fn extract_end_time_ns(meta: &dyn CuMsgMetadataTrait) -> Option<u64> {
    option_time_ns(meta.process_time().end)
}

fn option_time_ns(value: OptionCuTime) -> Option<u64> {
    Option::<cu29::clock::CuTime>::from(value).map(|t| t.as_nanos())
}

fn duration_stats_from(stats: &CuDurationStatistics) -> DurationStats {
    if stats.is_empty() {
        return DurationStats::default();
    }
    DurationStats {
        min_ns: Some(stats.min().as_nanos()),
        max_ns: Some(stats.max().as_nanos()),
        mean_ns: Some(stats.mean().as_nanos() as f64),
        stddev_ns: Some(stats.stddev().as_nanos() as f64),
    }
}

fn jitter_stats_from(stats: &CuDurationStatistics) -> DurationStats {
    if stats.len() < 2 {
        return DurationStats::default();
    }
    DurationStats {
        min_ns: Some(stats.jitter_min().as_nanos()),
        max_ns: Some(stats.jitter_max().as_nanos()),
        mean_ns: Some(stats.jitter_mean().as_nanos() as f64),
        stddev_ns: Some(stats.jitter_stddev().as_nanos() as f64),
    }
}

fn build_graph_signature(graph: &CuGraph, mission: Option<&str>) -> String {
    let mut parts = Vec::new();
    parts.push(format!("mission={}", mission.unwrap_or("default")));

    let mut nodes: Vec<_> = graph.get_all_nodes();
    nodes.sort_by_key(|a| a.1.get_id());
    for (_, node) in nodes {
        parts.push(format!(
            "node|{}|{}|{}",
            node.get_id(),
            node.get_type(),
            flavor_label(node.get_flavor())
        ));
    }

    let mut edges: Vec<String> = graph
        .edges()
        .map(|cnx| {
            format!(
                "edge|{}|{}|{}",
                format_endpoint(cnx.src.as_str(), cnx.src_channel.as_deref()),
                format_endpoint(cnx.dst.as_str(), cnx.dst_channel.as_deref()),
                cnx.msg
            )
        })
        .collect();
    edges.sort();
    parts.extend(edges);

    let joined = parts.join("\n");
    format!("fnv1a64:{:016x}", fnv1a64(joined.as_bytes()))
}

fn flavor_label(flavor: Flavor) -> &'static str {
    match flavor {
        Flavor::Task => "task",
        Flavor::Bridge => "bridge",
    }
}

fn format_endpoint(node: &str, channel: Option<&str>) -> String {
    match channel {
        Some(ch) => format!("{node}/{ch}"),
        None => node.to_string(),
    }
}

fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge_key() -> EdgeKey {
        EdgeKey {
            src: "src".to_string(),
            src_channel: None,
            dst: "dst".to_string(),
            dst_channel: None,
            msg: "Msg".to_string(),
        }
    }

    #[test]
    fn edge_stats_average_and_rate() {
        let mut acc = EdgeAccumulator::default();
        acc.record_sample(Some(100), Some(1_000_000_000));
        acc.record_sample(Some(300), Some(2_000_000_000));
        let stats = acc.finalize(edge_key());

        assert_eq!(stats.samples, 2);
        assert_eq!(stats.none_samples, 0);
        assert_eq!(stats.total_raw_bytes, 400);
        assert!((stats.avg_raw_bytes.unwrap() - 200.0).abs() < 1e-6);
        assert!((stats.rate_hz.unwrap() - 1.0).abs() < 1e-6);
        assert!((stats.throughput_bytes_per_sec.unwrap() - 400.0).abs() < 1e-6);
    }

    #[test]
    fn edge_stats_handles_missing_times() {
        let mut acc = EdgeAccumulator::default();
        acc.record_sample(Some(64), None);
        let stats = acc.finalize(edge_key());
        assert_eq!(stats.samples, 1);
        assert_eq!(stats.valid_time_samples, 0);
        assert!(stats.rate_hz.is_none());
        assert!(stats.throughput_bytes_per_sec.is_none());
    }

    fn stage(task: &str, durations: &[u64]) -> StageAccumulator {
        let mut acc = StageAccumulator::new(task.to_string());
        for &duration in durations {
            acc.record_sample(duration);
        }
        acc
    }

    #[test]
    fn pipeline_names_the_slowest_step_and_its_rate() {
        // 1 ms + 4 ms + 1 ms serial; the 4 ms step alone allows 250 Hz.
        let stages = vec![
            stage("cam", &[1_000_000, 1_000_000]),
            stage("detect", &[4_000_000, 4_000_000]),
            stage("brake", &[1_000_000, 1_000_000]),
        ];
        let pipeline = finalize_pipeline(&stages);

        let slowest = pipeline.bottleneck.unwrap();
        assert_eq!(slowest.task, "detect");
        assert_eq!(slowest.index, 1);
        assert!((slowest.max_rate_hz - 250.0).abs() < 1e-6);
        assert!((pipeline.serial_cycle_ns.unwrap() - 6_000_000.0).abs() < 1e-6);
        // 6 ms serial / 4 ms bottleneck: pipelining buys 1.5x, not 3x.
        assert!((pipeline.max_pipeline_speedup.unwrap() - 1.5).abs() < 1e-6);

        let shares: Vec<f64> = pipeline
            .stages
            .iter()
            .map(|stage| stage.share.unwrap())
            .collect();
        assert!((shares.iter().sum::<f64>() - 1.0).abs() < 1e-6);
        assert!((shares[1] - 4.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn pipeline_cycle_is_unknown_when_a_step_was_never_sampled() {
        // An unsampled step must not make the serial cycle look cheaper.
        let stages = vec![stage("cam", &[1_000_000]), stage("detect", &[])];
        let pipeline = finalize_pipeline(&stages);

        assert!(pipeline.serial_cycle_ns.is_none());
        assert!(pipeline.max_pipeline_speedup.is_none());
        assert_eq!(pipeline.bottleneck.unwrap().task, "cam");
        assert_eq!(pipeline.stages[1].samples, 0);
        assert!(pipeline.stages[1].share.is_none());
        assert!(pipeline.stages[1].duration.mean_ns.is_none());
    }

    #[test]
    fn pipeline_without_any_sample_reports_no_bottleneck() {
        let pipeline = finalize_pipeline(&[stage("cam", &[])]);
        assert!(pipeline.bottleneck.is_none());
        assert!(format_bottleneck(&pipeline).contains("unknown"));
    }

    #[test]
    fn pack_ranges_follow_the_flattened_slot_order() {
        let packs = vec![
            OutputPackInfo {
                culist_index: 0,
                src: "cam".to_string(),
                msg_types: vec!["Image".to_string(), "Meta".to_string()],
            },
            OutputPackInfo {
                culist_index: 1,
                src: "detect".to_string(),
                msg_types: vec!["Boxes".to_string()],
            },
        ];
        let ranges = build_pack_ranges(&packs);

        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].start, ranges[0].len), (0, 2));
        assert_eq!((ranges[1].start, ranges[1].len), (2, 1));
        assert_eq!(ranges[1].task, "detect");
    }

    #[test]
    fn perf_stats_skip_missing_latency() {
        let mut perf = PerfAccumulator::new();
        perf.record_sample(Some(CuDuration::from_nanos(1_000)));
        perf.record_sample(None);
        let stats = perf.finalize();

        assert_eq!(stats.samples, 2);
        assert_eq!(stats.valid_time_samples, 1);
        assert_eq!(stats.end_to_end.min_ns, Some(1_000));
        assert_eq!(stats.end_to_end.max_ns, Some(1_000));
        assert_eq!(stats.jitter.min_ns, None);
    }
}
