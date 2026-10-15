//! Double-to-integer conversions that reproduce what the pinned RRDtool
//! builds observe for out-of-range values.
//!
//! In C, converting a `double` that is NaN or outside the target type's
//! range is undefined, so the result depends on the instruction the compiler
//! emits. The reference builds show two behaviors (see the platform notes in
//! `docs/compatibility.md`):
//!
//! - x86_64 (`cvttsd2si`) returns the type's minimum value for NaN and for
//!   any out-of-range input, whatever its sign.
//! - aarch64 (`fcvtzs`) saturates to the nearest bound and maps NaN to zero,
//!   which is also what Rust's `as` does on every target.
//!
//! In-range values truncate toward zero on both.

/// The x86 family's "integer indefinite" result applies there; every other
/// target is treated like aarch64.
const MINIMUM_ON_OVERFLOW: bool = cfg!(any(target_arch = "x86_64", target_arch = "x86"));

/// Converts like a C `(long)` cast on a 64-bit target.
pub(crate) fn c_long(value: f64) -> i64 {
    // Both bounds are powers of two, so they are exact as f64. Anything
    // strictly between them truncates to a representable i64.
    let in_range = (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value);
    if MINIMUM_ON_OVERFLOW && !in_range {
        i64::MIN
    } else {
        value as i64
    }
}

/// Converts like a C `(int)` cast where `int` is 32 bits.
pub(crate) fn c_int(value: f64) -> i32 {
    // Values in (-2^31 - 1, 2^31) truncate into range.
    let in_range = value > -2_147_483_649.0 && value < 2_147_483_648.0;
    if MINIMUM_ON_OVERFLOW && !in_range {
        i32::MIN
    } else {
        value as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_range_values_truncate_toward_zero() {
        assert_eq!(c_long(2.9), 2);
        assert_eq!(c_long(-2.9), -2);
        assert_eq!(c_int(2_147_483_647.9), i32::MAX);
        assert_eq!(c_int(-2_147_483_648.9), i32::MIN);
        assert_eq!(c_long(-9_223_372_036_854_775_808.0), i64::MIN);
    }

    #[test]
    fn out_of_range_values_follow_the_target() {
        let samples = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e30, -1e30];
        for value in samples {
            let long = c_long(value);
            let int = c_int(value);
            if MINIMUM_ON_OVERFLOW {
                assert_eq!(long, i64::MIN, "{value}");
                assert_eq!(int, i32::MIN, "{value}");
            } else if value.is_nan() {
                assert_eq!((long, int), (0, 0));
            } else if value > 0.0 {
                assert_eq!((long, int), (i64::MAX, i32::MAX), "{value}");
            } else {
                assert_eq!((long, int), (i64::MIN, i32::MIN), "{value}");
            }
        }
        assert_eq!(
            c_int(3e9),
            if MINIMUM_ON_OVERFLOW {
                i32::MIN
            } else {
                i32::MAX
            }
        );
        assert_eq!(c_long(3e9), 3_000_000_000);
    }
}
