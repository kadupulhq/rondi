#![cfg(unix)]
//! Repros from the rrd_dump.c review, compared against pinned RRDtool.

#[macro_use]
mod common;

// rrd_dump.c:81 CB_FMTS formats values with rrd_snprintf("%0.10e"): fmtflt
// rounds by its own scaling, drops the sign of -0 and prints subnormals
// without normalizing them.
#[test]
fn dump_values_use_rrd_snprintf() {
    let Some(sides) = common::Sides::new(
        &[
            &[
                "create",
                "d.rrd",
                "-b",
                "1000000000",
                "-s",
                "1",
                "DS:x:GAUGE:10:U:U",
                "RRA:LAST:0:1:12",
            ],
            &[
                "update",
                "d.rrd",
                "--",
                "1000000001:53631508319.5",
                "1000000002:1e-310",
                "1000000003:4.9e-324",
                "1000000004:9.99999999995",
                "1000000005:-1.23456789015e-300",
                "1000000006:1e308",
                "1000000007:inf",
                "1000000008:-inf",
                "1000000009:2.2250738585072014e-308",
                "1000000010:-5",
                "1000000011:-0",
            ],
        ],
        &[],
    ) else {
        return;
    };
    sides.assert_same(&["dump", "d.rrd"]);
}
