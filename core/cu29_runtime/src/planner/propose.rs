//! Candidate plans from a profile and a contract: the response-time model
//! with the worker as its unit, the contract's objective, and a local search
//! over which worker runs each occurrence and in which order.

use super::CuContract;
use super::CuMissionPlan;
use super::CuObjectiveKind;
use super::CuPlan;
use super::CuPlanPlacement;
use super::CuPlanThread;
use super::CuPlanWorker;
use super::CuProfile;
use crate::config::CuConfig;
use crate::config::SchedulingPolicy;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use cu29_traits::CuError;
use cu29_traits::CuResult;
use serde::Deserialize;
use serde::Serialize;

/// Response times past this many periods count as unbounded.
const RESPONSE_CAP_PERIODS: u64 = 20;
/// Moves without improvement before a restart jumps back to its best plan.
const PATIENCE: usize = 120;
/// Unconditional moves applied after such a jump.
const KICK: usize = 3;
/// Two candidates whose sum tier differs by less than this fraction count as
/// the same candidate.
const MIN_SEPARATION: f64 = 0.01;
/// A delivered rate this close to nominal counts as kept.
const RATE_TOLERANCE: f64 = 0.005;

/// What the proposer is asked to do.
#[derive(Clone, Debug)]
pub struct ProposeRequest<'a> {
    pub config: &'a CuConfig,
    pub mission: &'a str,
    pub contract: &'a CuContract,
    pub profile: &'a CuProfile,
    /// CopperLists per cycle of every candidate.
    pub copperlists_per_cycle: u32,
    /// How many distinct candidates to return.
    pub candidates: usize,
    pub seed: u64,
    pub moves: usize,
    pub restarts: usize,
}

/// One candidate: its plan and what the model predicts for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CuCandidate {
    pub plan: CuPlan,
    pub prediction: CuPrediction,
}

/// The model's prediction for one plan of one mission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuPrediction {
    /// The objective's tiers, smallest first is better.
    pub score: Vec<f64>,
    pub chains: BTreeMap<String, CuChainPrediction>,
    pub workers: BTreeMap<String, CuWorkerPrediction>,
    /// Predicted delivered fraction per contract source.
    pub sources: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuChainPrediction {
    pub latency_ns: u64,
    pub deadline_ns: u64,
    /// `latency / deadline`.
    pub ratio: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuWorkerPrediction {
    pub cpu: usize,
    /// Cost per cycle over the cycle period.
    pub load: f64,
    /// Response time of one cycle, `None` when it does not converge.
    pub response_ns: Option<u64>,
    /// Delivered fraction of cycles.
    pub rate: f64,
}

/// The model's view of one mission plan's inventory: cost per unit,
/// dependency edges, and what each chain and source maps to.
///
/// A unit is one occurrence, or an anytime base occurrence with its refine
/// occurrences: the executor runs those phases together on one worker, so
/// the search never separates them.
struct Model {
    inventory: CuMissionPlan,
    /// Occurrence indices of each unit, in phase order.
    units: Vec<Vec<usize>>,
    /// Expected cost of each unit per cycle, in nanoseconds.
    cost: Vec<u64>,
    /// Zero-lag predecessor units of each unit.
    preds: Vec<Vec<usize>>,
    /// CopperList period, the dispatch granularity `g`.
    copperlist_ns: u64,
    /// Cycle period `T`.
    cycle_ns: u64,
    /// Per chain: `(source occurrence, sink occurrence)` per CL offset.
    chains: Vec<Vec<(usize, usize)>>,
    chain_deadlines: Vec<u32>,
    /// Per contract source: its occurrences.
    sources: Vec<Vec<usize>>,
    cpus: Vec<usize>,
    /// CopperLists in flight at once, at least one cycle's worth.
    max_in_flight: u32,
    policy: SchedulingPolicy,
    deadline_objective: bool,
    margin: f64,
}

/// A candidate under search: each worker is one CPU with an ordered list of
/// occurrences.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Assignment {
    lanes: Vec<Vec<usize>>,
}

struct Evaluation {
    score: Vec<f64>,
    ends: Vec<u64>,
    starts: Vec<u64>,
    response: Vec<Option<u64>>,
    rate: Vec<f64>,
    load: Vec<f64>,
}

