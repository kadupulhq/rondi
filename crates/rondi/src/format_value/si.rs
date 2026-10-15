//! SI prefixes for `%s` and `%S` in PRINT and GPRINT formats.

use super::operation_nan;

/// Prefix characters for powers -6 to 6 of the base.
const PREFIXES: [char; 13] = [
    'a', 'f', 'p', 'n', 'u', 'm', ' ', 'k', 'M', 'G', 'T', 'P', 'E',
];

/// Whether a format chooses its own power or reuses the remembered one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitRequest {
    /// `%s`: choose a power for this value and remember it.
    Own,
    /// `%S`: reuse the remembered power; without one, choose like `%s`
    /// unless the value is zero.
    Shared,
}

/// The SI power shared by the PRINT and GPRINT items of one graph.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitScale {
    base: f64,
    remembered: Option<i32>,
}

impl UnitScale {
    /// `base` is the `--base` value, 1000 or 1024.
    pub fn new(base: u32) -> Self {
        Self {
            base: f64::from(base),
            remembered: None,
        }
    }

    /// Divides `value` by the chosen power of the base and returns the
    /// scaled value with its prefix character.
    pub fn apply(&mut self, value: f64, request: UnitRequest) -> (f64, char) {
        let power = match (request, self.remembered) {
            (UnitRequest::Shared, Some(power)) => power,
            (UnitRequest::Shared, None) if value == 0.0 => return (value, ' '),
            _ => {
                let power = self.power_of(value);
                self.remembered = Some(power);
                power
            }
        };
        (self.divide(value, power), prefix(power))
    }

    // Compatibility: the power is floor(ln|v| / ln base) truncated to a C
    // int; zero and NaN use power 0. An infinite quotient becomes INT_MIN
    // on x86_64 and saturates on other targets, so infinities print
    // `inf ?` on x86_64 and `nan ?` elsewhere (FS-17, FS-20).
    // Provenance: black-box compatibility test (format-si and format-print
    // vectors).
    fn power_of(&self, value: f64) -> i32 {
        if value == 0.0 || value.is_nan() {
            return 0;
        }
        let quotient = (value.abs().ln() / self.base.ln()).floor();
        if cfg!(target_arch = "x86_64") && !quotient.is_finite() {
            i32::MIN
        } else {
            quotient as i32
        }
    }

    fn divide(&self, value: f64, power: i32) -> f64 {
        if power == 0 {
            return value;
        }
        let scaled = value / self.base.powf(f64::from(power));
        if scaled.is_nan() && !value.is_nan() {
            return operation_nan();
        }
        scaled
    }
}

/// The prefix for a power of the base; `?` outside -6..=6.
pub fn prefix(power: i32) -> char {
    power
        .checked_add(6)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| PREFIXES.get(index))
        .copied()
        .unwrap_or('?')
}

#[cfg(test)]
mod tests {
    use super::{UnitRequest, UnitScale, prefix};

    #[test]
    fn prefixes_cover_atto_to_exa() {
        assert_eq!(prefix(-6), 'a');
        assert_eq!(prefix(0), ' ');
        assert_eq!(prefix(1), 'k');
        assert_eq!(prefix(6), 'E');
        assert_eq!(prefix(7), '?');
        assert_eq!(prefix(-7), '?');
        assert_eq!(prefix(i32::MIN), '?');
        assert_eq!(prefix(i32::MAX), '?');
    }

    #[test]
    fn own_power_is_floor_of_the_log() {
        let mut scale = UnitScale::new(1000);
        assert_eq!(scale.apply(1500.0, UnitRequest::Own), (1.5, 'k'));
        assert_eq!(scale.apply(999.9999, UnitRequest::Own), (999.9999, ' '));
        assert_eq!(scale.apply(-2e-3, UnitRequest::Own), (-2.0, 'm'));
        assert_eq!(scale.apply(1e-21, UnitRequest::Own), (1.0, '?'));
        let mut binary = UnitScale::new(1024);
        assert_eq!(binary.apply(2048.0, UnitRequest::Own), (2.0, 'k'));
    }

    // format-si-077 to -079: %S follows the most recent chosen power.
    #[test]
    fn shared_power_is_remembered() {
        let mut scale = UnitScale::new(1000);
        assert_eq!(scale.apply(0.0, UnitRequest::Shared), (0.0, ' '));
        assert_eq!(scale.apply(2500.0, UnitRequest::Shared), (2.5, 'k'));
        assert_eq!(scale.apply(1_500_000.0, UnitRequest::Own), (1.5, 'M'));
        assert_eq!(scale.apply(2500.0, UnitRequest::Shared), (0.0025, 'M'));
        assert_eq!(scale.apply(0.0, UnitRequest::Shared), (0.0, 'M'));
        assert_eq!(scale.apply(2500.0, UnitRequest::Own), (2.5, 'k'));
    }

    #[test]
    fn nan_keeps_power_zero_and_sets_it() {
        let mut scale = UnitScale::new(1000);
        let (value, symbol) = scale.apply(f64::NAN, UnitRequest::Shared);
        assert!(value.is_nan());
        assert_eq!(symbol, ' ');
        assert_eq!(scale.apply(2500.0, UnitRequest::Shared), (2500.0, ' '));
    }

    #[test]
    fn infinity_and_underflow_use_question_mark() {
        let mut scale = UnitScale::new(1000);
        let (value, symbol) = scale.apply(f64::INFINITY, UnitRequest::Own);
        assert_eq!(symbol, '?');
        if cfg!(target_arch = "x86_64") {
            assert_eq!(value, f64::INFINITY);
        } else {
            assert!(value.is_nan() && value.is_sign_positive());
        }
        let mut scale = UnitScale::new(1000);
        assert_eq!(scale.apply(5e-324, UnitRequest::Own), (f64::INFINITY, '?'));
    }
}
