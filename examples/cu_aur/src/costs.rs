//! Cost replay: the measured execution-time sequences, scaled and turned into crunch units.
//!
//! Every callback replays its own recorded sequence in order, scaled by one factor
//! `alpha`, and wraps when the run outlives the sequence. A target of `t` ms is charged
//! as `crunch(t * 1e6 / k)` units, where `k` is the host's nanoseconds per crunch unit
//! measured by the `calibrate` binary.
//!
//! The table is installed once before the application is built: a task resolves its own
//! sequence in `new` and every worker shares the table for the life of the process.

use cu29::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

/// `data/costs.json`: per callback, the recorded durations in milliseconds, in
/// recorded order.
#[derive(Debug, Deserialize)]
pub struct CostFile {
    /// The 86 callback names, in the order `cost_index` indexes them.
    pub order: Vec<String>,
    pub samples_ms: HashMap<String, Vec<f64>>,
}

/// `calibrate --output`: what one crunch unit costs on this host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calibration {
    /// Nanoseconds per crunch unit, from the linear fit.
    pub k_ns_per_unit: f64,
    /// Worst deviation of a fitted point from the fit, in percent.
    pub fit_error_pct: f64,
    pub cpu: String,
    pub measured_utc: String,
}

/// One callback's replay: the crunch units it runs on each firing and the durations
/// those units are meant to cost.
#[derive(Debug)]
pub struct CostSequence {
    pub name: String,
    pub units: Vec<u64>,
    pub targets_ns: Vec<u64>,
}

impl CostSequence {
    /// Where the `seq`-th firing (from 1) lands in a sequence shorter than the run
    /// needs: it wraps.
    pub fn index_of(&self, seq: u64) -> usize {
        (seq.saturating_sub(1) % self.units.len() as u64) as usize
    }
}

/// The replay table every task reads, indexed by the `cost_index` of its RON config.
#[derive(Debug)]
pub struct CostTable {
    pub alpha: f64,
    pub k_ns_per_unit: f64,
    sequences: Vec<CostSequence>,
}

impl CostTable {
    /// Scales the recorded sequences by `alpha` and converts them to crunch units.
    ///
    /// Done once, here, so a firing only indexes an array: the conversion is float work
    /// and 86 callbacks would otherwise redo it on every CopperList.
    pub fn build(file: &CostFile, alpha: f64, k_ns_per_unit: f64) -> CuResult<Self> {
        if !alpha.is_finite() || alpha <= 0.0 {
            return Err(CuError::from(format!(
                "alpha must be positive, got {alpha}"
            )));
        }
        if !k_ns_per_unit.is_finite() || k_ns_per_unit <= 0.0 {
            return Err(CuError::from(format!(
                "k_ns_per_unit must be positive, got {k_ns_per_unit}"
            )));
        }
        let mut sequences = Vec::with_capacity(file.order.len());
        for name in &file.order {
            let samples = file.samples_ms.get(name).ok_or_else(|| {
                CuError::from(format!("costs: '{name}' is in order but has no samples"))
            })?;
            if samples.is_empty() {
                return Err(CuError::from(format!("costs: '{name}' has no samples")));
            }
            let targets_ns: Vec<u64> = samples
                .iter()
                .map(|ms| (ms * alpha * 1e6).round().max(0.0) as u64)
                .collect();
            sequences.push(CostSequence {
                name: name.clone(),
                units: targets_ns
                    .iter()
                    .map(|ns| (*ns as f64 / k_ns_per_unit).round() as u64)
                    .collect(),
                targets_ns,
            });
        }
        Ok(Self {
            alpha,
            k_ns_per_unit,
            sequences,
        })
    }

    pub fn sequences(&self) -> &[CostSequence] {
        &self.sequences
    }

    pub fn sequence(&self, cost_index: u32) -> CuResult<&CostSequence> {
        self.sequences.get(cost_index as usize).ok_or_else(|| {
            CuError::from(format!(
                "config: cost_index {cost_index} is past the {} recorded callbacks",
                self.sequences.len()
            ))
        })
    }
}

static TABLE: OnceLock<CostTable> = OnceLock::new();

/// Installs the replay table. Called once, before the application is built.
pub fn install(table: CostTable) -> CuResult<()> {
    TABLE
        .set(table)
        .map_err(|_| CuError::from("the cost table was already installed"))
}

/// Reads the sequences, scales them, and installs the table.
pub fn install_from(costs: &Path, alpha: f64, calibration: &Path) -> CuResult<()> {
    let file = read_cost_file(costs)?;
    let calibration = read_calibration(calibration)?;
    install(CostTable::build(&file, alpha, calibration.k_ns_per_unit)?)
}

/// The installed replay table.
pub fn table() -> CuResult<&'static CostTable> {
    TABLE
        .get()
        .ok_or_else(|| CuError::from("no cost table installed: pass --costs and --alpha"))
}

pub fn read_cost_file(path: &Path) -> CuResult<CostFile> {
    let text = fs::read_to_string(path)
        .map_err(|e| CuError::new_with_cause(&format!("{}", path.display()), e))?;
    serde_json::from_str(&text)
        .map_err(|e| CuError::new_with_cause(&format!("{}", path.display()), e))
}

pub fn read_calibration(path: &Path) -> CuResult<Calibration> {
    let text = fs::read_to_string(path).map_err(|e| {
        CuError::new_with_cause(
            &format!(
                "{}: run the calibrate binary on this host first",
                path.display()
            ),
            e,
        )
    })?;
    ron::from_str(&text).map_err(|e| CuError::new_with_cause(&format!("{}", path.display()), e))
}

pub fn write_calibration(path: &Path, calibration: &Calibration) -> CuResult<()> {
    let text = ron::ser::to_string_pretty(calibration, ron::ser::PrettyConfig::default())
        .map_err(|e| CuError::new_with_cause("calibration must serialize", e))?;
    fs::write(path, text).map_err(|e| CuError::new_with_cause(&format!("{}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> CostFile {
        CostFile {
            order: vec!["a".to_string(), "b".to_string()],
            samples_ms: HashMap::from([
                ("a".to_string(), vec![1.0, 2.0, 3.0]),
                ("b".to_string(), vec![0.5]),
            ]),
        }
    }

    #[test]
    fn test_units_scale_by_alpha_and_k() {
        let table = CostTable::build(&file(), 0.5, 100.0).unwrap();
        let a = table.sequence(0).unwrap();
        // 1ms * 0.5 = 500_000ns, at 100ns per unit.
        assert_eq!(a.units, [5000, 10000, 15000]);
        assert_eq!(a.targets_ns, [500_000, 1_000_000, 1_500_000]);
    }

    #[test]
    fn test_a_short_sequence_wraps() {
        let table = CostTable::build(&file(), 1.0, 1.0).unwrap();
        let a = table.sequence(0).unwrap();
        assert_eq!((a.index_of(1), a.index_of(4), a.index_of(5)), (0, 0, 1));
        let b = table.sequence(1).unwrap();
        assert_eq!(b.targets_ns[b.index_of(7)], 500_000);
    }

    #[test]
    fn test_an_index_past_the_table_is_an_error() {
        let table = CostTable::build(&file(), 1.0, 1.0).unwrap();
        assert_eq!(table.sequence(1).unwrap().name, "b");
        assert!(table.sequence(2).is_err());
    }

    #[test]
    fn test_a_nonsense_scale_is_an_error() {
        assert!(CostTable::build(&file(), 0.0, 1.0).is_err());
        assert!(CostTable::build(&file(), 0.5, 0.0).is_err());
    }
}
