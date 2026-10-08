#![cfg(unix)]
//! rrd_open.c differentials: every command that opens an existing file must
//! fail with RRDtool's text, in rrd_open's order, for files it cannot read.

mod common;

const COMMANDS: &[&[&str]] = &[
    &["info"],
    &["lastupdate"],
    &["dump"],
    &["update", "1000000010:1"],
    &["first"],
    &["last"],
    &["fetch", "AVERAGE", "-s", "1000000000", "-e", "1000000010"],
    &["resize", "0", "GROW", "1"],
];

// B11 and fuzz C5: rrd_open.c:336 (open), 434 (mmap), 465-470 (cookie, float
// cookie, version), 471-497 (header pieces) and 538-541 (file length). The
// i386 seed has its float cookie at offset 12, so it fails on the float
// cookie before its 32-bit counts are read.
#[test]
fn b11_open_failures_use_rrd_open_texts() {
    let Some(sides) = common::Sides::new(
        &[&[
            "create",
            "base.rrd",
            "-b",
            "1000000000",
            "-s",
            "10",
            "DS:x:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:5",
        ]],
        &[],
    ) else {
        return;
    };
    let base = std::fs::read(sides.up.join("base.rrd")).unwrap();
    let mut v6 = base.clone();
    v6[4..8].copy_from_slice(b"0006");
    let mut cookie = base.clone();
    cookie[0..3].copy_from_slice(b"XYZ");
    sides.write("v6.rrd", &v6);
    sides.write("ck.rrd", &cookie);
    sides.write("short.rrd", &base[..600]);
    sides.write("hdr.rrd", &base[..300]);
    sides.write("stat.rrd", &base[..100]);
    sides.write("empty.rrd", b"");
    sides.write(
        "i386.rrd",
        include_bytes!("../../../fuzz/seeds/rrd_file/386_gauge_v3.rrd"),
    );
    for dir in [&sides.up, &sides.ro] {
        std::fs::create_dir(dir.join("dir.rrd")).unwrap();
    }
    for file in [
        "v6.rrd",
        "ck.rrd",
        "short.rrd",
        "hdr.rrd",
        "stat.rrd",
        "empty.rrd",
        "i386.rrd",
        "dir.rrd",
        "missing.rrd",
    ] {
        for command in COMMANDS {
            let mut args = vec![command[0], file];
            args.extend_from_slice(&command[1..]);
            sides.assert_same(&args);
        }
    }
}
