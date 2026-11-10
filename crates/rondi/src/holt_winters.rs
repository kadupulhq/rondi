//! Holt-Winters archive roles and raw slot access (RS-53, HW-1, HW-14).
//!
//! Provenance: specification (docs/compatibility/holt-winters.md HW-1).

use crate::rrd_binary::{RrdArchiveInfo, RrdCdpPrepInfo};

/// What a Holt-Winters archive computes, from its consolidation name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Prediction { multiplicative: bool },
    Seasonal,
    DevSeasonal,
    DevPredict,
    Failures,
}

impl Role {
    /// Maps the raw consolidation name; `None` for classic functions.
    pub fn from_cf(raw: &[u8]) -> Option<Role> {
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        match &raw[..end] {
            b"HWPREDICT" => Some(Role::Prediction {
                multiplicative: false,
            }),
            b"MHWPREDICT" => Some(Role::Prediction {
                multiplicative: true,
            }),
            b"SEASONAL" => Some(Role::Seasonal),
            b"DEVSEASONAL" => Some(Role::DevSeasonal),
            b"DEVPREDICT" => Some(Role::DevPredict),
            b"FAILURES" => Some(Role::Failures),
            _ => None,
        }
    }
}

/// Archive parameter slots (RS-53).
pub mod param {
    pub const ALPHA: usize = 1;
    pub const GAMMA: usize = 1;
    pub const DELTA_POS: usize = 1;
    pub const BETA: usize = 2;
    pub const SMOOTHING_WINDOW: usize = 2;
    pub const DELTA_NEG: usize = 2;
    pub const DEPENDENT: usize = 3;
    pub const SMOOTH_IDX: usize = 4;
    pub const WINDOW_LEN: usize = 4;
    pub const THRESHOLD: usize = 5;
}

/// CDP slots (RS-53).
pub mod cdp {
    pub const INTERCEPT: usize = 2;
    pub const SEASONAL: usize = 2;
    pub const LAST_INTERCEPT: usize = 3;
    pub const LAST_SEASONAL: usize = 3;
    pub const SLOPE: usize = 4;
    pub const LAST_SLOPE: usize = 5;
    pub const NAN_COUNT: usize = 6;
    pub const INIT_FLAG: usize = 6;
    pub const LAST_NAN_COUNT: usize = 7;
    pub const PRIMARY: usize = 8;
    pub const SECONDARY: usize = 9;
}

impl RrdArchiveInfo {
    pub fn param_f64(&self, slot: usize) -> f64 {
        f64::from_bits(self.param_words[slot])
    }

    pub fn param_u64(&self, slot: usize) -> u64 {
        self.param_words[slot]
    }
}

impl RrdCdpPrepInfo {
    pub fn word_f64(&self, slot: usize) -> f64 {
        f64::from_bits(self.words[slot])
    }

    pub fn word_u64(&self, slot: usize) -> u64 {
        self.words[slot]
    }

    pub fn set_f64(&mut self, slot: usize, value: f64) {
        self.set_u64(slot, value.to_bits());
    }

    /// Stores a raw word and keeps the four named fields in sync.
    pub fn set_u64(&mut self, slot: usize, value: u64) {
        self.words[slot] = value;
        match slot {
            0 => self.value = f64::from_bits(value),
            1 => self.unknown_datapoints = value,
            8 => self.primary_value = f64::from_bits(value),
            9 => self.secondary_value = f64::from_bits(value),
            _ => {}
        }
    }
}

/// The failure history: bytes 0 to `min(window_len, 28)` of the CDP, newest
/// first (HW-14). Never reads past the 80-byte entry.
///
/// Compatibility: the history is a byte string at the start of the CDP.
/// Provenance: specification (docs/compatibility/holt-winters.md HW-14).
pub fn failure_history(prep: &RrdCdpPrepInfo, window_len: u64) -> Vec<u8> {
    let len = window_len.min(28) as usize;
    prep.words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .take(len)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrd_binary::inspect_path;
    use std::path::PathBuf;

    #[test]
    fn reader_keeps_raw_hw_words() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../rondi-cli/tests/data/hw/inputs/hwimp.rrd");
        let info = inspect_path(&path).expect("hwimp.rrd reads");
        let find = |name: &[u8]| {
            info.archives
                .iter()
                .find(|a| a.consolidation.as_bytes() == name)
                .expect("archive present")
        };
        let hw = find(b"HWPREDICT");
        assert_eq!(hw.param_f64(param::ALPHA), 0.1);
        assert_eq!(hw.param_f64(param::BETA), 0.01);
        assert!(hw.cdp_prep[0].word_f64(cdp::INTERCEPT).is_nan());
        assert_eq!(hw.cdp_prep[0].word_u64(cdp::NAN_COUNT), 1);
        let failures = find(b"FAILURES");
        assert_eq!(failures.param_u64(param::WINDOW_LEN), 9);
        assert_eq!(failures.param_u64(param::THRESHOLD), 7);
        assert_eq!(failure_history(&failures.cdp_prep[0], 9), vec![0; 9]);
        assert_eq!(failure_history(&failures.cdp_prep[0], 1000).len(), 28);
    }

    #[test]
    fn role_from_cf() {
        assert_eq!(
            Role::from_cf(b"HWPREDICT"),
            Some(Role::Prediction {
                multiplicative: false
            })
        );
        assert_eq!(
            Role::from_cf(b"MHWPREDICT"),
            Some(Role::Prediction {
                multiplicative: true
            })
        );
        assert_eq!(Role::from_cf(b"SEASONAL"), Some(Role::Seasonal));
        assert_eq!(Role::from_cf(b"DEVSEASONAL"), Some(Role::DevSeasonal));
        assert_eq!(Role::from_cf(b"DEVPREDICT"), Some(Role::DevPredict));
        assert_eq!(Role::from_cf(b"FAILURES"), Some(Role::Failures));
        assert_eq!(Role::from_cf(b"AVERAGE"), None);
    }
}
