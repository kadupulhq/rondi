// Dead code is expected until the update path is wired in to this module.
#![allow(dead_code)]

mod amount;
mod consolidate;
mod pdp;

/// One row written by `updatev`, as printed (UP-31, UP-32).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RrdUpdateSummary {
    pub timestamp: i64,
    pub consolidation: String,
    pub pdp_per_row: u64,
    pub values: Vec<f64>,
}