impl Model {
    fn new(request: &ProposeRequest<'_>, inventory: CuMissionPlan) -> CuResult<Self> {
        let contract = request.contract;
        let profile = request.profile;
        let copperlist_ns = match request
            .config
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.rate_target_hz)
        {
            Some(rate) => 1_000_000_000 / rate.max(1),
            None => profile
                .window_ns
                .checked_div(profile.copperlists.max(1))
                .unwrap_or(0)
                .max(1),
        };
        let background: BTreeSet<&str> = inventory
            .background
            .iter()
            .map(|entry| entry.task.as_str())
            .collect();
        // A refine occurrence joins the unit of its task's base occurrence in
        // the same CopperList; the profile measures the whole job under the
        // base key.
        let mut units: Vec<Vec<usize>> = Vec::new();
        let mut unit_of = vec![usize::MAX; inventory.steps.len()];
        for (index, step) in inventory.steps.iter().enumerate() {
            if is_refine(&step.key) {
                let base = inventory.steps.iter().position(|other| {
                    other.copperlist == step.copperlist
                        && !is_refine(&other.key)
                        && task_of_key(&other.key) == task_of_key(&step.key)
                });
                let Some(base) = base.map(|b| unit_of[b]).filter(|&u| u != usize::MAX) else {
                    return Err(CuError::from(format!(
                        "Refine step '{}' has no base step before it",
                        step.key
                    )));
                };
                units[base].push(index);
                unit_of[index] = base;
            } else {
                unit_of[index] = units.len();
                units.push(vec![index]);
            }
        }
        let mut cost = Vec::with_capacity(units.len());
        for unit in &units {
            let step = &inventory.steps[unit[0]];
            let task = task_of_key(&step.key);
            if task.is_some_and(|task| background.contains(task)) {
                // The gateway only publishes and dispatches; the compute runs
                // on its pool.
                cost.push(0);
                continue;
            }
            let operation = profile.operations.get(&step.key).ok_or_else(|| {
                CuError::from(format!("The profile has no measurement for '{}'", step.key))
            })?;
            let fired = (operation.firing_rate_hz * copperlist_ns as f64 / 1e9).clamp(0.0, 1.0);
            let expected =
                operation.fired.mean_ns * fired + operation.skipped.mean_ns * (1.0 - fired);
            cost.push(expected as u64);
        }
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
        for edge in &inventory.dependencies {
            let (from, to) = (unit_of[edge.from as usize], unit_of[edge.to as usize]);
            if edge.cycle_lag == 0 && from != to && !preds[to].contains(&from) {
                preds[to].push(from);
            }
        }
        // Running an anytime task's phases together must not close a cycle
        // through another component (a resource or state edge into a
        // refinement and out of the base).
        if let Some(unit) = first_unit_on_a_cycle(&preds) {
            return Err(CuError::from(format!(
                "Step '{}' and its refinements cannot run together: an edge leads out of one of its phases and back into an earlier one through another component",
                inventory.steps[units[unit][0]].key
            )));
        }
        // A task's unit at each CopperList offset.
        let units_of = |task: &str| -> Vec<usize> {
            (0..inventory.copperlists_per_cycle)
                .filter_map(|offset| {
                    inventory
                        .steps
                        .iter()
                        .position(|step| {
                            step.copperlist == offset
                                && !is_refine(&step.key)
                                && task_of_key(&step.key) == Some(task)
                        })
                        .map(|index| unit_of[index])
                })
                .collect()
        };
        let mut chains = Vec::new();
        for chain in &contract.chains {
            let sources = units_of(&chain.source);
            let sinks = units_of(&chain.sink);
            if sources.len() != inventory.copperlists_per_cycle as usize
                || sinks.len() != sources.len()
            {
                return Err(CuError::from(format!(
                    "Chain '{}' names a task without a whole-phase step",
                    chain.id
                )));
            }
            chains.push(sources.into_iter().zip(sinks).collect());
        }
        let sources = contract
            .sources
            .iter()
            .map(|source| units_of(&source.task))
            .collect();
        let copperlists_per_cycle = inventory.copperlists_per_cycle;
        Ok(Self {
            cycle_ns: copperlist_ns * u64::from(copperlists_per_cycle),
            inventory,
            units,
            cost,
            preds,
            copperlist_ns,
            chains,
            chain_deadlines: contract
                .chains
                .iter()
                .map(|chain| chain.deadline_ms)
                .collect(),
            sources,
            cpus: contract.cpus.clone(),
            max_in_flight: contract.max_in_flight.max(copperlists_per_cycle),
            policy: contract.worker_policy,
            deadline_objective: contract.objective.kind == CuObjectiveKind::Deadline,
            margin: contract.objective.margin,
        })
    }

    /// Whether a lane order respects every zero-lag edge among its own
    /// occurrences and the whole cycle graph stays acyclic.
    fn is_valid(&self, assignment: &Assignment) -> bool {
        let n = self.units.len();
        let mut incoming = vec![0usize; n];
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (to, preds) in self.preds.iter().enumerate() {
            for &from in preds {
                succs[from].push(to);
                incoming[to] += 1;
            }
        }
        for lane in &assignment.lanes {
            for pair in lane.windows(2) {
                succs[pair[0]].push(pair[1]);
                incoming[pair[1]] += 1;
            }
        }
        let mut ready: VecDeque<usize> = (0..n).filter(|&i| incoming[i] == 0).collect();
        let mut seen = 0;
        while let Some(node) = ready.pop_front() {
            seen += 1;
            for &next in &succs[node] {
                incoming[next] -= 1;
                if incoming[next] == 0 {
                    ready.push_back(next);
                }
            }
        }
        seen == n
    }

    /// Response-time analysis per worker, then the cycle's timeline composed
    /// along dependencies, then the objective.
    fn evaluate(&self, assignment: &Assignment) -> Option<Evaluation> {
        let lanes = &assignment.lanes;
        let g = self.copperlist_ns;
        let period = self.cycle_ns;
        let lane_cost: Vec<u64> = lanes
            .iter()
            .map(|lane| lane.iter().map(|&o| self.cost[o]).sum())
            .collect();
        // One worker per CPU in this search: no higher-priority lanes share a
        // CPU, so the response is the lane's own cost plus dispatch slack.
        let mut response = Vec::with_capacity(lanes.len());
        let mut rate = Vec::with_capacity(lanes.len());
        let mut load = Vec::with_capacity(lanes.len());
        for &cost in &lane_cost {
            let r = fixpoint(cost, &[], g, RESPONSE_CAP_PERIODS * period);
            response.push(r);
            rate.push(match r {
                Some(r) if r <= period => 1.0,
                Some(r) => period as f64 / (r + g / 2) as f64,
                None => 0.0,
            });
            load.push(cost as f64 / period as f64);
        }
        // Timeline: each lane runs its units in order; a unit starts when its
        // lane is free and every predecessor has ended.
        let n = self.units.len();
        let mut lane_of = vec![0usize; n];
        for (l, lane) in lanes.iter().enumerate() {
            for &o in lane {
                lane_of[o] = l;
            }
        }
        let mut starts = vec![0u64; n];
        let mut ends = vec![0u64; n];
        let mut done = vec![false; n];
        let mut lane_free = vec![0u64; lanes.len()];
        let mut progressed = true;
        let mut remaining = n;
        while remaining > 0 && progressed {
            progressed = false;
            for (l, lane) in lanes.iter().enumerate() {
                let Some(&o) = lane.iter().find(|&&o| !done[o]) else {
                    continue;
                };
                if self.preds[o].iter().any(|&p| !done[p]) {
                    continue;
                }
                let release = self.preds[o].iter().map(|&p| ends[p]).max().unwrap_or(0);
                let start = lane_free[l].max(release);
                starts[o] = start;
                ends[o] = start + self.cost[o];
                lane_free[l] = ends[o];
                done[o] = true;
                remaining -= 1;
                progressed = true;
            }
        }
        if remaining > 0 {
            return None;
        }
        let mut chain_ratio = Vec::with_capacity(self.chains.len());
        for (chain, pairs) in self.chains.iter().enumerate() {
            let deadline = u64::from(self.contract_deadline(chain)) * 1_000_000;
            let latency = pairs
                .iter()
                .map(|&(source, sink)| ends[sink].saturating_sub(starts[source]))
                .max()
                .unwrap_or(0);
            chain_ratio.push(latency as f64 / deadline as f64);
        }
        // Admission is gated by the whole cycle: the slowest lane throttles
        // every source, wherever the source itself runs, and so does the
        // cycle's critical path when fewer cycles than its length fit in
        // flight (`max_in_flight` CopperLists over `k` per cycle).
        let makespan = ends.iter().copied().max().unwrap_or(0);
        let cycles_in_flight =
            f64::from(self.max_in_flight) / f64::from(self.inventory.copperlists_per_cycle.max(1));
        let pipelined = makespan as f64 / cycles_in_flight;
        let cycle_rate =
            rate.iter()
                .copied()
                .fold(1.0f64, f64::min)
                .min(if pipelined <= period as f64 {
                    1.0
                } else {
                    period as f64 / (pipelined + g as f64 / 2.0)
                });
        let source_rate: Vec<f64> = self.sources.iter().map(|_| cycle_rate).collect();
        let rate_deficit: f64 = source_rate.iter().map(|&r| rate_deficit(r)).sum();
        let sum: f64 = chain_ratio.iter().sum();
        let max_load = load.iter().copied().fold(0.0f64, f64::max);
        let score = if self.deadline_objective {
            vec![
                round6(rate_deficit),
                chain_ratio.iter().filter(|&&r| r > 1.0).count() as f64,
                chain_ratio
                    .iter()
                    .filter(|&&r| r > 1.0 - self.margin)
                    .count() as f64,
                round6(sum),
                round6(max_load),
            ]
        } else {
            vec![round6(rate_deficit), round6(sum), round6(max_load)]
        };
        Some(Evaluation {
            score,
            ends,
            starts,
            response,
            rate,
            load,
        })
    }

    fn contract_deadline(&self, chain: usize) -> u32 {
        self.chain_deadlines[chain]
    }
}

