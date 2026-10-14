//! Rondi's storage and time-series semantics.
pub mod compatibility;
mod consolidation;
mod data_source;
mod format;
pub mod graph;
pub mod graph_layout;
pub mod import_export;
mod queries;
mod rpn;
mod rrd_binary;
mod rrd_create;
mod rrd_modify;
mod rrd_number;
mod rrd_resize;
mod rrd_restore;
pub mod rrd_snprintf;
pub mod rrdc_proto;
#[cfg(unix)]
pub mod safe_fs;
pub(crate) mod storage;
pub mod time;
pub mod vdef;

pub use format::{
    ArchivePoint, DatabaseConfig, FORMAT_VERSION, FetchResult, RrdFetchResult, RrdFetchRow, Update,
};
pub use rrd_binary::{
    RrdArchiveInfo, RrdCdpPrepInfo, RrdDataSourceInfo, RrdDataSourceTune, RrdDumpHeader, RrdInfo,
    RrdRawUpdate, RrdResizeAction, RrdTuneBound, RrdUpdateSummary, cf_conv_error, create_rrd_file,
    dst_conv_error, dump_rrd_file, dump_rrd_file_with_header, dump_rrd_file_with_header_bytes,
    dump_rrd_open_file, dump_rrd_open_file_bytes, fetch_rrd_file, fetch_rrd_open_file,
    first_rrd_time, first_rrd_time_from_info, inspect_rrd_file, inspect_rrd_open_file,
    is_rrd_locking_mode, open_output_file, parse_rrd_scaled_duration, rrd_name_error,
    tune_rrd_data_sources, tune_rrd_data_sources_file, update_rrd_file, update_rrd_file_precise,
    update_rrd_raw_batch, update_rrd_raw_batch_file, update_rrd_raw_values,
    update_rrd_raw_values_precise, update_rrd_raw_values_precise_verbose,
    update_rrd_raw_values_verbose, update_rrd_text, update_rrd_values, update_rrd_values_verbose,
    with_rrd_locking,
};
#[cfg(unix)]
pub use rrd_create::create_rrd_at;
pub use rrd_create::rrd_create_r2;
pub use rrd_modify::{RrdRraOp, modify_rrd_file};
pub use rrd_number::{c_strtol, parse_rrd_number};
pub use rrd_resize::resize_rrd_file;
pub use rrd_restore::restore_rrd_file;
pub use storage::{DEFAULT_IDEMPOTENCY_WINDOW, DEFAULT_MAX_ROWS, Store, StoreError, StoreOptions};
pub use vdef::{VdefError, VdefFunction, VdefResult, evaluate_vdef};
