//! Profile extraction for profile-guided scheduling: per-operation costs,
//! chain latencies and delivered source rates from one recorded run.

use crate::copperlists_reader;
use cu29::clock::Tov;
use cu29::config::{CuConfig, DEFAULT_MISSION_ID};
use cu29::curuntime::{CuExecutionUnit, CuStepPhase, CuTaskType};
use cu29::planner::graph_signature;
use cu29::planner::{
    CuChainProfile, CuContract, CuCostStats, CuOperationProfile, CuProfile, CuSourceProfile,
    PlanEntityKind, assemble_runtime_plan_for_mission, step_key,
};
use cu29::prelude::{CopperListTuple, CuPayloadRawBytes, ErasedCuStampedData};
use cu29::{CuError, CuResult};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

/// Per-origin samples gathered while reading the log.
#[derive(Default)]
struct OperationSamples {
    fired: Vec<u64>,
    skipped: Vec<u64>,
}

/// The process interval of one origin in one CopperList, and whether any of
/// its output slots carried a payload.
struct OriginInterval {
    start_ns: u64,
    end_ns: u64,
    fired: bool,
    tov_ns: Option<u64>,
}

/// Reads every CopperList of `reader` and measures the run against
/// `contract` for `mission`.
///
/// An operation's cost is the span of its output slots' `process_time` in
/// one CopperList; an anytime task's base and refinements share one slot and
/// are measured as one job under the base key. A chain sample is taken in
/// every CopperList where the source fired and the sink ran.
pub fn compute_profile<P>(
    mut reader: impl Read,
    config: &CuConfig,
    mission: Option<&str>,
    contract: &CuContract,
) -> CuResult<CuProfile>
where
    P: CopperListTuple + CuPayloadRawBytes,
{
    contract.validate(config, mission)?;
    let mission_id = mission.unwrap_or(DEFAULT_MISSION_ID);
    let graph = config.get_graph(mission)?;
    let plan = assemble_runtime_plan_for_mission(config, graph, mission_id)?;
    // The key an origin's measured span belongs to: its first phase's step.
    // A sink produces no payload, so it counts as fired when one of its
    // inputs' producers fired in the same CopperList.
    let origins = P::get_all_task_ids();
    let mut key_of_origin: HashMap<String, String> = HashMap::new();
    let mut inputs_of_origin: HashMap<String, Vec<String>> = HashMap::new();
    let mut producer_of_slot: HashMap<u32, String> = HashMap::new();
    let origin_of = |entity: &cu29::planner::PlanEntity| match entity.kind {
        PlanEntityKind::Task { .. } => entity.label.clone(),
        PlanEntityKind::BridgeRx { .. } | PlanEntityKind::BridgeTx { .. } => {
            format!("bridge::{}", entity.label)
        }
    };
    for unit in &plan.execution.steps {
        let CuExecutionUnit::Step(step) = unit else {
            continue;
        };
        if let Some(output) = &step.output_msg_pack {
            producer_of_slot.insert(
                output.culist_index,
                origin_of(&plan.entities[step.node_id as usize]),
            );
        }
    }
    for unit in &plan.execution.steps {
        let CuExecutionUnit::Step(step) = unit else {
            continue;
        };
        if step.phase == CuStepPhase::AnytimeRefine {
            continue;
        }
        let entity = &plan.entities[step.node_id as usize];
        let origin = origin_of(entity);
        // Only operations with a CopperList slot are measured.
        if !origins.contains(&origin.as_str()) {
            continue;
        }
        if step.task_type == CuTaskType::Sink {
            inputs_of_origin.insert(
                origin.clone(),
                step.input_msg_indices_types
                    .iter()
                    .filter_map(|input| producer_of_slot.get(&input.culist_index).cloned())
                    .collect(),
            );
        }
        key_of_origin
            .entry(origin)
            .or_insert_with(|| step_key(mission_id, entity, step.phase, None));
    }
    let mut samples: BTreeMap<String, OperationSamples> = key_of_origin
        .values()
        .map(|key| (key.clone(), OperationSamples::default()))
        .collect();
    let mut chain_samples: Vec<Vec<u64>> = vec![Vec::new(); contract.chains.len()];
    let mut source_fired: BTreeMap<&str, u64> = contract
        .sources
        .iter()
        .map(|source| (source.task.as_str(), 0))
        .collect();
    let mut spans = Vec::new();
    let mut copperlists = 0u64;
    let mut window: Option<(u64, u64)> = None;
    let mut intervals: HashMap<&str, OriginInterval> = HashMap::new();

    for culist in copperlists_reader::<P>(&mut reader) {
        copperlists += 1;
        intervals.clear();
        for (msg, origin) in culist.msgs.cumsgs().iter().zip(origins.iter()) {
            record_interval(&mut intervals, origin, *msg);
        }
        for (sink, inputs) in &inputs_of_origin {
            let fed = inputs
                .iter()
                .any(|input| intervals.get(input.as_str()).is_some_and(|i| i.fired));
            if let Some(interval) = intervals.get_mut(sink.as_str()) {
                interval.fired |= fed;
            }
        }
        let mut cl_start = None;
        let mut cl_end = None;
        for (origin, interval) in &intervals {
            let Some(key) = key_of_origin.get(*origin) else {
                continue;
            };
            let cost = interval.end_ns.saturating_sub(interval.start_ns);
            let entry = samples.entry(key.clone()).or_default();
            if interval.fired {
                entry.fired.push(cost);
            } else {
                entry.skipped.push(cost);
            }
            cl_start = Some(cl_start.map_or(interval.start_ns, |s: u64| s.min(interval.start_ns)));
            cl_end = Some(cl_end.map_or(interval.end_ns, |e: u64| e.max(interval.end_ns)));
            if interval.fired
                && let Some(count) = source_fired.get_mut(*origin)
            {
                *count += 1;
            }
        }
        if let (Some(start), Some(end)) = (cl_start, cl_end) {
            spans.push(end - start);
            window = Some(window.map_or((start, end), |(s, e)| (s.min(start), e.max(end))));
        }
        for (chain, samples) in contract.chains.iter().zip(chain_samples.iter_mut()) {
            let (Some(source), Some(sink)) = (
                intervals.get(chain.source.as_str()),
                intervals.get(chain.sink.as_str()),
            ) else {
                continue;
            };
            if !source.fired || !sink.fired {
                continue;
            }
            let start = source.tov_ns.unwrap_or(source.start_ns);
            samples.push(sink.end_ns.saturating_sub(start));
        }
    }

    let (window_start, window_end) = window
        .ok_or_else(|| CuError::from("The log carries no CopperList with process timestamps"))?;
    let window_ns = window_end.saturating_sub(window_start);
    let window_s = window_ns as f64 / 1e9;
    let mut profile = CuProfile::new(graph_signature(graph, mission), mission_id.to_string());
    profile.copperlists = copperlists;
    profile.window_ns = window_ns;
    profile.copperlist_span = CuCostStats::from_samples(&mut spans);
    for (key, mut operation) in samples {
        let fired = CuCostStats::from_samples(&mut operation.fired);
        // `n` firings span `n - 1` intervals of the window.
        let firing_rate_hz = if window_s > 0.0 && fired.samples >= 2 {
            (fired.samples - 1) as f64 / window_s
        } else {
            0.0
        };
        profile.operations.insert(
            key,
            CuOperationProfile {
                fired,
                skipped: CuCostStats::from_samples(&mut operation.skipped),
                firing_rate_hz,
            },
        );
    }
    for (chain, mut latencies) in contract.chains.iter().zip(chain_samples) {
        let deadline_ns = u64::from(chain.deadline_ms) * 1_000_000;
        let misses = latencies.iter().filter(|&&l| l > deadline_ns).count() as u64;
        profile.chains.insert(
            chain.id.clone(),
            CuChainProfile {
                deadline_ms: chain.deadline_ms,
                latency: CuCostStats::from_samples(&mut latencies),
                misses,
            },
        );
    }
    for source in &contract.sources {
        let fired = source_fired[source.task.as_str()];
        // The window spans intervals between firings: `n` firings make
        // `n - 1` of them, against the intervals the period allows.
        let expected = window_ns as f64 / (f64::from(source.period_ms) * 1e6);
        profile.sources.insert(
            source.task.clone(),
            CuSourceProfile {
                period_ms: source.period_ms,
                fired,
                expected,
                delivered_rate: if expected > 0.0 && fired > 0 {
                    (fired - 1) as f64 / expected
                } else {
                    0.0
                },
            },
        );
    }
    Ok(profile)
}

