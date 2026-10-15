//! Text formatting of values for PRINT, GPRINT, dump and xport.
//!
//! RRDtool prints numbers with two different printf implementations, and
//! both are observable:
//!
//! - graph PRINT and GPRINT text, `graphv` values and `valstrfduration`
//!   numbers follow the host C library ([`host_number`]);
//! - dump, xport data values and xport PRINT entries use RRDtool's own
//!   printf, whose digits come from power-of-ten scaling ([`scaled_number`]).
//!
//! The PRINT value format language with its SI prefixes is in
//! [`PrintFormat`] and [`UnitScale`], durations in [`format_duration`] and
//! the placeholder for an unknown time in [`missing_time_text`]. Calling
//! the C library's `strftime` is left to the caller.
//!
//! The behavior is specified in `docs/compatibility/format-strings.md`.
//! Rules the specification leaves open were settled by black-box runs of
//! RRDtool 1.11.0 on macOS arm64, Linux aarch64 and Linux x86_64; the
//! comments at each rule say so.

mod duration;
mod field;
mod host;
mod missing_time;
mod print;
mod scaled;
mod si;
mod spec;

pub use duration::{DEFAULT_DURATION_FORMAT, DurationError, format_duration};
pub use field::Field;
pub use host::{HOST_NAN_SIGN, NanSign, host_number, printf_number};
pub use missing_time::missing_time_text;
pub use print::{InvalidPrintFormat, PRINT_FORMAT_PATTERN, PrintFormat, PrintText, Printer};
pub use scaled::{data_value, scaled_number};
pub use si::{UnitRequest, UnitScale, prefix};
pub use spec::{Flags, Notation, NumberSpec};

/// The NaN an invalid operation such as `inf / inf` produces on the
/// target: x86_64 sets its sign bit, ARM does not. Formatting code uses
/// it instead of relying on how an expression happens to be compiled.
pub(crate) fn operation_nan() -> f64 {
    let nan = f64::NAN.copysign(1.0);
    if cfg!(target_arch = "x86_64") {
        -nan
    } else {
        nan
    }
}
