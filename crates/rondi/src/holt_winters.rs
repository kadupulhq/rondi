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
pub(crate) mod param {
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
pub(crate) mod cdp {
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
    pub(crate) fn param_f64(&self, slot: usize) -> f64 {
        f64::from_bits(self.param_words[slot])
    }

    pub(crate) fn param_u64(&self, slot: usize) -> u64 {
        self.param_words[slot]
    }
}

impl RrdCdpPrepInfo {
    pub(crate) fn word_f64(&self, slot: usize) -> f64 {
        f64::from_bits(self.words[slot])
    }

    pub(crate) fn word_u64(&self, slot: usize) -> u64 {
        self.words[slot]
    }

    pub(crate) fn set_f64(&mut self, slot: usize, value: f64) {
        self.set_u64(slot, value.to_bits());
    }

    /// Stores a raw word and keeps the four named fields in sync.
    pub(crate) fn set_u64(&mut self, slot: usize, value: u64) {
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
pub(crate) fn failure_history(prep: &RrdCdpPrepInfo, window_len: u64) -> Vec<u8> {
    let len = window_len.min(28) as usize;
    prep.words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .take(len)
        .collect()
}

/// One value of an `info` line for a Holt-Winters archive.
#[derive(Debug, Clone, PartialEq)]
pub enum InfoValue {
    Float(f64),
    Uint(u64),
    /// A string; `info` prints it quoted, rrdcached's `INFO` as is.
    Text(Vec<u8>),
}

/// The largest FAILURES window `info` can print (HW-5).
pub(crate) const MAX_WINDOW_LEN: u64 = 28;

/// The `info` lines of a Holt-Winters archive after `pdp_per_row`, as
/// `(key suffix after "rra[i].", value)` pairs; `None` for a classic archive.
/// `version` is the file's version text. Fails for a FAILURES window above 28.
///
/// Compatibility: parameter keys, then per-DS CDP keys, in the HW-3 order;
/// `smoothing_window` appears only when the version reads as 4 or more.
/// Provenance: specification (docs/compatibility/holt-winters.md HW-3, HW-5).
pub fn info_fields(
    archive: &RrdArchiveInfo,
    version: &str,
) -> Result<Option<Vec<(String, InfoValue)>>, String> {
    use InfoValue::{Float, Text, Uint};
    let Some(role) = Role::from_cf(&archive.raw_consolidation) else {
        return Ok(None);
    };
    let window_len = archive.param_u64(param::WINDOW_LEN);
    if role == Role::Failures && window_len > MAX_WINDOW_LEN {
        return Err(format!(
            "FAILURES window length {window_len} exceeds {MAX_WINDOW_LEN}"
        ));
    }
    let shows_window = crate::rrd_binary::version_number(version.as_bytes()) >= 4;
    let mut out: Vec<(String, InfoValue)> = Vec::new();
    let mut put = |key: String, value: InfoValue| out.push((key, value));
    match role {
        Role::Prediction { .. } => {
            put("alpha".into(), Float(archive.param_f64(param::ALPHA)));
            put("beta".into(), Float(archive.param_f64(param::BETA)));
        }
        Role::Seasonal | Role::DevSeasonal => {
            put("gamma".into(), Float(archive.param_f64(param::GAMMA)));
            if shows_window {
                put(
                    "smoothing_window".into(),
                    Float(archive.param_f64(param::SMOOTHING_WINDOW)),
                );
            }
        }
        Role::DevPredict => {}
        Role::Failures => {
            put(
                "delta_pos".into(),
                Float(archive.param_f64(param::DELTA_POS)),
            );
            put(
                "delta_neg".into(),
                Float(archive.param_f64(param::DELTA_NEG)),
            );
            put(
                "failure_threshold".into(),
                Uint(archive.param_u64(param::THRESHOLD)),
            );
            put("window_length".into(), Uint(window_len));
        }
    }
    for (ds, prep) in archive.cdp_prep.iter().enumerate() {
        let key = |name: &str| format!("cdp_prep[{ds}].{name}");
        match role {
            Role::Prediction { .. } => {
                put(key("intercept"), Float(prep.word_f64(cdp::INTERCEPT)));
                put(key("slope"), Float(prep.word_f64(cdp::SLOPE)));
                put(key("NaN_count"), Uint(prep.word_u64(cdp::NAN_COUNT)));
            }
            Role::Seasonal => put(key("seasonal"), Float(prep.word_f64(cdp::SEASONAL))),
            Role::DevSeasonal => put(key("deviation"), Float(prep.word_f64(cdp::SEASONAL))),
            Role::DevPredict => {}
            Role::Failures => {
                let text = failure_history(prep, window_len)
                    .iter()
                    .map(|&byte| if byte == 1 { b'1' } else { b'0' })
                    .collect();
                put(key("history"), Text(text));
            }
        }
    }
    Ok(Some(out))
}

/// The `<params>` lines of a Holt-Winters archive in `dump`, each ready to
/// write (tab-indented, newline-terminated); `None` for a classic archive.
/// Doubles print as `%0.10e` with `nan` for an unknown (`<params>` spells it
/// differently from `<cdp_prep>`). Fails for a FAILURES window above 28.
///
/// Compatibility: parameter elements in the HW-4 order; `smoothing_window`
/// only under the version rule of HW-3.
/// Provenance: specification (docs/compatibility/holt-winters.md HW-4, HW-5).
pub(crate) fn dump_params(
    archive: &RrdArchiveInfo,
    version: &str,
) -> Result<Option<String>, String> {
    use crate::rrd_binary::format_rrd_float as f;
    let Some(role) = Role::from_cf(&archive.raw_consolidation) else {
        return Ok(None);
    };
    let window_len = archive.param_u64(param::WINDOW_LEN);
    if role == Role::Failures && window_len > MAX_WINDOW_LEN {
        return Err(format!(
            "FAILURES window length {window_len} exceeds {MAX_WINDOW_LEN}"
        ));
    }
    let shows_window = crate::rrd_binary::version_number(version.as_bytes()) >= 4;
    let dep = archive.param_u64(param::DEPENDENT);
    let mut out = String::new();
    let mut put = |name: &str, value: String| {
        out.push_str(&format!("\t\t<{name}>{value}</{name}>\n"));
    };
    match role {
        Role::Prediction { .. } => {
            put("hw_alpha", f(archive.param_f64(param::ALPHA)));
            put("hw_beta", f(archive.param_f64(param::BETA)));
            put("dependent_rra_idx", dep.to_string());
        }
        Role::Seasonal | Role::DevSeasonal => {
            put("seasonal_gamma", f(archive.param_f64(param::GAMMA)));
            put(
                "seasonal_smooth_idx",
                archive.param_u64(param::SMOOTH_IDX).to_string(),
            );
            if shows_window {
                put(
                    "smoothing_window",
                    f(archive.param_f64(param::SMOOTHING_WINDOW)),
                );
            }
            put("dependent_rra_idx", dep.to_string());
        }
        Role::DevPredict => put("dependent_rra_idx", dep.to_string()),
        Role::Failures => {
            put("delta_pos", f(archive.param_f64(param::DELTA_POS)));
            put("delta_neg", f(archive.param_f64(param::DELTA_NEG)));
            put("window_len", window_len.to_string());
            put(
                "failure_threshold",
                archive.param_u64(param::THRESHOLD).to_string(),
            );
            put("dependent_rra_idx", dep.to_string());
        }
    }
    Ok(Some(out))
}

/// The role-specific `<cdp_prep>` `<ds>` lines after `secondary_value`
/// (tab-indented, newline-terminated); `None` for a classic archive.
///
/// Compatibility: element names and order of HW-4; the history prints each
/// byte as a decimal integer with no separator, signed except on Linux
/// aarch64.
/// Provenance: specification (docs/compatibility/holt-winters.md HW-4).
pub(crate) fn dump_cdp(archive: &RrdArchiveInfo, prep: &RrdCdpPrepInfo) -> Option<String> {
    use crate::rrd_binary::format_rrd_value as f;
    let role = Role::from_cf(&archive.raw_consolidation)?;
    let mut out = String::new();
    let mut put = |name: &str, value: String| {
        out.push_str(&format!("\t\t\t<{name}>{value}</{name}>\n"));
    };
    match role {
        Role::Prediction { .. } => {
            put("intercept", f(prep.word_f64(cdp::INTERCEPT)));
            put("last_intercept", f(prep.word_f64(cdp::LAST_INTERCEPT)));
            put("slope", f(prep.word_f64(cdp::SLOPE)));
            put("last_slope", f(prep.word_f64(cdp::LAST_SLOPE)));
            put("nan_count", prep.word_u64(cdp::NAN_COUNT).to_string());
            put(
                "last_nan_count",
                prep.word_u64(cdp::LAST_NAN_COUNT).to_string(),
            );
        }
        Role::Seasonal | Role::DevSeasonal => {
            put("seasonal", f(prep.word_f64(cdp::SEASONAL)));
            put("last_seasonal", f(prep.word_f64(cdp::LAST_SEASONAL)));
            put("init_flag", prep.word_u64(cdp::INIT_FLAG).to_string());
        }
        Role::DevPredict => {}
        Role::Failures => {
            let history: String = failure_history(prep, archive.param_u64(param::WINDOW_LEN))
                .iter()
                .map(|&byte| {
                    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
                    let shown = i32::from(byte);
                    #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
                    let shown = i32::from(byte as i8);
                    shown.to_string()
                })
                .collect();
            put("history", history);
        }
    }
    Some(out)
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
