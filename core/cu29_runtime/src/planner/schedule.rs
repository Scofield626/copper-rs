//! Periodic multicore schedule representation and graph validation.

use super::AssembledPlan;
use super::PlanEntityKind;
use super::fixed::execution_keys;
use crate::config::CuConfig;
use crate::curuntime::CuExecutionUnit;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use cu29_traits::CuError;
use cu29_traits::CuResult;
use serde::Deserialize;
use serde::Serialize;

/// A repeating schedule for one mission, potentially spanning several CLs.
///
/// `steps` is an inventory: indices remain stable when an optimizer moves
/// steps between lanes. Each lane lists its exact execution order. Explicit
/// dependencies add precedence across lanes or schedule cycles. A cycle
/// advances CL ids by `copperlists_per_cycle`; each lane finishes its previous
/// cycle before beginning its next one. There is no global barrier between
/// cycles. The first cycle starts with earlier-cycle dependencies satisfied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuMissionPlan {
    /// Number of consecutive CopperLists represented by one schedule cycle.
    pub copperlists_per_cycle: u32,
    /// Each concrete process step occurs once per CL in this inventory.
    pub steps: Vec<CuPlanStep>,
    /// Ordered work and placement for each logical execution lane.
    pub lanes: Vec<CuPlanLane>,
    /// Additional precedence edges. These order work; they do not change
    /// message wiring or transfer payloads between CopperLists.
    #[serde(default)]
    pub dependencies: Vec<CuPlanDependency>,
}

/// One occurrence of a concrete process step within a schedule cycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuPlanStep {
    /// Opaque stable identity exported from the generated process plan.
    pub key: String,
    /// Zero-based CopperList offset within the repeating cycle.
    pub copperlist: u32,
}

/// One sequential lane. Every inventory index must appear in exactly one lane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuPlanLane {
    /// The execution context assigned to this lane.
    pub placement: CuPlanPlacement,
    /// Inventory indices in the order this lane executes them each cycle.
    pub steps: Vec<u32>,
}

/// Logical placement, separate from machine-specific CPU affinity.
///
/// Pool workers use the corresponding `runtime.thread_pools` entry. Its
/// affinity maps worker `index` to `affinity[index % affinity.len()]` when
/// configured. Sharing an affinity CPU does not merge workers or imply
/// precedence. Host CPU availability and real-time OS permissions are checked
/// by thread setup, not by portable plan validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CuPlanPlacement {
    /// The application's main execution context.
    Main,
    /// A logical worker in an explicitly configured thread pool.
    Worker { pool: String, index: u32 },
}

/// Precedence from `from` to `to`, addressed by inventory index.
///
/// For cycle `n`, `to(n)` waits for `from(n - cycle_lag)`. Zero means the
/// same cycle; one can express state reuse across the cycle boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuPlanDependency {
    /// Index of the predecessor occurrence in the step inventory.
    pub from: u32,
    /// Index of the dependent occurrence in the step inventory.
    pub to: u32,
    /// Number of schedule cycles between predecessor and dependent.
    pub cycle_lag: u32,
}

pub(super) struct PlanShape {
    keys: Vec<String>,
    required: BTreeSet<(usize, usize)>,
    components: Vec<Vec<usize>>,
}