fn round6(value: f64) -> f64 {
    (value * 1e6 + 0.5) as u64 as f64 / 1e6
}

/// Smallest `x = c + sum ceil((x + g) / T) C` over `hp`; `None` past `cap`.
fn fixpoint(c: u64, hp: &[(u64, u64)], g: u64, cap: u64) -> Option<u64> {
    let mut x = c;
    loop {
        let next = c + hp
            .iter()
            .map(|&(cost, period)| (x + g).div_ceil(period.max(1)) * cost)
            .sum::<u64>();
        if next == x {
            return Some(x);
        }
        if next > cap {
            return None;
        }
        x = next;
    }
}

/// The task id inside a step key, `None` for bridge steps.
fn task_of_key(key: &str) -> Option<&str> {
    key.split('|').find_map(|part| part.strip_prefix("task:"))
}

fn is_refine(key: &str) -> bool {
    key.contains("|phase:refine:")
}

/// A unit that no topological order can place, if the unit graph has a cycle.
fn first_unit_on_a_cycle(preds: &[Vec<usize>]) -> Option<usize> {
    let mut incoming: Vec<usize> = preds.iter().map(Vec::len).collect();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); preds.len()];
    for (to, from) in preds.iter().enumerate() {
        for &from in from {
            succs[from].push(to);
        }
    }
    let mut ready: VecDeque<usize> = (0..preds.len()).filter(|&i| incoming[i] == 0).collect();
    while let Some(unit) = ready.pop_front() {
        for &next in &succs[unit] {
            incoming[next] -= 1;
            if incoming[next] == 0 {
                ready.push_back(next);
            }
        }
    }
    // Every node left is on a cycle or downstream of one; strip the
    // downstream ones (no remaining successor of theirs is itself left) so a
    // cycle member is named.
    let mut left: Vec<bool> = incoming.iter().map(|&n| n > 0).collect();
    loop {
        let stripped = (0..preds.len()).find(|&i| left[i] && succs[i].iter().all(|&s| !left[s]));
        match stripped {
            Some(i) => left[i] = false,
            None => break,
        }
    }
    (0..preds.len()).find(|&i| left[i])
}

