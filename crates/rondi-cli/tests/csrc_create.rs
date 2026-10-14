#![cfg(unix)]
//! rrd_create.c and rrd_open.c differentials. Created files are compared
//! with each archive rotated to logical order and the rra_ptr words zeroed,
//! because rrd_select_initial_row (rrd_open.c:1205) picks a random row.
//! Option-form cases wait for the shared option parser.

mod common;

use std::os::unix::fs::PermissionsExt;

const DS: &str = "DS:x:GAUGE:20:U:U";
const RRA: &str = "RRA:AVERAGE:0.5:1:5";

/// Header with zeroed rra_ptr words, then each archive oldest row first.
/// Assumes the 64-bit little-endian version 3-5 layout.
fn masked(bytes: &[u8]) -> Vec<u8> {
    let word =
        |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
    let (ds, rra) = (word(24), word(32));
    let pointers = 128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra;
    let mut out = bytes[..pointers].to_vec();
    out.extend(std::iter::repeat_n(0, 8 * rra));
    let mut data = pointers + 8 * rra;
    for index in 0..rra {
        let rows = word(128 + 120 * ds + 120 * index + 24);
        let length = rows * ds * 8;
        let archive = &bytes[data..data + length];
        let next = ((word(pointers + 8 * index) + 1) % rows) * ds * 8;
        out.extend_from_slice(&archive[next..]);
        out.extend_from_slice(&archive[..next]);
        data += length;
    }
    out.extend_from_slice(&bytes[data..]);
    out
}

fn assert_same_created(sides: &common::Sides, args: &[&str], name: &str) {
    sides.assert_same(args);
    let up = std::fs::read(sides.up.join(name)).ok();
    let ro = std::fs::read(sides.ro.join(name)).ok();
    assert_eq!(up.is_some(), ro.is_some(), "{args:?}");
    if let (Some(up), Some(ro)) = (up, ro) {
        assert!(masked(&ro) == masked(&up), "{name} differs for {args:?}");
    }
    for dir in [&sides.up, &sides.ro] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

fn create_args<'a>(definitions: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["create", "o.rrd", "-b", "1000000000", "-s", "10"];
    args.extend_from_slice(definitions);
    args
}

// B3, B5: parseDS matches DS_RE and parseGENERIC_DS scans min and max with
// "%18[^:]:%18[^:]", ignoring what follows (rrd_create.c:309-325,
// 1098-1164); parseRRA splits with strtok_r, which skips empty fields, and
// reads the CF with "%19[A-Z]" (rrd_create.c:494-503).
#[test]
fn b3_b5_definition_grammar_matches_parse_ds_and_parse_rra() {
    let Some(sides) = common::Sides::new(&[], &[]) else {
        return;
    };
    for definitions in [
        &["DS:x:GAUGE:20:U:U:extra", RRA][..],
        &["DS:x=y:GAUGE:20:U:U", RRA],
        &["DS:x=y[2]:GAUGE:20:U:U", RRA],
        &["DS:x:GAUGE:20:1234567890123456789:U", RRA],
        &["DS:x:GAUGE:20:1:12345678901234567890", RRA],
        &[DS, "RRA:AVERAGE::0.5:1:5"],
        &[DS, "RRA:AVERAGE:0.5:1:5:"],
        &[DS, "RRA:AVERAGE1:0.5:1:5"],
        &[DS, "RRA:AVERAGE:nan:1:5"],
        &[DS, "RRA:AVERAGE:0.5:1:50s"],
        &["DS:x:DCOUNTER:5m:0:1e9", RRA],
        &["DS:x:GAUGE:20:nan:U", RRA],
    ] {
        assert_same_created(&sides, &create_args(definitions), "o.rrd");
    }
}

