#![cfg(unix)]
//! Repros from the rrd_update.c line-by-line review. Each test runs the same
//! command sequence against pinned RRDtool and the Rondi alias, starting from
//! one upstream-created file so the randomized initial row cannot differ, and
//! compares stdout, stderr, exit status and the final file bytes.
//! Option-form cases (A1, A2, A4) wait for the shared option parser.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

struct Step<'a> {
    args: &'a [&'a str],
    env: &'a [(&'a str, &'a str)],
}

fn step<'a>(args: &'a [&'a str]) -> Step<'a> {
    Step { args, env: &[] }
}

fn run_side(binary: &Path, seed: &Path, dir: &Path, steps: &[Step<'_>]) -> (String, Vec<u8>) {
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join("t.rrd");
    std::fs::copy(seed, &file).unwrap();
    let mut transcript = String::new();
    for step in steps {
        let mut command = Command::new(binary);
        command
            .current_dir(dir)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS");
        for (key, value) in step.env {
            command.env(key, value);
        }
        let output = command.args(step.args).output().unwrap();
        transcript.push_str(&format!(
            "$ {:?}\nstatus={:?}\nstdout:\n{}stderr:\n{}",
            step.args,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    (transcript, std::fs::read(&file).unwrap())
}

fn assert_same(create: &[&str], steps: &[Step<'_>]) {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        oracle_skip!("skipping RRDtool differential test: rrdtool is not installed");
        return;
    }
    common::require_oracle();
    let temp = tempfile::tempdir().unwrap();
    let seed = temp.path().join("seed.rrd");
    let created = Command::new("rrdtool")
        .arg("create")
        .arg(&seed)
        .args(create)
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let upstream = run_side(Path::new("rrdtool"), &seed, &temp.path().join("up"), steps);
    let rondi = run_side(&alias, &seed, &temp.path().join("ro"), steps);
    assert_eq!(rondi.0, upstream.0, "command output differs");
    assert!(rondi.1 == upstream.1, "final file bytes differ");
}

const TWO_GAUGE: &[&str] = &[
    "--start",
    "1000000000",
    "--step",
    "10",
    "DS:a:GAUGE:20:U:U",
    "DS:b:GAUGE:20:U:U",
    "RRA:AVERAGE:0.5:1:5",
];

// A3: parse_ds reports the remainder of the argument after the extra colon.
#[test]
fn a3_extra_data_message_reports_remaining_text() {
    assert_same(
        TWO_GAUGE,
        &[step(&["update", "t.rrd", "1000000010:1:2:7:8"])],
    );
}

// A5: update_pdp_prep treats any value whose first byte is 'U' as unknown,
// and nothing else (lowercase u/unknown go to rrd_strtodbl and fail).
#[test]
fn a5_unknown_is_first_byte_uppercase_u() {
    assert_same(
        TWO_GAUGE,
        &[
            step(&["update", "t.rrd", "1000000010:Uxyz:1"]),
            step(&["update", "t.rrd", "1000000020:u:1"]),
            step(&["update", "t.rrd", "1000000030:unknown:1"]),
        ],
    );
}

// A6: rrd_strtodbl distinguishes a partial conversion in its message.
#[test]
fn a6_partial_conversion_messages() {
    assert_same(
        TWO_GAUGE,
        &[
            step(&["update", "t.rrd", "1000000010:12abc:1"]),
            step(&["update", "t.rrd", "1000000010x:1:1"]),
        ],
    );
}

// A7: a sample that fails in update_pdp_prep has already replaced last_ds of
// earlier data sources in the mapped header; the next COUNTER delta uses it.
#[test]
fn a7_failed_sample_leaves_earlier_last_ds() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:c:COUNTER:20:U:U",
            "DS:g:GAUGE:20:U:U",
            "RRA:LAST:0.5:1:5",
        ],
        &[
            step(&["update", "t.rrd", "1000000010:100:1"]),
            step(&["update", "t.rrd", "1000000020:200:abc"]),
            step(&["update", "t.rrd", "1000000030:300:1"]),
        ],
    );
}

// A8: past the heartbeat the value is never parsed, only copied to last_ds.
#[test]
fn a8_value_past_heartbeat_is_not_parsed() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:20:U:U",
            "RRA:LAST:0.5:1:5",
        ],
        &[step(&["update", "t.rrd", "1000000100:abc"])],
    );
}

// A9: DCOUNTER/DDERIVE parse nothing while last_ds is unknown.
#[test]
fn a9_dcounter_value_unparsed_while_last_ds_unknown() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:d:DCOUNTER:20:U:U",
            "RRA:LAST:0.5:1:5",
        ],
        &[
            step(&["update", "t.rrd", "1000000010:abc"]),
            step(&["update", "t.rrd", "1000000020:5"]),
        ],
    );
}