/// A source within `RATE_TOLERANCE` of its period keeps its rate.
pub(super) fn rate_deficit(rate: f64) -> f64 {
    (1.0 - RATE_TOLERANCE - rate).max(0.0)
}

/// A deterministic pseudo-random sequence for the search (splitmix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

impl Model {
    /// The start: occurrences in a topological order of the inventory, each
    /// placed on the least loaded CPU.
    fn start_plan(&self) -> Assignment {
        let n = self.units.len();
        let mut incoming = vec![0usize; n];
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (to, preds) in self.preds.iter().enumerate() {
            for &from in preds {
                succs[from].push(to);
                incoming[to] += 1;
            }
        }
        let mut ready: VecDeque<usize> = (0..n).filter(|&i| incoming[i] == 0).collect();
        let mut lanes = vec![Vec::new(); self.cpus.len()];
        let mut load = vec![0u64; self.cpus.len()];
        while let Some(o) = ready.pop_front() {
            let lane = (0..lanes.len())
                .min_by_key(|&l| (load[l], l))
                .expect("at least one CPU");
            lanes[lane].push(o);
            load[lane] += self.cost[o];
            for &next in &succs[o] {
                incoming[next] -= 1;
                if incoming[next] == 0 {
                    ready.push_back(next);
                }
            }
        }
        Assignment { lanes }
    }

