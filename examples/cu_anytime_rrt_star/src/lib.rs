//! Anytime RRT\* planner example, built on the `cu29` `Anytime` trait.
//!
//! This crate is currently a scaffold. The payload types below are the stable
//! shapes the planner and the surrounding source/sink will use; the planner
//! implementation itself lands in a follow-up commit.

use cu29::bincode::{Decode, Encode};
use cu29::prelude::*;

pub mod occupancy_grid;
pub mod tasks;

/// 2-D world-frame point used by both queries and planned paths.
#[derive(Default, Debug, Clone, Copy, Encode, Decode, Serialize, Deserialize, Reflect)]
pub struct Point2D {
    pub x: f32,
    pub y: f32,
}

/// Per-cycle planning request handed to the planner.
///
/// `max_cost` is an optional per-cycle quality target: once the best-so-far
/// solution beats it, `refine` short-circuits with `Step::Satisfied`.
#[derive(Default, Debug, Clone, Encode, Decode, Serialize, Deserialize, Reflect)]
pub struct PlanQuery {
    pub start: Point2D,
    pub goal: Point2D,
    pub max_cost: Option<f32>,
}

/// Best-so-far path emitted by the planner.
///
/// `waypoints` is empty and `cost` is `f32::INFINITY` until the first solution
/// is found.
#[derive(Default, Debug, Clone, Encode, Decode, Serialize, Deserialize, Reflect)]
pub struct PlannedPath {
    pub waypoints: Vec<Point2D>,
    pub cost: f32,
    pub tree_size: u32,
    pub iterations: u32,
}