// B4: rrd_set_error texts from parseDS, parseGENERIC_DS (rrd_create.c:368-424,
// 1098-1164), dst_conv/rrd_cf_conv (rrd_format.c), parseRRA (480-760) and
// rrd_create_r2 (942-1031), including the later error replacing the first.
#[test]
fn b4_definition_errors_use_rrd_create_texts() {
    let Some(sides) = common::Sides::new(&[], &[]) else {
        return;
    };
    for definitions in [
        &["DS:x:GAUGE:20:U", RRA][..],
        &["DS:x:GAUGE:20", RRA],
        &["DS:x:BOGUS:20:U:U", RRA],
        &["DS:x.y:GAUGE:20:U:U", RRA],
        &["DS:x:GAUGE:0:U:U", RRA],
        &["DS:x:GAUGE:abc:U:U", RRA],
        &["DS:x:GAUGE:123456789012345678901234567890123:U:U", RRA],
        &["DS:x:GAUGE:20:abc:U", RRA],
        &["DS:x:GAUGE:20:1x:U", RRA],
        &["DS:x:GAUGE:20:5:x", RRA],
        &["DS:x:GAUGE:20:5:1", RRA],
        &[DS, "DS:x:GAUGE:20:U:U", RRA],
        &[DS, "RRA:AVERAGE:0.5:1:5:9"],
        &[DS, "RRA:AVERAGE:0.5:1"],
        &[DS, "RRA:LAST:0.5"],
        &[DS, "RRA::::"],
        &[DS, "RRA:AVERAGE:1:1:5"],
        &[DS, "RRA:AVERAGE:-0.1:1:5"],
        &[DS, "RRA:FOO:0.5:1:5"],
        &[DS, "RRA:average:0.5:1:5"],
        &[DS, "RRA:AVERAGE:0.5:7s:5"],
        &[DS, "RRA:AVERAGE:0.5:0:5"],
        &[DS, "RRA:AVERAGE:0.5:1:0"],
        &[DS, "RRA:AVERAGE:0.5:1:5s"],
        &["RRA:FOO:0.5:1:5", DS],
        &[DS],
        &[RRA],
        &[DS, "foo", RRA],
    ] {
        assert_same_created(&sides, &create_args(definitions), "o.rrd");
    }
    for args in [
        &["create", "o.rrd", "-t", "a.rrd", "-t", "b.rrd", DS, RRA][..],
        &["create", "o.rrd", "--step", "0", DS, RRA],
        &["create", "o.rrd", "--step", "10x", DS, RRA],
        &["create", "o.rrd", "-b", "end-1h", DS, RRA],
        &["create", "o.rrd", "-b", "315360000", DS, RRA],
        &["create", "o.rrd", "-t", "missing.rrd"],
        &["create", "o.rrd", "-r", "missing.rrd", DS, RRA],
        &["create", "--step", "10"],
    ] {
        assert_same_created(&sides, args, "o.rrd");
    }
}

// B6: xff goes through rrd_strtodbl (rrd_create.c:575-580), not a correctly
// rounded parser.
#[test]
fn b6_xff_bits_match_rrd_strtod() {
    let Some(sides) = common::Sides::new(&[], &[]) else {
        return;
    };
    for xff in ["0.123456789012345678", "0.11111111111111111111111"] {
        let rra = format!("RRA:AVERAGE:{xff}:1:5");
        assert_same_created(&sides, &create_args(&[DS, &rra]), "o.rrd");
    }
}

