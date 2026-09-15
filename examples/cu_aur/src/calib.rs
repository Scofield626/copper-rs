//! What one crunch unit costs on this host.
//!
//! Every callback replays its own measured sequence, so targets are continuous over four
//! orders of magnitude while the crunch is linear in its argument: one fit over five unit
//! counts gives the `k_ns_per_unit` that turns a target duration into units.
//!
//! Measure in release: the crunch is the workload, and a debug build has a different one.

use crate::tasks::crunch;
use std::time::{Duration, Instant};

/// The five points of the fit, spanning the replayed range: the shortest callback firing
/// of the dataset is a few hundred nanoseconds and the longest is nearly 200ms.
pub const POINTS: [u64; 5] = [100, 3_000, 100_000, 3_000_000, 100_000_000];
/// Timed runs per point; the median is what the fit uses.
const RUNS: usize = 15;
/// A short point is timed over several back-to-back calls so the clock's own cost does
/// not land in the fit. Each call still pays its own fixed cost, as a callback does.
const REPS_BUDGET: u64 = 300_000;
const WARMUP: Duration = Duration::from_millis(500);

/// Median nanoseconds of one crunch of `units`, over `RUNS` timed batches.
fn measure(units: u64) -> f64 {
    let reps = (REPS_BUDGET / units.max(1)).clamp(1, 1000);
    let mut samples: Vec<f64> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..reps {
                crunch(units);
            }
            start.elapsed().as_secs_f64() * 1e9 / reps as f64
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[RUNS / 2]
}

/// `ns = k * units`, fitted through the origin as the mean of the points' per-unit cost.
///
/// Every point counts the same, which is what the replay needs: the targets span four
/// orders of magnitude and an ordinary least squares over absolute residuals would be
/// decided by the largest point alone.
pub fn fit(points: &[(f64, f64)]) -> f64 {
    points.iter().map(|(x, y)| y / x).sum::<f64>() / points.len() as f64
}

/// Times [`POINTS`] on this host and returns them with the fitted cost per unit and the
/// worst relative miss of the fit, in percent.
pub fn measure_unit_cost() -> (Vec<(f64, f64)>, f64, f64) {
    // The app runs the crunch back to back, so calibrate against a warm core.
    let warmup = Instant::now();
    while warmup.elapsed() < WARMUP {
        crunch(100_000);
    }
    let points: Vec<(f64, f64)> = POINTS
        .iter()
        .map(|units| (*units as f64, measure(*units)))
        .collect();
    let k_ns_per_unit = fit(&points);
    let worst = points
        .iter()
        .map(|(units, measured)| ((k_ns_per_unit * units - measured) / measured * 100.0).abs())
        .fold(0.0, f64::max);
    (points, k_ns_per_unit, worst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fit_recovers_a_proportional_cost() {
        let points: Vec<(f64, f64)> = POINTS
            .iter()
            .map(|units| (*units as f64, 3.5 * *units as f64))
            .collect();
        assert!((fit(&points) - 3.5).abs() < 1e-9, "{}", fit(&points));
    }

    /// The smallest point must count as much as the largest, or a replay of a
    /// sub-microsecond callback is fitted by a millisecond one.
    #[test]
    fn test_every_point_counts_the_same() {
        let mut points: Vec<(f64, f64)> = POINTS
            .iter()
            .map(|units| (*units as f64, 3.0 * *units as f64))
            .collect();
        points[0].1 *= 2.0;
        let k = fit(&points);
        assert!((k - 3.6).abs() < 1e-9, "{k}");
    }
}
