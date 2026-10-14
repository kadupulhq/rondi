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
fn sides() -> Option<Sides> {
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
    let Some(sides) = sides() else {
        return;
    };
    patch(&sides, STAT_HEAD, b"\xe7\0");
    patch(&sides, last_ds(), b"1\xc350\0");
    sides.assert_same_bytes(&["info", "t.rrd"]);
    sides.assert_same_bytes(&["lastupdate", "t.rrd"]);
}