    /// One random move: an occurrence moved to another lane and position, or
    /// two neighbours on one lane swapped. Invalid results are rejected by
    /// the caller through `is_valid`.
    fn neighbour(&self, assignment: &Assignment, rng: &mut Rng) -> Assignment {
        let mut lanes = assignment.lanes.clone();
        let occupied: Vec<usize> = (0..lanes.len()).filter(|&l| !lanes[l].is_empty()).collect();
        if occupied.is_empty() {
            return assignment.clone();
        }
        if rng.below(2) == 0 || lanes.len() == 1 {
            let from = occupied[rng.below(occupied.len())];
            let at = rng.below(lanes[from].len());
            let o = lanes[from].remove(at);
            let to = rng.below(lanes.len());
            let position = rng.below(lanes[to].len() + 1);
            lanes[to].insert(position, o);
        } else {
            let wide: Vec<usize> = occupied
                .iter()
                .copied()
                .filter(|&l| lanes[l].len() > 1)
                .collect();
            if wide.is_empty() {
                return assignment.clone();
            }
            let lane = wide[rng.below(wide.len())];
            let at = rng.below(lanes[lane].len() - 1);
            lanes[lane].swap(at, at + 1);
        }
        Assignment { lanes }
    }

    /// Hill climbing from the list schedule and from seeded random starts;
    /// every valid assignment evaluated is kept for the final ranking.
    fn search(&self, seed: u64, moves: usize, restarts: usize) -> Vec<(Vec<f64>, Assignment)> {
        let mut seen: BTreeMap<Assignment, Option<Vec<f64>>> = BTreeMap::new();
        let score = |assignment: &Assignment,
                     seen: &mut BTreeMap<Assignment, Option<Vec<f64>>>|
         -> Option<Vec<f64>> {
            if let Some(known) = seen.get(assignment) {
                return known.clone();
            }
            let value = if self.is_valid(assignment) {
                self.evaluate(assignment).map(|evaluation| evaluation.score)
            } else {
                None
            };
            seen.insert(assignment.clone(), value.clone());
            value
        };
        let start = self.start_plan();
        for attempt in 0..restarts.max(1) {
            let mut rng = Rng(seed.wrapping_add(attempt as u64));
            let mut current = if attempt == 0 {
                start.clone()
            } else {
                let mut random = start.clone();
                for _ in 0..self.units.len() * 2 {
                    let candidate = self.neighbour(&random, &mut rng);
                    if self.is_valid(&candidate) {
                        random = candidate;
                    }
                }
                random
            };
            let mut current_score = score(&current, &mut seen);
            let mut best = current.clone();
            let mut best_score = current_score.clone();
            let mut stall = 0;
            for _ in 0..moves {
                let candidate = self.neighbour(&current, &mut rng);
                let candidate_score = score(&candidate, &mut seen);
                let better = match (&candidate_score, &current_score) {
                    (Some(c), Some(cur)) => better_score(c, cur),
                    (Some(_), None) => true,
                    _ => false,
                };
                if better {
                    current = candidate;
                    current_score = candidate_score;
                    stall = 0;
                    if best_score
                        .as_ref()
                        .is_none_or(|b| better_score(current_score.as_ref().unwrap(), b))
                    {
                        best = current.clone();
                        best_score = current_score.clone();
                    }
                } else {
                    stall += 1;
                    if stall >= PATIENCE {
                        current = best.clone();
                        for _ in 0..KICK {
                            let kicked = self.neighbour(&current, &mut rng);
                            if self.is_valid(&kicked) {
                                current = kicked;
                            }
                        }
                        current_score = score(&current, &mut seen);
                        stall = 0;
                    }
                }
            }
        }
        let mut scored: Vec<(Vec<f64>, Assignment)> = seen
            .into_iter()
            .filter_map(|(assignment, score)| score.map(|score| (score, assignment)))
            .collect();
        scored.sort_by(|a, b| {
            if better_score(&a.0, &b.0) {
                core::cmp::Ordering::Less
            } else if better_score(&b.0, &a.0) {
                core::cmp::Ordering::Greater
            } else {
                a.1.cmp(&b.1)
            }
        });
        scored
    }