fn record_interval<'a>(
    intervals: &mut HashMap<&'a str, OriginInterval>,
    origin: &'a str,
    msg: &dyn ErasedCuStampedData,
) {
    let metadata = msg.metadata();
    let (Some(start_ns), Some(end_ns)) = (
        time_ns(metadata.process_time().start),
        time_ns(metadata.process_time().end),
    ) else {
        return;
    };
    if end_ns < start_ns {
        return;
    }
    let fired = msg.payload().is_some();
    let tov_ns = match msg.tov() {
        Tov::Time(time) => Some(time.as_nanos()),
        Tov::Range(range) => Some(range.start.as_nanos()),
        Tov::None => None,
    };
    intervals
        .entry(origin)
        .and_modify(|interval| {
            interval.start_ns = interval.start_ns.min(start_ns);
            interval.end_ns = interval.end_ns.max(end_ns);
            interval.fired |= fired;
            if interval.tov_ns.is_none() {
                interval.tov_ns = tov_ns;
            }
        })
        .or_insert(OriginInterval {
            start_ns,
            end_ns,
            fired,
            tov_ns,
        });
}

fn time_ns(value: cu29::clock::OptionCuTime) -> Option<u64> {
    Option::<cu29::clock::CuTime>::from(value).map(|t| t.as_nanos())
}
