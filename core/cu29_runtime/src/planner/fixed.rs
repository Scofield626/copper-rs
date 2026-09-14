//! Portable, exact process schedules and their build-time enforcement.

use super::AssembledPlan;
use super::CuMissionPlan;
use super::FIXED_PLANNER;
use super::Linearity;
use super::StepOrder;
use super::assemble_from_order;
use super::assemble_runtime_plan_for_mission;
use super::assemble_runtime_plan_with_planner;
use super::build_plan_graph;
use super::configured_fixed_plan;
use super::mission_graphs;
use super::schedule::PlanShape;
use super::step_key;
use crate::config::ComponentConfig;
use crate::config::CuConfig;
use crate::config::CuGraph;
use crate::config::NodeId;
use crate::config::PlannerConfig;
use crate::config::RuntimeConfig;
use crate::config::Value;
use crate::curuntime::CuExecutionUnit;
use crate::curuntime::CuStepPhase;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use cu29_traits::CuError;
use cu29_traits::CuResult;
use serde::Deserialize;
use serde::Serialize;

const PLAN_VERSION: u32 = 1;

/// An experimental, portable process schedule for every mission in an app.
///
/// Version 1 represents serial and multicore schedules, including schedules
/// spanning several CopperLists. Each mission has a step inventory, execution
/// lanes, and precedence constraints across lanes and repeating cycles.
/// Entries retain existing error handling and anytime budget/skip semantics;
/// fixing their order does not force optional refinements to run.
///
/// This is scheduling input, not serialized Rust code or message storage.
/// Copper validates it against the task graph and generates typed calls and
/// CopperList slots at compile time. Background work retains its configured
/// execution semantics. Multicore representation and validation are available;
/// execution currently supports only the serial subset through [`Fixed`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuPlan {
    /// Format version. Unsupported versions are rejected before use.
    pub version: u32,
    /// Schedule indexed by mission id (`default` without named missions).
    pub missions: BTreeMap<String, CuMissionPlan>,
}

impl CuPlan {
    /// Export the effective generated schedule, including any baked custom
    /// planner order or fixed plan already present in `config`.
    pub fn from_config(config: &CuConfig) -> CuResult<Self> {
        if let Some(plan) = configured_fixed_plan(config)? {
            plan.validate(config)?;
            return Ok(plan);
        }
        let mut missions = BTreeMap::new();
        for (mission, graph) in mission_graphs(config) {
            let plan = assemble_runtime_plan_for_mission(config, graph, &mission)?;
            missions.insert(
                mission.clone(),
                PlanShape::new(&plan, &mission)?.serial_plan()?,
            );
        }
        Ok(Self {
            version: PLAN_VERSION,
            missions,
        })
    }

    /// Validate inventory, lane placement, acyclic precedence, message
    /// dependencies, anytime phases, and mutable task/bridge state order
    /// within and across CopperLists. This does not select an executor.
    ///
    /// CPU availability, runtime memory sizing, execution times, and deadline
    /// feasibility are not properties checked by this structural validator.
    pub fn validate(&self, config: &CuConfig) -> CuResult<()> {
        self.check_version()?;
        let missions = mission_graphs(config);
        if !self.missions.keys().eq(missions.iter().map(|(id, _)| id)) {
            return Err(CuError::from(
                "Execution plan must contain exactly the configured missions",
            ));
        }
        for (mission, graph) in missions {
            let canonical = assemble_runtime_plan_with_planner(config, graph, &Linearity)?;
            let shape = PlanShape::new(&canonical, &mission)?;
            self.missions[&mission]
                .validate(config, &shape)
                .map_err(|e| CuError::from(format!("Plan for mission '{mission}': {e}")))?;
        }
        Ok(())
    }

    /// Serialize a plan as human-editable RON. Graph legality is checked by
    /// [`Fixed::apply`], not by serialization.
    pub fn serialize_ron(&self) -> CuResult<String> {
        self.check_version()?;
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .map_err(|e| CuError::new_with_cause("Could not serialize execution plan", e))
    }

    /// Parse a saved plan and reject unsupported format versions.
    pub fn deserialize_ron(text: &str) -> CuResult<Self> {
        let plan: Self = ron::from_str(text)
            .map_err(|e| CuError::new_with_cause("Could not parse execution plan", e))?;
        plan.check_version()?;
        Ok(plan)
    }

    /// Read a standalone plan file in host tooling or a build script.
    #[cfg(feature = "std")]
    pub fn read(path: &std::path::Path) -> CuResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CuError::new_with_cause("Could not read execution plan", e))?;
        Self::deserialize_ron(&text)
    }

    /// Write a standalone plan file in host tooling or a build script.
    #[cfg(feature = "std")]
    pub fn write(&self, path: &std::path::Path) -> CuResult<()> {
        std::fs::write(path, self.serialize_ron()?)
            .map_err(|e| CuError::new_with_cause("Could not write execution plan", e))
    }

    fn check_version(&self) -> CuResult<()> {
        if self.version != PLAN_VERSION {
            return Err(CuError::from(format!(
                "Unsupported execution plan version {}; expected {PLAN_VERSION}",
                self.version
            )));
        }
        Ok(())
    }
}