impl PlanShape {
    pub(super) fn new(plan: &AssembledPlan, mission: &str) -> CuResult<Self> {
        let keys = execution_keys(plan, mission)?;
        let mut producers = BTreeMap::new();
        let mut entities: BTreeMap<_, Vec<usize>> = BTreeMap::new();
        let mut components: BTreeMap<_, Vec<usize>> = BTreeMap::new();
        for (index, unit) in plan.execution.steps.iter().enumerate() {
            let CuExecutionUnit::Step(step) = unit else {
                return Err(CuError::from("Nested execution loops cannot be exported"));
            };
            if let Some(output) = &step.output_msg_pack {
                producers.insert(output.culist_index, index);
            }
            entities.entry(step.node_id).or_default().push(index);
            let component = match plan.entities[step.node_id as usize].kind {
                PlanEntityKind::Task { task_index, .. } => (0, task_index),
                PlanEntityKind::BridgeRx {
                    bridge_config_index,
                    ..
                }
                | PlanEntityKind::BridgeTx {
                    bridge_config_index,
                    ..
                } => (1, bridge_config_index),
            };
            components.entry(component).or_default().push(index);
        }
        let mut required = BTreeSet::new();
        for indices in entities.values() {
            for pair in indices.windows(2) {
                required.insert((pair[0], pair[1]));
            }
        }
        for (index, unit) in plan.execution.steps.iter().enumerate() {
            let CuExecutionUnit::Step(step) = unit else {
                continue;
            };
            for input in &step.input_msg_indices_types {
                let producer = producers
                    .get(&input.culist_index)
                    .ok_or_else(|| CuError::from("Missing process-step producer"))?;
                required.insert((*producer, index));
            }
        }
        Ok(Self {
            keys,
            required,
            components: components.into_values().collect(),
        })
    }

    pub(super) fn serial_plan(self) -> CuResult<CuMissionPlan> {
        let mut dependencies = BTreeSet::new();
        for (from, to) in self.required {
            dependencies.insert(CuPlanDependency {
                from: step_id(from)?,
                to: step_id(to)?,
                cycle_lag: 0,
            });
        }
        for component in &self.components {
            for pair in component.windows(2) {
                dependencies.insert(CuPlanDependency {
                    from: step_id(pair[0])?,
                    to: step_id(pair[1])?,
                    cycle_lag: 0,
                });
            }
            if let (Some(first), Some(last)) = (component.first(), component.last()) {
                dependencies.insert(CuPlanDependency {
                    from: step_id(*last)?,
                    to: step_id(*first)?,
                    cycle_lag: 1,
                });
            }
        }
        let order = (0..self.keys.len())
            .map(step_id)
            .collect::<CuResult<Vec<_>>>()?;
        Ok(CuMissionPlan {
            copperlists_per_cycle: 1,
            steps: self
                .keys
                .into_iter()
                .map(|key| CuPlanStep { key, copperlist: 0 })
                .collect(),
            lanes: vec![CuPlanLane {
                placement: CuPlanPlacement::Main,
                steps: order,
            }],
            dependencies: dependencies.into_iter().collect(),
        })
    }
}

fn step_id(index: usize) -> CuResult<u32> {
    u32::try_from(index).map_err(|_| CuError::from("Execution plan has too many steps"))
}

impl CuMissionPlan {
    pub(super) fn serial_keys(&self) -> CuResult<Vec<String>> {
        if self.copperlists_per_cycle != 1
            || self.lanes.len() != 1
            || self.lanes[0].placement != CuPlanPlacement::Main
        {
            return Err(CuError::from(
                "This plan is valid but requires a multicore/multi-CopperList executor. Fixed currently executes one main lane and one CopperList per cycle; placement and dependencies will not be ignored.",
            ));
        }
        self.lanes[0]
            .steps
            .iter()
            .map(|&index| {
                self.steps
                    .get(index as usize)
                    .map(|step| step.key.clone())
                    .ok_or_else(|| CuError::from("Unknown lane step"))
            })
            .collect()
    }