// A10: the timestamp check (-2, skipped by --skip-past-updates) runs before
// any value is converted.
#[test]
fn a10_skip_past_updates_precedes_value_validation() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:20:U:U",
            "RRA:LAST:0.5:1:5",
        ],
        &[
            step(&["update", "t.rrd", "1000000010:1"]),
            step(&[
                "update",
                "t.rrd",
                "--skip-past-updates",
                "1000000005:abc",
                "1000000020:2",
            ]),
        ],
    );
}

// A11: COUNTER/DERIVE use a digit scan with "not a simple ... integer" text;
// an empty value passes it, and rrd_diff accepts up to LAST_DS_LEN digits
// and turns longer values into unknown without an error.
#[test]
fn a11_counter_digit_scan_and_lengths() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:c:COUNTER:600:U:U",
            "RRA:LAST:0.5:1:50",
        ],
        &[
            step(&["update", "t.rrd", "1000000010:1.5"]),
            step(&["update", "t.rrd", "1000000020:-5"]),
            step(&["update", "t.rrd", "1000000030:"]),
            step(&[
                "update",
                "t.rrd",
                "1000000040:123456789012345678901234567890",
            ]),
            step(&[
                "update",
                "t.rrd",
                "1000000050:1234567890123456789012345678901",
            ]),
            step(&["update", "t.rrd", "1000000060:7"]),
        ],
    );
}

// A12: rrd_diff takes the sign from any '-' before the first digit, so "-0"
// is negative.
#[test]
fn a12_derive_negative_zero_sign() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:d:DERIVE:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[step(&[
            "update",
            "t.rrd",
            "1000000010:-3",
            "1000000020:-0",
            "1000000030:5",
            "1000000040:-0",
        ])],
    );
}

// A13: rrd_diff converts the decimal difference with rrd_strtod's digit
// accumulation, which is not correctly rounded above 2^53.
#[test]
fn a13_counter_delta_uses_rrd_strtod_rounding() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:c:COUNTER:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[step(&[
            "update",
            "t.rrd",
            "1000000010:0",
            "1000000020:14189154938208861744",
        ])],
    );
}

// A14: updatev reports return_value = -1 when an update fails.
#[test]
fn a14_updatev_return_value_on_failure() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[
            step(&["updatev", "t.rrd", "1000000010:3", "1000000020:abc"]),
            step(&["updatev", "t.rrd", "1000000005:3"]),
        ],
    );
}

// A15: rrd_update_v rejects only an *empty* RRDCACHED_ADDRESS and otherwise
// updates locally; it has no --daemon option.
#[test]
fn a15_updatev_rrdcached_address_handling() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[
            Step {
                args: &["updatev", "t.rrd", "1000000010:1"],
                env: &[("RRDCACHED_ADDRESS", "")],
            },
            Step {
                args: &["updatev", "t.rrd", "1000000020:2"],
                env: &[("RRDCACHED_ADDRESS", "unix:/nonexistent")],
            },
            step(&["updatev", "t.rrd", "--daemon", "unix:/x", "1000000030:3"]),
        ],
    );
}

// A16: parse_template rejects more names than data sources.
#[test]
fn a16_template_longer_than_ds_count() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[step(&["update", "t.rrd", "-t", "g:g", "1000000010:1:2"])],
    );
}

// A17: at-style timestamp errors from get_time_from_reading.
#[test]
fn a17_at_style_time_errors() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[
            step(&["update", "t.rrd", "end-1h@5"]),
            step(&["update", "t.rrd", "garbage@5"]),
        ],
    );
}

// A18: rrd_strtod skips C isspace (including \v) and lets the int exponent wrap.
#[test]
fn a18_rrd_strtod_whitespace_and_exponent_wrap() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:600:U:U",
            "RRA:LAST:0.5:1:10",
        ],
        &[
            step(&["update", "t.rrd", "1000000010:1e4294967297"]),
            step(&["update", "t.rrd", "1000000020:\u{b}5"]),
        ],
    );
}

// Control: the consolidation path itself matches byte-for-byte.
#[test]
fn control_core_update_math_matches() {
    assert_same(
        &[
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:600:U:U",
            "DS:h:COUNTER:600:U:U",
            "RRA:LAST:0.5:1:4",
            "RRA:MAX:0.5:3:4",
        ],
        &[step(&[
            "updatev",
            "t.rrd",
            "1000000003.25:1:5",
            "1000000017:U:7",
            "1000000088.5:-1e300:4294967300",
            "1000000099:inf:4",
        ])],
    );
}
