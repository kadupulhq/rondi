//! Rondi's storage and time-series semantics.
pub mod compatibility;
mod consolidation;
mod data_source;
mod format;
pub mod import_export;
mod queries;
mod rrd_binary;
pub(crate) mod storage;
pub mod vdef;
pub mod xport;

pub use format::{
    ArchivePoint, DatabaseConfig, FORMAT_VERSION, FetchResult, RrdFetchResult, RrdFetchRow, Update,
};
pub use rrd_binary::{
    RrdArchiveInfo, RrdCdpPrepInfo, RrdDataSourceInfo, RrdDataSourceTune, RrdDumpHeader, RrdInfo,
    RrdResizeAction, RrdTuneBound, RrdUpdateSummary, create_rrd_file, dump_rrd_file,
    dump_rrd_file_with_header, fetch_rrd_file, first_rrd_time, inspect_rrd_file,
    parse_rrd_scaled_duration, resize_rrd_file, restore_rrd_file, tune_rrd_data_sources,
    update_rrd_file, update_rrd_file_precise, update_rrd_raw_values, update_rrd_raw_values_precise,
    update_rrd_raw_values_precise_verbose, update_rrd_raw_values_verbose, update_rrd_values,
    update_rrd_values_verbose,
};
pub use storage::{Store, StoreError};
pub use vdef::{VdefError, VdefFunction, VdefResult, evaluate_vdef};
pub use xport::{
    RrdXportCdef, RrdXportColumn, RrdXportDefinition, RrdXportResult, fetch_xport,
    fetch_xport_with_cdefs,
};
