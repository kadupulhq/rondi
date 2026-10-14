#![cfg(unix)]
//! Differentials for rrd_first.c, rrd_last.c and their printing in
//! rrd_tool.c. Option-form cases wait for the shared option parser.

mod common;

const CREATE: &[&str] = &[
    "create",
    "f.rrd",
    "--start",
    "1000000000",
    "--step",
    "10",
    "DS:a:GAUGE:20:U:U",
    "RRA:AVERAGE:0.5:1:6",
    "RRA:MAX:0.5:3:4",
];
const UPDATE: &[&str] = &["update", "f.rrd", "1000000010:1", "1000000050:2"];

// C4: rrd_first.c:33 reads --rraindex with strtol(optarg, &endptr, 0) into
// an int and ignores trailing text; rrd_first.c:84 rejects an index past
// rra_cnt with a bare "invalid rraindex number".
#[test]
fn c4_first_rraindex_uses_strtol_base_zero() {
    let Some(sides) = common::Sides::new(&[CREATE, UPDATE], &[]) else {
        return;
    };
    for index in ["0x1", "010", "1abc", "-1", "2", "4294967297"] {
        sides.assert_same(&["first", "--rraindex", index, "f.rrd"]);
    }
}

// C5: rrd_first and rrd_last return -1 on every error and rrd_tool.c:736-750
// prints it before the ERROR line; the usage texts are rrd_first.c:63 and
// rrd_last.c:44, and rrd_first uses argv[optind] when given more files.
#[test]
fn c5_first_and_last_print_minus_one_on_any_error() {
    let Some(sides) = common::Sides::new(&[CREATE, UPDATE], &[("g.txt", b"garbage\n")]) else {
        return;
    };
    for args in [
        &["first", "--rraindex", "7", "f.rrd"][..],
        &["first", "g.txt"],
        &["first", "missing.rrd"],
        &["first", "--rraindex", "1"],
        &["first", "f.rrd", "f.rrd"],
        &["last", "f.rrd", "f.rrd"],
        &["last", "g.txt"],
        &["last", "missing.rrd"],
        &["last", "--daemon", "unix:/nonexistent"],
    ] {
        sides.assert_same(args);
    }
}
