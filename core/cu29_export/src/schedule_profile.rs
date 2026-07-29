//! Builds a measured [`PlanProfile`] from a recorded log.
//!
//! Per-task durations come from the `process_time` window every message
//! metadata already carries; no extra instrumentation is involved. The output
//! RON is the exact value of the config's `runtime.plan_profile` field, which
//! any profile-guided [`PlanPolicy`](cu29::config::PlanPolicy) then reads
//! (see `sched-v0.md`).

use crate::copperlists_reader;
use crate::logstats::{collect_output_packs, extract_end_time_ns, extract_start_time_ns};
use cu29::config::{CuConfig, PlanProfile};
use cu29::prelude::{CopperListTuple, CuPayloadRawBytes};
use cu29::{CuError, CuResult};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// Which statistic of the sampled durations fills the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ProfileStat {
    /// Arithmetic mean of the samples.
    #[default]
    Mean,
    /// 99th percentile: robust to outliers, still pessimistic.
    P99,
    /// Largest observed sample.
    Max,
}

/// One task's flattened slot range in the copperlist message vector.
struct PackRange {
    start: usize,
    len: usize,
    task: String,
}

pub fn compute_schedule_profile<P>(
    mut reader: impl Read,
    config: &CuConfig,
    mission: Option<&str>,
    stat: ProfileStat,
) -> CuResult<PlanProfile>
where
    P: CopperListTuple + CuPayloadRawBytes,
{
    let graph = config.get_graph(mission)?;
    let packs = collect_output_packs(graph, config.plan_policy(), &config.plan_profile())?;

    // The copperlist message vector flattens the packs in slot order.
    let mut ranges = Vec::with_capacity(packs.len());
    let mut base = 0usize;
    for pack in &packs {
        ranges.push(PackRange {
            start: base,
            len: pack.msg_types.len(),
            task: pack.src.clone(),
        });
        base += pack.msg_types.len();
    }

    let mut samples: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for culist in copperlists_reader::<P>(&mut reader) {
        let cumsgs = culist.msgs.cumsgs();
        for range in &ranges {
            let mut start_ns: Option<u64> = None;
            let mut end_ns: Option<u64> = None;
            let end_slot = (range.start + range.len).min(cumsgs.len());
            for msg in &cumsgs[range.start.min(end_slot)..end_slot] {
                let meta = msg.metadata();
                if let Some(start) = extract_start_time_ns(meta) {
                    start_ns = Some(start_ns.map_or(start, |current| current.min(start)));
                }
                if let Some(end) = extract_end_time_ns(meta) {
                    end_ns = Some(end_ns.map_or(end, |current| current.max(end)));
                }
            }
            if let (Some(start), Some(end)) = (start_ns, end_ns)
                && let Some(duration) = end.checked_sub(start)
            {
                samples
                    .entry(range.task.clone())
                    .or_default()
                    .push(duration);
            }
        }
    }

    Ok(PlanProfile {
        task_duration_ns: finalize_samples(samples, stat),
    })
}

fn finalize_samples(
    samples: BTreeMap<String, Vec<u64>>,
    stat: ProfileStat,
) -> BTreeMap<String, u64> {
    let mut task_duration_ns = BTreeMap::new();
    for (task, mut durations) in samples {
        if durations.is_empty() {
            continue;
        }
        durations.sort_unstable();
        let value = match stat {
            ProfileStat::Mean => {
                (durations.iter().map(|&d| d as u128).sum::<u128>() / durations.len() as u128)
                    as u64
            }
            ProfileStat::P99 => durations[(durations.len() - 1) * 99 / 100],
            ProfileStat::Max => *durations.last().unwrap(),
        };
        task_duration_ns.insert(task, value);
    }
    task_duration_ns
}

/// Writes the profile as pretty RON: the pasteable `plan_profile:` value.
pub fn write_schedule_profile(profile: &PlanProfile, path: &Path) -> CuResult<()> {
    let ron = ron::ser::to_string_pretty(profile, ron::ser::PrettyConfig::default())
        .map_err(|e| CuError::new_with_cause("Failed to serialize schedule profile", e))?;
    std::fs::write(path, ron)
        .map_err(|e| CuError::new_with_cause("Failed to write schedule profile", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(entries: &[(&str, &[u64])]) -> BTreeMap<String, Vec<u64>> {
        entries
            .iter()
            .map(|(task, durations)| (task.to_string(), durations.to_vec()))
            .collect()
    }

    #[test]
    fn finalize_picks_the_requested_statistic() {
        let input = samples(&[("cam", &[100, 200, 300]), ("imu", &[10])]);
        let mean = finalize_samples(input.clone(), ProfileStat::Mean);
        assert_eq!(mean.get("cam"), Some(&200));
        assert_eq!(mean.get("imu"), Some(&10));

        let max = finalize_samples(input.clone(), ProfileStat::Max);
        assert_eq!(max.get("cam"), Some(&300));

        let p99 = finalize_samples(input, ProfileStat::P99);
        assert_eq!(p99.get("cam"), Some(&200));
    }

    #[test]
    fn profile_snippet_round_trips_through_ron() {
        let profile = PlanProfile {
            task_duration_ns: samples(&[("cam", &[0])])
                .into_keys()
                .map(|task| (task, 1234u64))
                .collect(),
        };
        let ron = ron::ser::to_string_pretty(&profile, ron::ser::PrettyConfig::default()).unwrap();
        let parsed: PlanProfile = ron::from_str(&ron).unwrap();
        assert_eq!(parsed, profile);
    }
}