    /// The plan an assignment describes, with the contract's placement.
    fn materialize(
        &self,
        assignment: &Assignment,
        max_in_flight: u32,
        dispatcher: Option<CuPlanThread>,
    ) -> CuMissionPlan {
        let mut mission = self.inventory.clone();
        mission.max_in_flight = max_in_flight.max(mission.copperlists_per_cycle);
        mission.workers = assignment
            .lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| !lane.is_empty())
            .map(|(l, lane)| CuPlanWorker {
                id: format!("cpu{}", self.cpus[l]),
                placement: CuPlanPlacement::Thread {
                    cpu: Some(self.cpus[l]),
                    policy: self.policy,
                },
                steps: lane
                    .iter()
                    .flat_map(|&u| self.units[u].iter().map(|&o| o as u32))
                    .collect(),
            })
            .collect();
        mission.dispatcher = dispatcher;
        mission
    }

    fn prediction(&self, assignment: &Assignment, contract: &CuContract) -> CuPrediction {
        let evaluation = self
            .evaluate(assignment)
            .expect("a ranked assignment evaluates");
        let chains = contract
            .chains
            .iter()
            .zip(&self.chains)
            .map(|(chain, pairs)| {
                let deadline_ns = u64::from(chain.deadline_ms) * 1_000_000;
                let latency_ns = pairs
                    .iter()
                    .map(|&(source, sink)| {
                        evaluation.ends[sink].saturating_sub(evaluation.starts[source])
                    })
                    .max()
                    .unwrap_or(0);
                (
                    chain.id.clone(),
                    CuChainPrediction {
                        latency_ns,
                        deadline_ns,
                        ratio: latency_ns as f64 / deadline_ns as f64,
                    },
                )
            })
            .collect();
        let workers = assignment
            .lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| !lane.is_empty())
            .map(|(l, _)| {
                (
                    format!("cpu{}", self.cpus[l]),
                    CuWorkerPrediction {
                        cpu: self.cpus[l],
                        load: evaluation.load[l],
                        response_ns: evaluation.response[l],
                        rate: evaluation.rate[l],
                    },
                )
            })
            .collect();
        let mut lane_of = BTreeMap::new();
        for (l, lane) in assignment.lanes.iter().enumerate() {
            for &o in lane {
                lane_of.insert(o, l);
            }
        }
        let sources = contract
            .sources
            .iter()
            .zip(&self.sources)
            .map(|(source, occurrences)| {
                let rate = occurrences
                    .iter()
                    .map(|o| evaluation.rate[lane_of[o]])
                    .fold(1.0f64, f64::min);
                (source.task.clone(), rate)
            })
            .collect();
        CuPrediction {
            score: evaluation.score,
            chains,
            workers,
            sources,
        }
    }
}

/// Lexicographic comparison, smaller is better.
fn better_score(a: &[f64], b: &[f64]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x < y {
            return true;
        }
        if x > y {
            return false;
        }
    }
    false
}

/// Whether two scores describe the same candidate for reporting: every tier
/// before the sum tier equal, and the sum tier within `MIN_SEPARATION`.
fn same_candidate(a: &[f64], b: &[f64], sum_tier: usize) -> bool {
    a[..sum_tier] == b[..sum_tier]
        && (a[sum_tier] - b[sum_tier]).abs() <= MIN_SEPARATION * b[sum_tier].abs().max(1e-9)
}