    pub(super) fn validate(&self, config: &CuConfig, shape: &PlanShape) -> CuResult<()> {
        if self.copperlists_per_cycle == 0 {
            return Err(CuError::from("copperlists_per_cycle must be positive"));
        }
        let expected = shape
            .keys
            .len()
            .checked_mul(self.copperlists_per_cycle as usize)
            .ok_or_else(|| CuError::from("Execution plan step count overflow"))?;
        if self.steps.len() != expected {
            return Err(CuError::from(format!(
                "Expected {expected} process-step occurrences, found {}",
                self.steps.len()
            )));
        }
        let by_key: BTreeMap<_, _> = shape
            .keys
            .iter()
            .enumerate()
            .map(|(i, key)| (key, i))
            .collect();
        let mut occurrences = BTreeMap::new();
        for (index, step) in self.steps.iter().enumerate() {
            let key = *by_key
                .get(&step.key)
                .ok_or_else(|| CuError::from(format!("Unknown process step '{}'", step.key)))?;
            if step.copperlist >= self.copperlists_per_cycle {
                return Err(CuError::from(format!(
                    "CopperList offset {} is outside the schedule cycle",
                    step.copperlist
                )));
            }
            if occurrences.insert((key, step.copperlist), index).is_some() {
                return Err(CuError::from(format!(
                    "Duplicate process step '{}' in CopperList {}",
                    step.key, step.copperlist
                )));
            }
        }
        if self.lanes.is_empty() {
            return Err(CuError::from("Execution plan needs at least one lane"));
        }
        let mut assigned = vec![false; self.steps.len()];
        let mut placements = Vec::new();
        let mut edges = vec![Vec::new(); self.steps.len()];
        for lane in &self.lanes {
            if placements.contains(&&lane.placement) {
                return Err(CuError::from(
                    "Two lanes cannot claim the same execution context",
                ));
            }
            placements.push(&lane.placement);
            if let CuPlanPlacement::Worker { pool, index } = &lane.placement {
                let spec = config
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.thread_pools.iter().find(|spec| &spec.id == pool))
                    .ok_or_else(|| CuError::from(format!("Unknown worker pool '{pool}'")))?;
                if *index as usize >= spec.threads {
                    return Err(CuError::from(format!(
                        "Worker {index} is outside pool '{pool}' ({} workers)",
                        spec.threads
                    )));
                }
            }
            if lane.steps.is_empty() {
                return Err(CuError::from("Execution lanes cannot be empty"));
            }
            for &index in &lane.steps {
                let seen = assigned
                    .get_mut(index as usize)
                    .ok_or_else(|| CuError::from(format!("Unknown lane step index {index}")))?;
                if core::mem::replace(seen, true) {
                    return Err(CuError::from(format!(
                        "Step index {index} is assigned more than once"
                    )));
                }
            }
            for pair in lane.steps.windows(2) {
                edges[pair[0] as usize].push((pair[1] as usize, 0));
            }
            if let (Some(first), Some(last)) = (lane.steps.first(), lane.steps.last()) {
                edges[*last as usize].push((*first as usize, 1));
            }
        }
        if assigned.iter().any(|assigned| !assigned) {
            return Err(CuError::from(
                "Every process-step occurrence must be assigned to a lane",
            ));
        }
        for dependency in &self.dependencies {
            if dependency.from as usize >= edges.len() || dependency.to as usize >= edges.len() {
                return Err(CuError::from("Dependency references an unknown step index"));
            }
            edges[dependency.from as usize].push((dependency.to as usize, dependency.cycle_lag));
        }
        let order = topological_order(&edges)?;
        let mut rank = vec![0; order.len()];
        for (position, &step) in order.iter().enumerate() {
            rank[step] = position;
        }
        let require = |from: usize, to: usize, lag: u32| -> CuResult<()> {
            if !reaches(&edges, from, to, lag) {
                return Err(CuError::from(format!(
                    "Missing precedence: '{}' (CL {}) must finish before '{}' (CL {}, cycle lag {lag})",
                    self.steps[from].key,
                    self.steps[from].copperlist,
                    self.steps[to].key,
                    self.steps[to].copperlist,
                )));
            }
            Ok(())
        };
        for cl in 0..self.copperlists_per_cycle {
            for &(from, to) in &shape.required {
                require(occurrences[&(from, cl)], occurrences[&(to, cl)], 0)?;
            }
        }
        // A task or bridge instance has one mutable state across all CLs.
        // Require ordered calls within each CL, then last(previous CL) before
        // first(next CL), including across repeating schedule boundaries.
        for component in &shape.components {
            let mut endpoints = Vec::new();
            for cl in 0..self.copperlists_per_cycle {
                let mut steps: Vec<_> = component
                    .iter()
                    .map(|&key| occurrences[&(key, cl)])
                    .collect();
                steps.sort_by_key(|&step| rank[step]);
                for pair in steps.windows(2) {
                    require(pair[0], pair[1], 0)?;
                }
                if let (Some(&first), Some(&last)) = (steps.first(), steps.last()) {
                    endpoints.push((first, last));
                }
            }
            for pair in endpoints.windows(2) {
                require(pair[0].1, pair[1].0, 0)?;
            }
            if let (Some(first), Some(last)) = (endpoints.first(), endpoints.last()) {
                require(last.1, first.0, 1)?;
            }
        }
        Ok(())
    }
}

