#![cfg(unix)]
//! Damaged-header repros: RRDtool trusts the fixed-width name fields and the
//! row and PDP counts it reads back, so each test patches an upstream-created
//! file and compares the commands against pinned RRDtool.

#[macro_use]
mod common;

use common::Sides;

const STAT_HEAD: usize = 128;
const DS_DEF: usize = 120;
const RRA_DEF: usize = 120;
const LIVE_HEAD: usize = 16;

/// One GAUGE `g` and the AVERAGE, MAX, MIN and LAST archives, updated a
/// few times so every field has content.
fn fixture() -> Option<Sides> {
    Sides::new(
        &[
            &[
                "create",
                "t.rrd",
                "--start",
                "1000000000",
                "--step",
                "300",
                "DS:g:GAUGE:600:0:100",
                "RRA:AVERAGE:0.5:1:20",
                "RRA:MAX:0.5:3:10",
                "RRA:MIN:0:3:10",
                "RRA:LAST:0.99:2:10",
            ],
            &[
                "update",
                "t.rrd",
                "1000000300:10",
                "1000000600:20",
                "1000000900:30",
                "1000001200:33",
            ],
        ],
        &[],
    )
}

/// Overwrites `bytes` at `offset` in both copies of t.rrd.
fn patch(sides: &Sides, offset: usize, bytes: &[u8]) {
    let mut file = std::fs::read(sides.up.join("t.rrd")).unwrap();
    file[offset..offset + bytes.len()].copy_from_slice(bytes);
    sides.write("t.rrd", &file);
}

fn last_ds() -> usize {
    STAT_HEAD + DS_DEF + 4 * RRA_DEF + LIVE_HEAD
}

// rrd_info_print and rrd_tool.c's lastupdate print names and last_ds with
// %s, so bytes that are not UTF-8 reach stdout unchanged.
#[test]
fn info_and_lastupdate_print_raw_name_bytes() {
    let Some(sides) = fixture() else {
        return;
    };
    patch(&sides, STAT_HEAD, b"\xe7\0");
    patch(&sides, last_ds(), b"1\xc350\0");
    sides.assert_same_bytes(&["info", "t.rrd"]);
    sides.assert_same_bytes(&["lastupdate", "t.rrd"]);
}

fn rra_def(index: usize) -> usize {
    STAT_HEAD + DS_DEF + index * RRA_DEF
}

// dst_conv and rrd_cf_conv set an error for a name they do not know and
// return -1. info and dump print everything through their default branches
// and exit 1 with the last such error; fetch does the same when the bad CF
// belongs to an RRA it did not pick; tune -h never converts the type.
#[test]
fn unknown_type_and_cf_names_report_conversion_errors() {
    let Some(sides) = fixture() else {
        return;
    };
    patch(&sides, STAT_HEAD + 20, b"GAU\xaaE\0");
    patch(&sides, rra_def(1), b"MAX\xcc\0");
    for args in [
        &["info", "t.rrd"][..],
        &["dump", "t.rrd"],
        &[
            "fetch",
            "t.rrd",
            "AVERAGE",
            "-s",
            "1000000000",
            "-e",
            "1000001200",
        ],
        &[
            "fetch",
            "t.rrd",
            "MAX",
            "-s",
            "1000000000",
            "-e",
            "1000001200",
        ],
        &["tune", "t.rrd", "-h", "g:900"],
    ] {
        sides.assert_same_bytes(args);
    }
    sides.assert_same_file("t.rrd");

    // Only a bad CF: the last RRA's name is the message.
    let Some(cf_only) = fixture() else {
        return;
    };
    patch(&cf_only, rra_def(3), b"\x01\0");
    cf_only.assert_same_bytes(&["info", "t.rrd"]);
    cf_only.assert_same_bytes(&["dump", "t.rrd"]);
}

// rrd_fetch_fn's own message when no RRA serves the CF.
#[test]
fn fetch_without_matching_rra_uses_rrd_fetch_message() {
    let Some(sides) = fixture() else {
        return;
    };
    sides.assert_same(&["fetch", "t.rrd", "HWPREDICT", "-s", "1000000000"]);
}
