//! The value ordering used by SORT, MEDIAN, PERCENT and PREDICTPERC.
//!
//! Compatibility: RRDtool sorts with the C library `qsort` and a comparison
//! that is not a total order once infinities are present, so the result
//! depends on the host's sort algorithm. Rondi hands the same comparison to
//! the host `qsort` instead of imitating any particular algorithm.
//! Provenance: reference-source-inspection; see "Ordering used by SORT,
//! MEDIAN, PERCENT and PREDICTPERC" in docs/compatibility/rpn.md.

use std::cmp::Ordering;

/// Sorts `values` in place, smallest first.
pub(super) fn sort(values: &mut [f64]) {
    if values.len() < 2 {
        return;
    }
    host_sort(values);
}

/// Compares `x` with `y`: unknown values are smallest; otherwise an infinite
/// operand decides by its platform classification, checking `x` first; finite
/// values compare numerically.
fn compare(x: f64, y: f64) -> Ordering {
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) if x.is_infinite() => infinity_class(x),
        (false, false) if y.is_infinite() => infinity_class(y),
        (false, false) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
    }
}

/// Compatibility: macOS classifies both infinities as "greater"; glibc
/// classifies negative infinity as "less".
/// Provenance: black-box compatibility test (sort-18, sort-19 and their
/// linux-aarch64 recordings).
fn infinity_class(value: f64) -> Ordering {
    if cfg!(target_vendor = "apple") || value > 0.0 {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

#[cfg(unix)]
fn host_sort(values: &mut [f64]) {
    use std::ffi::{c_int, c_void};

    extern "C" fn compare_raw(left: *const c_void, right: *const c_void) -> c_int {
        // SAFETY: qsort passes pointers to elements of the f64 slice below.
        let (x, y) = unsafe { (*left.cast::<f64>(), *right.cast::<f64>()) };
        match compare(x, y) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    // SAFETY: the pointer, element count and element size describe the
    // exclusively borrowed slice, and the comparator neither unwinds nor
    // retains the pointers.
    unsafe {
        libc::qsort(
            values.as_mut_ptr().cast::<c_void>(),
            values.len(),
            std::mem::size_of::<f64>(),
            Some(compare_raw),
        );
    }
}

/// Without a C library, an insertion sort: it accepts an inconsistent
/// comparison without failing, which the standard library sorts do not
/// promise.
#[cfg(not(unix))]
fn host_sort(values: &mut [f64]) {
    for next in 1..values.len() {
        let mut slot = next;
        while slot > 0 && compare(values[slot - 1], values[slot]) == Ordering::Greater {
            values.swap(slot - 1, slot);
            slot -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_sort_first_and_finite_values_ascend() {
        let mut values = [3.0, f64::NAN, -1.0, 2.5, f64::NAN, 0.0];
        sort(&mut values);
        assert!(values[0].is_nan() && values[1].is_nan());
        assert_eq!(&values[2..], &[-1.0, 0.0, 2.5, 3.0]);
    }

    #[test]
    fn infinities_use_the_platform_classification() {
        assert_eq!(compare(f64::INFINITY, 1.0), Ordering::Greater);
        assert_eq!(compare(1.0, f64::INFINITY), Ordering::Greater);
        let negative = if cfg!(target_vendor = "apple") {
            Ordering::Greater
        } else {
            Ordering::Less
        };
        assert_eq!(compare(f64::NEG_INFINITY, 1.0), negative);
        assert_eq!(compare(f64::NAN, f64::NEG_INFINITY), Ordering::Less);
    }

    #[test]
    fn inconsistent_comparisons_still_permute_the_input() {
        let mut values = [f64::INFINITY, 1.0, f64::NEG_INFINITY, f64::NAN, -2.0, 7.0];
        sort(&mut values);
        let mut seen: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        seen.sort();
        assert_eq!(seen, ["-2", "-inf", "1", "7", "NaN", "inf"]);
    }
}