/// Enforce an exact saved process schedule during runtime generation.
///
/// Select `cu29::planner::Fixed` with `config: { "plan": { ... } }`, or use
/// [`Fixed::apply`] to produce that configuration from a [`CuPlan`]. Unlike
/// [`super::Pinned`], no bridge placement or anytime scheduling heuristic is
/// applied to the supplied sequence. Invalid plans are errors, never hints.
///
/// This consumes an already scheduled plan; [`super::CuPlanner`] remains the
/// extension point for heuristics that choose a graph-node order. There is
/// no runtime dispatcher, file I/O, or serialization on the execution path.
/// Multicore plans may be validated and embedded, but runtime generation
/// rejects them until an executor implements their placement and dependencies.
pub struct Fixed {
    plan: CuPlan,
}

impl Fixed {
    /// Take ownership of a saved plan. Its graph constraints are validated
    /// when it is applied or consumed during runtime generation.
    pub fn new(plan: CuPlan) -> CuResult<Self> {
        plan.check_version()?;
        Ok(Self { plan })
    }

    /// Validate all missions and select this fixed plan in `config`.
    ///
    /// Serialize the resulting config and compile the app against it. Changing
    /// a deployed app's startup config cannot change its compiled schedule.
    /// The config is left untouched if validation fails.
    ///
    /// ```
    /// use cu29_runtime::config::CuConfig;
    /// use cu29_runtime::planner::{CuPlan, Fixed};
    /// # fn main() -> cu29_traits::CuResult<()> {
    /// let mut config = CuConfig::deserialize_ron(
    ///     r#"(tasks: [(id: "sensor", type: "Sensor", kind: source)])"#,
    /// )?;
    /// let plan = CuPlan::from_config(&config)?;
    /// Fixed::new(plan)?.apply(&mut config)?;
    /// let config_for_compilation = config.serialize_ron()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn apply(&self, config: &mut CuConfig) -> CuResult<()> {
        self.plan.validate(config)?;
        let value = cu29_value::to_value(&self.plan)
            .map_err(|e| CuError::new_with_cause("Could not encode execution plan parameter", e))?;
        let value = Value::deserialize(value)
            .map_err(|e| CuError::new_with_cause("Could not embed execution plan", e))?;
        let mut params = ComponentConfig::default();
        params.set("plan", value);
        config
            .runtime
            .get_or_insert_with(RuntimeConfig::default)
            .planner = Some(PlannerConfig {
            type_: FIXED_PLANNER.to_string(),
            config: Some(params),
            resolved: None,
        });
        Ok(())
    }

    pub(super) fn assemble(
        &self,
        config: &CuConfig,
        graph: &CuGraph,
        mission: &str,
    ) -> CuResult<AssembledPlan> {
        self.plan.validate(config)?;
        let result = self.assemble_steps(config, graph, mission);
        result.map_err(|e| CuError::from(format!("Fixed plan for mission '{mission}': {e}")))
    }

    fn assemble_steps(
        &self,
        config: &CuConfig,
        graph: &CuGraph,
        mission: &str,
    ) -> CuResult<AssembledPlan> {
        let requested = self
            .plan
            .missions
            .get(mission)
            .ok_or_else(|| CuError::from("Missing mission"))?
            .serial_keys()?;
        // The canonical expansion enumerates legal identities, independent of
        // the requested order. It is never substituted for the user's order.
        let canonical = assemble_runtime_plan_with_planner(config, graph, &Linearity)?;
        let keys = execution_keys(&canonical, mission)?;
        let by_key: BTreeMap<_, _> = keys.iter().enumerate().map(|(i, key)| (key, i)).collect();
        let mut node_order = Vec::with_capacity(canonical.entities.len());
        for key in &requested {
            let index = *by_key
                .get(key)
                .ok_or_else(|| CuError::from(format!("Unknown process step '{key}'")))?;
            let CuExecutionUnit::Step(step) = &canonical.execution.steps[index] else {
                return Err(CuError::from("Nested loops cannot be materialized"));
            };
            if step.phase != CuStepPhase::AnytimeRefine {
                node_order.push(step.node_id);
            }
        }
        // Recompute message slots from the supplied base-node order, then
        // move the generated typed steps into the exact supplied phase order.
        let mut assembled =
            assemble_from_order(build_plan_graph(config, graph)?, StepOrder(node_order))?;
        let remapped_keys = execution_keys(&assembled, mission)?;
        let mut units: BTreeMap<_, _> = remapped_keys
            .into_iter()
            .zip(assembled.execution.steps)
            .collect();
        assembled.execution.steps = requested
            .iter()
            .map(|key| {
                units.remove(key).ok_or_else(|| {
                    CuError::from(format!("Could not materialize process step '{key}'"))
                })
            })
            .collect::<CuResult<Vec<_>>>()?;
        Ok(assembled)
    }
}

pub(super) fn execution_keys(plan: &AssembledPlan, mission: &str) -> CuResult<Vec<String>> {
    let mut refines: BTreeMap<NodeId, u32> = BTreeMap::new();
    plan.execution
        .steps
        .iter()
        .map(|unit| {
            let CuExecutionUnit::Step(step) = unit else {
                return Err(CuError::from(
                    "Nested loops are not supported by plan version 1",
                ));
            };
            let ordinal = if step.phase == CuStepPhase::AnytimeRefine {
                let next = refines.entry(step.node_id).or_default();
                *next += 1;
                Some(*next)
            } else {
                None
            };
            Ok(step_key(
                mission,
                &plan.entities[step.node_id as usize],
                step.phase,
                ordinal,
            ))
        })
        .collect()
}
