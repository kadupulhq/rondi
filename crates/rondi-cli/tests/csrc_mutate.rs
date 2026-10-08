#![cfg(unix)]
//! Differentials for rrd_resize.c, rrd_tune.c and rrd_restore.c.

mod common;

const BASE: &[&str] = &[
    "create",
    "base.rrd",
    "--start",
    "1000000000",
    "--step",
    "10",
    "DS:a:GAUGE:20:U:U",
    "DS:b:COUNTER:20:0:U",
    "RRA:AVERAGE:0.5:1:5",
    "RRA:MAX:0.5:2:4",
];

// D2: rrd_resize.c:27-64 checks the reserved name, the argument count, the
// action and the row count in that order, parses both numbers with
// strtol(..., 0) and ignores trailing text; the RRA checks follow the open.
#[test]
fn d2_resize_argument_checks_follow_c_order_and_text() {
    let Some(sides) = common::Sides::new(&[BASE], &[]) else {
        return;
    };
    for args in [
        &["resize", "resize.rrd", "0", "GROW"][..],
        &["resize", "resize.rrd"],
        &["resize", "base.rrd", "0", "GROW"],
        &["resize", "base.rrd", "0", "BAD", "0"],
        &["resize", "base.rrd", "0", "grow", "1"],
        &["resize", "base.rrd", "0", "GROW", "0"],
        &["resize", "base.rrd", "0", "SHRINK", "-3"],
        &["resize", "missing.rrd", "9", "GROW", "1"],
        &["resize", "base.rrd", "-1", "GROW", "1"],
        &["resize", "base.rrd", "5", "GROW", "1"],
        &["resize", "base.rrd", "1", "SHRINK", "4"],
        &["resize", "base.rrd", "0", "GROW", "2abc"],
        &["resize", "base.rrd", "x", "GROW", "0x2"],
    ] {
        sides.assert_same(args);
        sides.assert_same_file("resize.rrd");
        for dir in [&sides.up, &sides.ro] {
            let _ = std::fs::remove_file(dir.join("resize.rrd"));
        }
    }
}