// B7: write_rrd (rrd_create.c:1406-1491) writes "-" to stdout, creates
// `<name>XXXXXX` beside the target without creating directories, and chmods
// a new file to 0644 whatever the umask.
#[test]
fn b7_write_rrd_filesystem_behavior_matches_upstream() {
    let Some(sides) = common::Sides::new(&[], &[]) else {
        return;
    };
    let args = create_args(&[DS, RRA]);
    for mask in ["077", "000"] {
        let mut modes = Vec::new();
        for (program, dir) in [
            ("rrdtool".to_owned(), &sides.up),
            (sides.rondi().display().to_string(), &sides.ro),
        ] {
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("umask {mask}; exec \"$0\" \"$@\""))
                .arg(program)
                .args(&args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success());
            modes.push(
                std::fs::metadata(dir.join("o.rrd"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
            );
            std::fs::remove_file(dir.join("o.rrd")).unwrap();
        }
        assert_eq!(modes[1], modes[0], "umask {mask}");
    }
    let mut nested = args.clone();
    nested[1] = "sub/dir/o.rrd";
    sides.assert_same(&nested);
    let mut piped = args.clone();
    piped[1] = "-";
    let [up, ro] = [std::path::Path::new("rrdtool"), sides.rondi()].map(|program| {
        std::process::Command::new(program)
            .args(&piped)
            .current_dir(&sides.up)
            .output()
            .unwrap()
    });
    assert_eq!(
        (ro.status.code(), &ro.stderr),
        (up.status.code(), &up.stderr)
    );
    assert!(masked(&ro.stdout) == masked(&up.stdout));
    assert!(!sides.up.join("-").exists());
}

// B8: --no-overwrite has rrd_create_r2's text, no I/O prefix
// (rrd_create.c:856-859).
#[test]
fn b8_no_overwrite_text_matches_upstream() {
    let Some(sides) = common::Sides::new(&[], &[("o.rrd", b"x")]) else {
        return;
    };
    let mut args = create_args(&[DS, RRA]);
    args.push("-O");
    sides.assert_same(&args);
    sides.assert_same_file("o.rrd");
}

// B9: --template memcpy()s ds_def and rra_def and keeps version 0003 unless
// a command-line DS needs 0005 (rrd_create.c:881-919, 1028-1031).
#[test]
fn b9_template_copies_definitions_verbatim() {
    let Some(sides) = common::Sides::new(
        &[&[
            "create",
            "tpl.rrd",
            "-b",
            "1000000123",
            "-s",
            "60",
            "DS:a:GAUGE:120:nan:U",
            "DS:b:DCOUNTER:5m:0:1e9",
            "RRA:AVERAGE:0.5:1:10",
        ]],
        &[],
    ) else {
        return;
    };
    for args in [
        &["create", "o.rrd", "-t", "tpl.rrd"][..],
        &[
            "create",
            "o.rrd",
            "-t",
            "tpl.rrd",
            "-s",
            "30",
            "DS:c:GAUGE:60:U:U",
        ],
        &[
            "create",
            "o.rrd",
            "-t",
            "tpl.rrd",
            "-b",
            "1000000000",
            "RRA:MAX:0.5:2:3",
        ],
        &["create", "o.rrd", "-t", "tpl.rrd", "DS:a:GAUGE:60:U:U"],
        &["create", "o.rrd", "-t", "tpl.rrd", "DS:c:DDERIVE:60:U:U"],
    ] {
        assert_same_created(&sides, args, "o.rrd");
    }
}

// B10: rrd_open (rrd_open.c:465-560) checks nothing beyond the cookies,
// version and lengths, so files with a NaN xff, cur_row past row_cnt, an
// empty DS name or a version 0001/0002 header (whose live head is a time_t)
// open, read and update. Zero steps are covered separately: RRDtool divides
// by them.
#[test]
fn b10_rrd_open_accepts_what_it_does_not_check() {
    let Some(sides) = common::Sides::new(
        &[
            &[
                "create",
                "base.rrd",
                "-b",
                "1000000000",
                "-s",
                "10",
                "DS:x:GAUGE:20:U:U",
                "DS:y:COUNTER:20:U:U",
                "RRA:AVERAGE:0.5:1:5",
                "RRA:MAX:0.5:2:4",
            ],
            &[
                "update",
                "base.rrd",
                "1000000010:1:100",
                "1000000020:2:200",
                "1000000030:3:300",
            ],
            &[
                "create",
                "nanxff.rrd",
                "-b",
                "1000000000",
                "-s",
                "10",
                "DS:x:GAUGE:20:U:U",
                "DS:y:COUNTER:20:U:U",
                "RRA:AVERAGE:nan:1:5",
            ],
            &[
                "update",
                "nanxff.rrd",
                "1000000010:1:100",
                "1000000020:2:200",
            ],
        ],
        &[],
    ) else {
        return;
    };
    let base = std::fs::read(sides.up.join("base.rrd")).unwrap();
    let live = 128 + 2 * 120 + 2 * 120;
    let pointer = live + 16 + 2 * 112 + 4 * 80;
    for version in ["0001", "0002"] {
        let mut legacy = base[..4].to_vec();
        legacy.extend_from_slice(version.as_bytes());
        legacy.extend_from_slice(&base[8..live + 8]);
        legacy.extend_from_slice(&base[live + 16..]);
        sides.write(&format!("v{version}.rrd"), &legacy);
    }
    let mut row = base.clone();
    row[pointer..pointer + 8].copy_from_slice(&7_u64.to_le_bytes());
    sides.write("currow.rrd", &row);
    let mut name = base.clone();
    name[128..148].fill(0);
    sides.write("emptyname.rrd", &name);
    for file in [
        "v0001.rrd",
        "v0002.rrd",
        "nanxff.rrd",
        "currow.rrd",
        "emptyname.rrd",
    ] {
        for args in [
            &["info", file][..],
            &["lastupdate", file],
            &["dump", file],
            &[
                "fetch",
                file,
                "AVERAGE",
                "-s",
                "1000000000",
                "-e",
                "1000000040",
            ],
            &["first", file],
            &["last", file],
            &["update", file, "1000000040:4:400", "1000000055:5:500"],
            &["tune", file, "-h", "y:30"],
        ] {
            sides.assert_same(args);
            sides.assert_same_file(file);
        }
    }
}

// B10, zero step: rrd_open opens it, then RRDtool divides by the step, which
// traps on x86_64 and reads garbage on aarch64. Rondi opens it for `info`,
// `lastupdate` and `last`, and refuses the dividing commands with an error.
#[test]
fn b10_zero_step_opens_and_never_divides() {
    let Some(sides) = common::Sides::new(
        &[&[
            "create",
            "base.rrd",
            "-b",
            "1000000000",
            "-s",
            "10",
            DS,
            RRA,
        ]],
        &[],
    ) else {
        return;
    };
    let mut zero = std::fs::read(sides.up.join("base.rrd")).unwrap();
    zero[40..48].fill(0);
    sides.write("step0.rrd", &zero);
    for args in [
        &["info", "step0.rrd"][..],
        &["lastupdate", "step0.rrd"],
        &["last", "step0.rrd"],
    ] {
        sides.assert_same(args);
    }
    for args in [
        &["dump", "step0.rrd"][..],
        &["first", "step0.rrd"],
        &[
            "fetch",
            "step0.rrd",
            "AVERAGE",
            "-s",
            "1000000000",
            "-e",
            "1000000040",
        ],
        &["update", "step0.rrd", "1000000040:4"],
    ] {
        let (_, ro) = sides.both(args, &[]);
        assert_eq!(
            (ro.status, ro.stderr.as_str()),
            (
                Some(1),
                "ERROR: unsupported RRD operation: RRD step, pdp_cnt or row_cnt is zero\n"
            ),
            "{args:?}"
        );
    }
    assert!(std::fs::read(sides.ro.join("step0.rrd")).unwrap() == zero);
}
