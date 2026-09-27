use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatabaseConfig {
    pub step: u64,
    pub heartbeat: u64,
    pub rows: usize,
    pub start: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchivePoint {
    pub timestamp: i64,
    pub value: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Update {
    pub timestamp: i64,
    pub value: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DatabaseFile {
    pub version: u32,
    pub config: DatabaseConfig,
    pub last_update: i64,
    pub points: Vec<ArchivePoint>,
    pub bucket_start: i64,
    pub known_seconds: u64,
    pub weighted_sum: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FetchResult {
    pub database: String,
    pub step: u64,
    pub points: Vec<ArchivePoint>,
}

/// Values fetched from an existing RRDtool archive. `start` and `end` are
/// the aligned request boundaries returned by RRDtool; each row is timestamped
/// at the end of its consolidation interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RrdFetchResult {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    pub data_sources: Vec<String>,
    pub rows: Vec<RrdFetchRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RrdFetchRow {
    pub timestamp: i64,
    pub values: Vec<Option<f64>>,
}