/// Proposes up to `request.candidates` distinct plans for one mission.
///
/// The inventory is the config's exported plan for `copperlists_per_cycle`
/// CopperLists, so every required edge is present whatever the placement;
/// the search only chooses which CPU runs each occurrence and in which order.
/// Every returned plan passes [`CuPlan::validate`].
pub fn propose(request: &ProposeRequest<'_>) -> CuResult<Vec<CuCandidate>> {
    request
        .contract
        .validate(request.config, Some(request.mission))?;
    let graph = request.config.get_graph(Some(request.mission))?;
    let signature = super::graph_signature(graph, Some(request.mission));
    if request.profile.config_signature != signature {
        return Err(CuError::from(format!(
            "The profile was recorded on another graph ({}); this config's mission '{}' is {signature}",
            request.profile.config_signature, request.mission
        )));
    }
    let mut exported = CuPlan::from_config_cyclic(request.config, request.copperlists_per_cycle)?;
    exported.concurrent_resources = request.contract.concurrent_safe_resources.clone();
    let inventory = exported
        .missions
        .get(request.mission)
        .ok_or_else(|| CuError::from(format!("Unknown mission '{}'", request.mission)))?
        .clone();
    let model = Model::new(request, inventory)?;
    let scored = model.search(request.seed, request.moves, request.restarts);
    let sum_tier = if model.deadline_objective { 3 } else { 1 };
    let mut chosen: Vec<(Vec<f64>, Assignment)> = Vec::new();
    for (score, assignment) in scored {
        if chosen.len() >= request.candidates {
            break;
        }
        if chosen
            .iter()
            .any(|(known, _)| same_candidate(&score, known, sum_tier))
        {
            continue;
        }
        chosen.push((score, assignment));
    }
    if chosen.is_empty() {
        return Err(CuError::from(
            "No valid plan was found on this contract's CPUs",
        ));
    }
    let mut candidates = Vec::with_capacity(chosen.len());
    for (_, assignment) in chosen {
        let mission = model.materialize(
            &assignment,
            request.contract.max_in_flight,
            request.contract.dispatcher.clone(),
        );
        let mut plan = exported.clone();
        plan.missions.insert(request.mission.to_string(), mission);
        let mut prepared = request.config.clone();
        plan.provide_capacity(&mut prepared);
        plan.validate(&prepared)?;
        candidates.push(CuCandidate {
            prediction: model.prediction(&assignment, request.contract),
            plan,
        });
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::CuChain;
    use crate::planner::CuCostStats;
    use crate::planner::CuOperationProfile;
    use crate::planner::CuSourceRate;

    fn config() -> CuConfig {
        CuConfig::deserialize_ron(
            r#"(
            runtime: (rate_target_hz: 100),
            logging: (copperlist_count: 4),
            tasks: [(id: "src", type: "Source"), (id: "left", type: "Left"),
                (id: "right", type: "Right", kind: stateless_task), (id: "sink", type: "Sink")],
            cnx: [(src: "src", dst: "left", msg: "u32"), (src: "src", dst: "right", msg: "u32"),
                (src: "left", dst: "sink", msg: "u32"), (src: "right", dst: "sink", msg: "u32")],
        )"#,
        )
        .unwrap()
    }

    fn contract(cpus: Vec<usize>) -> CuContract {
        CuContract {
            chains: vec![CuChain {
                id: "hot".into(),
                source: "src".into(),
                sink: "sink".into(),
                deadline_ms: 10,
            }],
            sources: vec![CuSourceRate {
                task: "src".into(),
                period_ms: 10,
            }],
            cpus,
            max_in_flight: 2,
            objective: Default::default(),
            concurrent_safe_resources: Vec::new(),
            background_pools: Vec::new(),
            worker_policy: SchedulingPolicy::Fair,
            dispatcher: None,
        }
    }

    /// `left` and `right` each cost 3 ms every CopperList; the rest is free.
    fn profile() -> CuProfile {
        let graph = config().get_graph(Some("default")).unwrap().clone();
        let mut profile = CuProfile::new(
            crate::planner::graph_signature(&graph, Some("default")),
            "default".into(),
        );
        profile.copperlists = 100;
        profile.window_ns = 1_000_000_000;
        for (task, cost) in [
            ("src", 100_000),
            ("left", 3_000_000),
            ("right", 3_000_000),
            ("sink", 100_000),
        ] {
            let mut samples = vec![cost; 100];
            profile.operations.insert(
                format!("mission:default|task:{task}|phase:whole"),
                CuOperationProfile {
                    fired: CuCostStats::from_samples(&mut samples),
                    skipped: CuCostStats::default(),
                    firing_rate_hz: 100.0,
                },
            );
        }
        profile
    }

    fn request<'a>(
        config: &'a CuConfig,
        contract: &'a CuContract,
        profile: &'a CuProfile,
        cycle: u32,
    ) -> ProposeRequest<'a> {
        ProposeRequest {
            config,
            mission: "default",
            contract,
            profile,
            copperlists_per_cycle: cycle,
            candidates: 3,
            seed: 7,
            moves: 300,
            restarts: 3,
        }
    }

    #[test]
    fn parallel_branches_land_on_two_cpus_and_shorten_the_chain() {
        let config = config();
        let profile = profile();
        let one = contract(vec![0]);
        let serial = propose(&request(&config, &one, &profile, 1)).unwrap();
        let two = contract(vec![0, 1]);
        let parallel = propose(&request(&config, &two, &profile, 1)).unwrap();
        let serial_latency = serial[0].prediction.chains["hot"].latency_ns;
        let parallel_latency = parallel[0].prediction.chains["hot"].latency_ns;
        assert_eq!(serial_latency, 6_200_000);
        assert_eq!(parallel_latency, 3_200_000);
        assert_eq!(parallel[0].plan.missions["default"].workers.len(), 2);
        for candidate in serial.iter().chain(&parallel) {
            candidate.plan.validate(&config).unwrap();
            assert_eq!(candidate.prediction.sources["src"], 1.0);
        }
        // Same request, same answer.
        let again = propose(&request(&config, &two, &profile, 1)).unwrap();
        assert_eq!(again, parallel);
    }

    #[test]
    fn two_copperlists_per_cycle_let_the_stateless_task_overlap() {
        let config = config();
        let profile = profile();
        let two = contract(vec![0, 1, 2]);
        let candidates = propose(&request(&config, &two, &profile, 2)).unwrap();
        let best = &candidates[0];
        let mission = &best.plan.missions["default"];
        assert_eq!(
            (mission.copperlists_per_cycle, mission.max_in_flight),
            (2, 2)
        );
        best.plan.validate(&config).unwrap();
        // Both CopperLists of the cycle fit in one 20 ms cycle with three CPUs.
        assert!(best.prediction.chains["hot"].latency_ns <= 6_200_000);
        assert!(best.prediction.workers.values().all(|w| w.load < 1.0));
    }

    #[test]
    fn anytime_phases_stay_together_on_one_worker() {
        let config = CuConfig::deserialize_ron(
            r#"(
            runtime: (rate_target_hz: 100),
            logging: (copperlist_count: 2),
            tasks: [(id: "src", type: "Source"),
                (id: "refiner", type: "Refiner", anytime: (max_refines: 2)),
                (id: "sink", type: "Sink")],
            cnx: [(src: "src", dst: "refiner", msg: "u32"), (src: "refiner", dst: "sink", msg: "u32")],
        )"#,
        )
        .unwrap();
        let graph = config.get_graph(Some("default")).unwrap();
        let mut profile = CuProfile::new(
            crate::planner::graph_signature(graph, Some("default")),
            "default".into(),
        );
        profile.copperlists = 100;
        profile.window_ns = 1_000_000_000;
        for (key, cost) in [
            ("mission:default|task:src|phase:whole", 100_000),
            ("mission:default|task:refiner|phase:base", 3_000_000),
            ("mission:default|task:sink|phase:whole", 100_000),
        ] {
            let mut samples = vec![cost; 100];
            profile.operations.insert(
                key.into(),
                CuOperationProfile {
                    fired: CuCostStats::from_samples(&mut samples),
                    skipped: CuCostStats::default(),
                    firing_rate_hz: 100.0,
                },
            );
        }
        let mut contract = contract(vec![0, 1]);
        contract.chains[0].sink = "sink".into();
        contract.max_in_flight = 1;
        let candidates = propose(&request(&config, &contract, &profile, 1)).unwrap();
        for candidate in &candidates {
            let mission = &candidate.plan.missions["default"];
            let refine = |key: &str| {
                mission
                    .steps
                    .iter()
                    .position(|s| s.key.contains(key))
                    .unwrap() as u32
            };
            let (base, one, two) = (refine("phase:base"), refine("refine:1"), refine("refine:2"));
            let worker = mission
                .workers
                .iter()
                .find(|w| w.steps.contains(&base))
                .unwrap();
            let at = |step| worker.steps.iter().position(|&s| s == step).unwrap();
            assert_eq!((at(one), at(two)), (at(base) + 1, at(base) + 2));
        }
        assert_eq!(candidates[0].prediction.chains["hot"].latency_ns, 3_200_000);
    }

    #[test]
    fn a_missing_measurement_is_an_error() {
        let config = config();
        let mut profile = profile();
        profile
            .operations
            .remove("mission:default|task:left|phase:whole");
        let one = contract(vec![0]);
        assert!(propose(&request(&config, &one, &profile, 1)).is_err());
    }
}
