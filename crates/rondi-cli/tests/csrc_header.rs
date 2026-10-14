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

// rrd_open sizes the archives in wrapping unsigned long arithmetic, so a
// row_cnt of 2^61 + 10 adds the same 80 bytes as 10 and the file opens.
// update then runs into the end of the file in rrd_write.
#[test]
fn huge_row_count_wraps_like_rrd_open() {
    let Some(sides) = fixture() else {
        return;
    };
    patch(&sides, rra_def(3) + 24, &((1_u64 << 61) + 10).to_le_bytes());
    for args in [
        &["info", "t.rrd"][..],
        &["first", "t.rrd"],
        &["last", "t.rrd"],
        &["lastupdate", "t.rrd"],
        &[
            "fetch",
            "t.rrd",
            "AVERAGE",
            "-s",
            "1000000000",
            "-e",
            "1000001200",
        ],
        &["tune", "t.rrd", "-h", "g:900"],
        &["update", "t.rrd", "1000020000:5"],
    ] {
        sides.assert_same_bytes(args);
    }
    sides.assert_same_file("t.rrd");
}

// pdp_cnt * pdp_step wraps too: 2^62 + 2 PDPs of 300 s is 600 s, and
// 2^64 - 1 PDPs is -300 s, which first and dump use as they are.
#[test]
fn huge_pdp_count_wraps_like_rrd_first_and_dump() {
    let Some(sides) = fixture() else {
        return;
    };
    patch(&sides, rra_def(3) + 32, &((1_u64 << 62) + 2).to_le_bytes());
    sides.assert_same(&["dump", "t.rrd"]);
    sides.assert_same(&["first", "t.rrd", "--rraindex", "3"]);
    patch(&sides, rra_def(0) + 32, &u64::MAX.to_le_bytes());
    sides.assert_same(&["first", "t.rrd"]);
}

// Two billion steps ahead, write_to_rras would store a billion rows into the
// damaged archive; rrd_write stops at the end of the file instead, and Rondi
// must stop queuing rows there too rather than allocate for all of them.
#[test]
fn huge_row_count_update_stops_at_end_of_file() {
    let Some(sides) = fixture() else {
        return;
    };
    patch(&sides, rra_def(3) + 24, &((1_u64 << 61) + 10).to_le_bytes());
    sides.assert_same_bytes(&["update", "t.rrd", "600001001200:5"]);
    sides.assert_same_file("t.rrd");
}

// rrd_resize copies once per stored value of the unwrapped row count, so
// Rondi refuses the file (docs/COMPATIBILITY_DEFECTS.md ID-001).
#[test]
fn huge_row_count_resize_is_refused() {
    let Some(sides) = fixture() else {
        return;
    };
    patch(&sides, rra_def(3) + 24, &((1_u64 << 61) + 10).to_le_bytes());
    let output = std::process::Command::new(sides.rondi())
        .args(["resize", "t.rrd", "0", "GROW", "1"])
        .current_dir(&sides.ro)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "ERROR: invalid RRD file: RRA rows extend past the end of the file\n"
    );
    assert!(!sides.ro.join("resize.rrd").exists());
}