/// Only zero-lag edges can deadlock a periodic schedule: positive-lag edges
/// point from earlier cycles, with the initial boundary already satisfied.
fn topological_order(edges: &[Vec<(usize, u32)>]) -> CuResult<Vec<usize>> {
    let mut incoming = vec![0usize; edges.len()];
    for outgoing in edges {
        for &(to, lag) in outgoing {
            if lag == 0 {
                incoming[to] += 1;
            }
        }
    }
    let mut ready: VecDeque<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(i, &count)| (count == 0).then_some(i))
        .collect();
    let mut order = Vec::with_capacity(edges.len());
    while let Some(from) = ready.pop_front() {
        order.push(from);
        for &(to, lag) in &edges[from] {
            if lag == 0 {
                incoming[to] -= 1;
                if incoming[to] == 0 {
                    ready.push_back(to);
                }
            }
        }
    }
    if order.len() != edges.len() {
        return Err(CuError::from(
            "Execution plan contains a dependency/lane-order cycle",
        ));
    }
    Ok(order)
}

/// Required constraints span at most one cycle boundary. Traverse two layers
/// instead of allocating a quadratic transitive-closure matrix.
fn reaches(edges: &[Vec<(usize, u32)>], from: usize, to: usize, lag: u32) -> bool {
    if edges[from].contains(&(to, lag)) {
        return true;
    }
    let mut visited = vec![false; edges.len() * (lag as usize + 1)];
    let mut ready = vec![(from, 0u32)];
    visited[from] = true;
    while let Some((node, elapsed)) = ready.pop() {
        for &(next, distance) in &edges[node] {
            let Some(next_lag) = elapsed.checked_add(distance).filter(|&sum| sum <= lag) else {
                continue;
            };
            if next == to && next_lag == lag {
                return true;
            }
            let index = next_lag as usize * edges.len() + next;
            if !visited[index] {
                visited[index] = true;
                ready.push((next, next_lag));
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::CuPlan;
    use crate::planner::Fixed;
    use crate::planner::assemble_runtime_plan_for_mission;

    fn chain() -> CuConfig {
        CuConfig::deserialize_ron(
            r#"(
            runtime: (thread_pools: [(id: "rt", threads: 4, affinity: [0, 1])]),
            tasks: [(id: "src", type: "Source"), (id: "sink", type: "Sink")],
            cnx: [(src: "src", dst: "sink", msg: "u32")],
        )"#,
        )
        .unwrap()
    }

    fn worker(index: u32, steps: Vec<u32>) -> CuPlanLane {
        CuPlanLane {
            placement: CuPlanPlacement::Worker {
                pool: "rt".into(),
                index,
            },
            steps,
        }
    }

    fn pipeline() -> (CuConfig, CuPlan) {
        let config = chain();
        let mut plan = CuPlan::from_config(&config).unwrap();
        let mission = plan.missions.get_mut("default").unwrap();
        let first = mission.steps.clone();
        mission.steps.extend(first.into_iter().map(|mut step| {
            step.copperlist = 1;
            step
        }));
        mission.copperlists_per_cycle = 2;
        mission.lanes = vec![worker(0, vec![0, 2]), worker(1, vec![1, 3])];
        mission.dependencies = vec![
            CuPlanDependency {
                from: 0,
                to: 1,
                cycle_lag: 0,
            },
            CuPlanDependency {
                from: 2,
                to: 3,
                cycle_lag: 0,
            },
        ];
        // Lane order and wrap supply the per-component state constraints.
        (config, plan)
    }

    #[test]
    fn multicore_multicl_plan_round_trips_but_is_not_silently_executed_serially() {
        let (mut config, plan) = pipeline();
        plan.validate(&config).unwrap();
        let loaded = CuPlan::deserialize_ron(&plan.serialize_ron().unwrap()).unwrap();
        assert_eq!(loaded, plan);
        Fixed::new(loaded).unwrap().apply(&mut config).unwrap();
        let config = CuConfig::deserialize_ron(&config.serialize_ron().unwrap()).unwrap();
        assert_eq!(CuPlan::from_config(&config).unwrap(), plan);
        let err =
            assemble_runtime_plan_for_mission(&config, config.get_graph(None).unwrap(), "default")
                .err()
                .unwrap();
        assert!(
            err.to_string()
                .contains("requires a multicore/multi-CopperList executor"),
            "{err}"
        );
    }

    #[test]
    fn rejects_malformed_inventory_lanes_and_dependencies() {
        let (config, plan) = pipeline();
        for mutation in 0..12 {
            let mut invalid = plan.clone();
            let mission = invalid.missions.get_mut("default").unwrap();
            match mutation {
                0 => {
                    mission.steps.pop();
                }
                1 => {
                    mission.steps[0].key = "unknown".into();
                }
                2 => {
                    mission.steps[0].copperlist = 2;
                }
                3 => {
                    mission.steps[2] = mission.steps[0].clone();
                }
                4 => {
                    mission.lanes[0].steps.pop();
                }
                5 => {
                    mission.lanes[0].steps.push(0);
                }
                6 => {
                    mission.lanes[0].steps.push(99);
                }
                7 => {
                    mission.lanes[0].placement = CuPlanPlacement::Worker {
                        pool: "missing".into(),
                        index: 0,
                    };
                }
                8 => {
                    mission.lanes[0].placement = CuPlanPlacement::Worker {
                        pool: "rt".into(),
                        index: 4,
                    };
                }
                9 => {
                    mission.lanes[1].placement = mission.lanes[0].placement.clone();
                }
                10 => {
                    mission.dependencies[0].to = 99;
                }
                _ => {
                    mission.copperlists_per_cycle = 0;
                }
            }
            assert!(invalid.validate(&config).is_err(), "mutation {mutation}");
        }
    }

    #[test]
    fn rejects_missing_data_precedence_and_zero_lag_cycles() {
        let (config, plan) = pipeline();
        let mut missing = plan.clone();
        missing
            .missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .clear();
        let err = missing.validate(&config).unwrap_err();
        assert!(err.to_string().contains("Missing precedence"), "{err}");
        let mut cyclic = plan;
        cyclic
            .missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .push(CuPlanDependency {
                from: 1,
                to: 0,
                cycle_lag: 0,
            });
        let err = cyclic.validate(&config).unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    #[test]
    fn task_state_requires_precedence_between_cls_and_across_cycle_boundary() {
        let (config, mut plan) = pipeline();
        let mission = plan.missions.get_mut("default").unwrap();
        mission.lanes = (0..4).map(|index| worker(index, vec![index])).collect();
        assert!(plan.validate(&config).is_err());
        plan.missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .extend([
                CuPlanDependency {
                    from: 0,
                    to: 2,
                    cycle_lag: 0,
                },
                CuPlanDependency {
                    from: 1,
                    to: 3,
                    cycle_lag: 0,
                },
            ]);
        // Each worker's self-wrap alone cannot order distinct occurrences of
        // the same task instance across the two-CL cycle boundary.
        assert!(plan.validate(&config).is_err());
        plan.missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .extend([
                CuPlanDependency {
                    from: 2,
                    to: 0,
                    cycle_lag: 1,
                },
                CuPlanDependency {
                    from: 3,
                    to: 1,
                    cycle_lag: 1,
                },
            ]);
        plan.validate(&config).unwrap();
    }

    #[test]
    fn bridge_channels_share_state_even_without_a_message_edge_between_them() {
        let config = CuConfig::deserialize_ron(r#"(
            runtime: (thread_pools: [(id: "rt", threads: 2)]),
            tasks: [(id: "left", type: "Sink"), (id: "right", type: "Sink")],
            bridges: [(id: "radio", type: "Radio", channels: [Rx(id: "a"), Rx(id: "b")])],
            cnx: [(src: "radio/a", dst: "left", msg: "u32"), (src: "radio/b", dst: "right", msg: "u32")],
        )"#).unwrap();
        let mut plan = CuPlan::from_config(&config).unwrap();
        let mission = plan.missions.get_mut("default").unwrap();
        let index = |needle: &str| {
            mission
                .steps
                .iter()
                .position(|step| step.key.contains(needle))
                .unwrap() as u32
        };
        let (a, b, left, right) = (
            index("bridge:radio:rx:a|"),
            index("bridge:radio:rx:b|"),
            index("task:left|"),
            index("task:right|"),
        );
        mission.lanes = vec![worker(0, vec![a, left]), worker(1, vec![b, right])];
        mission.dependencies = vec![
            CuPlanDependency {
                from: a,
                to: left,
                cycle_lag: 0,
            },
            CuPlanDependency {
                from: b,
                to: right,
                cycle_lag: 0,
            },
        ];
        assert!(plan.validate(&config).is_err());
        plan.missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .extend([
                CuPlanDependency {
                    from: a,
                    to: b,
                    cycle_lag: 0,
                },
                CuPlanDependency {
                    from: b,
                    to: a,
                    cycle_lag: 1,
                },
            ]);
        plan.validate(&config).unwrap();
    }

    #[test]
    fn serial_order_remaps_message_slots_and_preserves_input_order() {
        let mut config = CuConfig::deserialize_ron(r#"(
            tasks: [(id: "left", type: "Source"), (id: "right", type: "Source"), (id: "join", type: "Join", anytime: (max_refines: 2)), (id: "sink", type: "Sink")],
            cnx: [(src: "left", dst: "join", msg: "u32"), (src: "right", dst: "join", msg: "u32"), (src: "join", dst: "sink", msg: "u32")],
        )"#).unwrap();
        let mut plan = CuPlan::from_config(&config).unwrap();
        plan.missions.get_mut("default").unwrap().lanes[0]
            .steps
            .swap(0, 1);
        plan.validate(&config).unwrap();
        Fixed::new(plan.clone())
            .unwrap()
            .apply(&mut config)
            .unwrap();
        let assembled =
            assemble_runtime_plan_for_mission(&config, config.get_graph(None).unwrap(), "default")
                .unwrap();
        let CuExecutionUnit::Step(join) = &assembled.execution.steps[2] else {
            panic!("join step")
        };
        assert_eq!(join.input_msg_indices_types[0].culist_index, 1);
        assert_eq!(join.input_msg_indices_types[1].culist_index, 0);
        let mut reversed_phases = plan.clone();
        reversed_phases.missions.get_mut("default").unwrap().lanes[0]
            .steps
            .swap(3, 4);
        assert!(reversed_phases.validate(&config).is_err());
        let mut early_consumer = plan;
        early_consumer.missions.get_mut("default").unwrap().lanes[0]
            .steps
            .swap(4, 5);
        assert!(early_consumer.validate(&config).is_err());
    }

    #[test]
    fn fixed_materialization_preserves_explicit_anytime_interleaving() {
        let mut config = CuConfig::deserialize_ron(r#"(
            tasks: [(id: "a", type: "Source", anytime: (max_refines: 1)),
                (id: "b", type: "Source"), (id: "sink_a", type: "Sink"), (id: "sink_b", type: "Sink")],
            cnx: [(src: "a", dst: "sink_a", msg: "u32"), (src: "b", dst: "sink_b", msg: "u32")],
        )"#).unwrap();
        let mut plan = CuPlan::from_config(&config).unwrap();
        let requested: Vec<_> = [
            "a|phase:base",
            "b|phase:whole",
            "sink_b|phase:whole",
            "a|phase:refine:1",
            "sink_a|phase:whole",
        ]
        .into_iter()
        .map(|key| format!("mission:default|task:{key}"))
        .collect();
        let mission = plan.missions.get_mut("default").unwrap();
        mission.lanes[0].steps = requested
            .iter()
            .map(|key| {
                mission
                    .steps
                    .iter()
                    .position(|step| &step.key == key)
                    .unwrap() as u32
            })
            .collect();
        Fixed::new(plan).unwrap().apply(&mut config).unwrap();
        let assembled =
            assemble_runtime_plan_for_mission(&config, config.get_graph(None).unwrap(), "default")
                .unwrap();
        assert_eq!(execution_keys(&assembled, "default").unwrap(), requested);
    }

    #[test]
    fn fine_grained_fork_join_and_anytime_phases_can_use_distinct_workers() {
        let config = CuConfig::deserialize_ron(r#"(
            runtime: (thread_pools: [(id: "rt", threads: 2)]),
            tasks: [(id: "left", type: "Source"), (id: "right", type: "Source"), (id: "join", type: "Join", anytime: (max_refines: 2)), (id: "sink", type: "Sink")],
            cnx: [(src: "left", dst: "join", msg: "u32"), (src: "right", dst: "join", msg: "u32"), (src: "join", dst: "sink", msg: "u32")],
        )"#).unwrap();
        let mut plan = CuPlan::from_config(&config).unwrap();
        let mission = plan.missions.get_mut("default").unwrap();
        // left -> base -> refine:2 -> sink on worker 0;
        // right -> refine:1 on worker 1. Explicit edges synchronize the join
        // and task state, without serializing the independent sources.
        mission.lanes = vec![worker(0, vec![0, 2, 4, 5]), worker(1, vec![1, 3])];
        plan.validate(&config).unwrap();
        plan.missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .retain(|edge| !(edge.from == 2 && edge.to == 3 && edge.cycle_lag == 0));
        assert!(plan.validate(&config).is_err());
    }

    #[test]
    fn versions_missions_and_unknown_fields_are_checked() {
        let mut config = CuConfig::deserialize_ron(
            r#"(
            missions: [(id: "alpha"), (id: "beta")],
            tasks: [(id: "src", type: "Source", kind: source)],
        )"#,
        )
        .unwrap();
        let plan = CuPlan::from_config(&config).unwrap();
        Fixed::new(plan.clone())
            .unwrap()
            .apply(&mut config)
            .unwrap();
        assert_eq!(CuPlan::from_config(&config).unwrap(), plan);
        let mut missing = plan.clone();
        missing.missions.remove("alpha");
        assert!(missing.validate(&config).is_err());
        let mut future = plan;
        future.version += 1;
        assert!(future.validate(&config).is_err());
        assert!(CuPlan::deserialize_ron("(version: 1, missions: {}, unknown: 0)").is_err());
    }

    #[cfg(feature = "std")]
    #[test]
    fn file_round_trip_and_failed_apply_leave_config_unchanged() {
        let (mut config, plan) = pipeline();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multicore.ron");
        plan.write(&path).unwrap();
        assert_eq!(CuPlan::read(&path).unwrap(), plan);
        let original = config.serialize_ron().unwrap();
        let mut invalid = plan;
        invalid
            .missions
            .get_mut("default")
            .unwrap()
            .dependencies
            .clear();
        assert!(Fixed::new(invalid).unwrap().apply(&mut config).is_err());
        assert_eq!(config.serialize_ron().unwrap(), original);
    }
}
