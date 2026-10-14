#![cfg(unix)]
//! rrd_modify.c differentials: `rrdtool tune` with DEL:, DS:, DELRRA:,
//! RRA#n:[+-=]rows, RRA: and --step. Each fixture is created and updated by
//! RRDtool once and copied to both sides, so both start from the same
//! randomly chosen rows. Results are compared byte for byte, except the
//! parts of an added DS definition that parseDS leaves uninitialized
//! (rrd_create.c:328 never clears dst[19] or par[3..9]).

mod common;

use std::os::unix::fs::PermissionsExt;

fn word(bytes: &[u8], offset: usize) -> usize {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
}

fn masked(bytes: &[u8], added: &[&str]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if out.len() < 128 {
        return out;
    }
    for index in 0..word(bytes, 24) {
        let offset = 128 + 120 * index;
        let field = &bytes[offset..offset + 20];
        let name = &field[..field.iter().position(|b| *b == 0).unwrap_or(20)];
        if added.iter().any(|added| added.as_bytes() == name) {
            out[offset + 39] = 0;
            out[offset + 64..offset + 120].fill(0);
        }
    }
    out
}

/// Names of the data sources `args` adds.
fn added_names<'a>(args: &[&'a str]) -> Vec<&'a str> {
    args.iter()
        .filter_map(|arg| arg.strip_prefix("DS:"))
        .map(|def| def.split([':', '=']).next().unwrap())
        .collect()
}

/// Runs `tune <copy of base> args` on both sides and compares the outcome,
/// the files and, with `inspect`, what `info` and `dump` print for them.
fn assert_same_tune(sides: &common::Sides, base: &str, case: usize, args: &[&str], inspect: bool) {
    let name = format!("case{case}.rrd");
    for dir in [&sides.up, &sides.ro] {
        std::fs::copy(dir.join(base), dir.join(&name)).unwrap();
    }
    let mut command = vec!["tune", name.as_str()];
    command.extend_from_slice(args);
    sides.assert_same(&command);
    let up = std::fs::read(sides.up.join(&name)).unwrap();
    let ro = std::fs::read(sides.ro.join(&name)).unwrap();
    let added = added_names(args);
    assert!(
        masked(&ro, &added) == masked(&up, &added),
        "{name} differs for {args:?} (upstream {} bytes, rondi {} bytes)",
        up.len(),
        ro.len()
    );
    if inspect {
        sides.assert_same(&["info", &name]);
        sides.assert_same(&["dump", &name]);
    }
}

fn borrow(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

fn update_args(file: &str, samples: impl Iterator<Item = String>) -> Vec<String> {
    let mut args = vec!["update".to_owned(), file.to_owned()];
    args.extend(samples);
    args
}

fn fixtures() -> Option<common::Sides> {
    let base = [
        "create",
        "base.rrd",
        "--start",
        "1000000000",
        "--step",
        "300",
        "DS:g:GAUGE:600:U:U",
        "DS:c:COUNTER:600:0:U",
        "RRA:AVERAGE:0.5:1:10",
        "RRA:MAX:0.5:3:8",
        "RRA:LAST:0.5:1:5",
        "RRA:MIN:0.5:2:6",
    ];
    let mut counter = 0;
    let base_updates = update_args(
        "base.rrd",
        (1..=40)
            .map(|i| {
                counter += 7 * i;
                format!("{}:{}.5:{counter}", 1_000_000_000 + 300 * i, i % 9)
            })
            .chain(std::iter::once("1000012170:3:99999".to_owned())),
    );
    let types = [
        "create",
        "types.rrd",
        "--start",
        "1000000000",
        "--step",
        "60",
        "DS:a:DERIVE:120:U:U",
        "DS:b:ABSOLUTE:120:0:1000",
        "DS:d:DCOUNTER:120:U:U",
        "DS:e:DDERIVE:120:U:U",
        "DS:g:GAUGE:120:-5:100",
        "RRA:AVERAGE:0.5:1:12",
        "RRA:AVERAGE:0.3:2:12",
        "RRA:MIN:0.5:2:12",
        "RRA:MAX:0.5:2:12",
        "RRA:LAST:0.5:1:12",
        "RRA:AVERAGE:0.5:3:7",
        "RRA:MIN:0.5:1:9",
        "RRA:MAX:0.5:4:5",
        "RRA:LAST:0.9:3:6",
    ];
    let types_updates = update_args(
        "types.rrd",
        (1..=50)
            .map(|i| {
                let t = 1_000_000_000 + 60 * i;
                if i % 7 == 3 {
                    format!("{t}:U:U:U:U:U")
                } else {
                    format!("{t}:{}:{}:{}.25:{}:{}", i * i, i * 3, i * 5, i * 2, i % 11)
                }
            })
            .chain(std::iter::once("1000003037:3000:20:400:120:4".to_owned())),
    );
    // Ten archives, eight of them AVERAGE with tied pdp and row counts, so
    // the qsort in find_candidate_rras has to order equal candidates.
    let ties = [
        "create",
        "ties.rrd",
        "--start",
        "1000000003",
        "--step",
        "10",
        "DS:a:GAUGE:15:U:U",
        "DS:b:COUNTER:15:U:U",
        "RRA:AVERAGE:0.5:1:10",
        "RRA:AVERAGE:0.1:1:10",
        "RRA:AVERAGE:0.9:2:10",
        "RRA:AVERAGE:0.1:2:10",
        "RRA:AVERAGE:0.5:1:5",
        "RRA:AVERAGE:0.2:3:4",
        "RRA:AVERAGE:0.8:3:4",
        "RRA:AVERAGE:0.99:1:10",
        "RRA:MAX:0.5:1:10",
        "RRA:MAX:0.1:1:10",
    ];
    let mut t = 1_000_000_003;
    let ties_updates = update_args(
        "ties.rrd",
        (1..=45).map(|i| {
            t += 7 + (i % 5) * 3;
            if i % 4 == 0 {
                format!("{t}:U:{}", i * 13)
            } else {
                format!("{t}:{}.5:{}", i % 13, i * 13)
            }
        }),
    );
    let fresh = [
        "create",
        "fresh.rrd",
        "--start",
        "1000000007",
        "--step",
        "10",
        "DS:x:GAUGE:20:U:U",
        "RRA:AVERAGE:0.5:1:4",
        "RRA:MAX:0.5:2:3",
    ];
    let sparse = [
        "create",
        "sparse.rrd",
        "--start",
        "1000000000",
        "--step",
        "300",
        "DS:in:COUNTER:600:U:U",
        "DS:out:GAUGE:600:U:U",
        "RRA:AVERAGE:0.5:1:20",
        "RRA:AVERAGE:0.5:6:10",
        "RRA:LAST:0.5:12:4",
    ];
    let sparse_updates = [
        "update",
        "sparse.rrd",
        "1000000300:100:1",
        "1000000600:400:2",
        "1000000900:900:U",
        "1000003000:1200:7",
    ];
    let base_updates = borrow(&base_updates);
    let types_updates = borrow(&types_updates);
    let ties_updates = borrow(&ties_updates);
    common::Sides::new(
        &[
            &base,
            &base_updates,
            &types,
            &types_updates,
            &ties,
            &ties_updates,
            &fresh,
            &sparse,
            &sparse_updates,
        ],
        &[],
    )
}

// handle_rra_defs, mod_rras, add_dss and add_rras (rrd_modify.c:202-384,
// 563-697, 1171-1297): rows come out oldest first with cur_row at the end,
// grown rows are back-filled from other archives by populate_row, and the
// CDP of an added archive is rebuilt by prepare_CDPs.
#[test]
fn tune_modify_rewrites_files_like_rrd_modify() {
    let Some(sides) = fixtures() else {
        return;
    };
    let cases: &[(&str, &[&str])] = &[
        ("base.rrd", &["DS:new:GAUGE:600:U:U"]),
        ("base.rrd", &["DEL:g"]),
        ("base.rrd", &["DEL:c", "DEL:g"]),
        ("base.rrd", &["DEL:nonexistent"]),
        ("base.rrd", &["DELRRA:1"]),
        ("base.rrd", &["DELRRA:9"]),
        ("base.rrd", &["RRA:AVERAGE:0.5:6:10"]),
        ("base.rrd", &["RRA#0:+5"]),
        ("base.rrd", &["RRA#0:-5"]),
        ("base.rrd", &["RRA#0:=30"]),
        ("base.rrd", &["RRA#0:-11"]),
        ("base.rrd", &["RRA#9:+3"]),
        ("base.rrd", &["DELRRA:0", "RRA#1:+4"]),
        ("base.rrd", &["DS:g2:GAUGE:600:U:U", "DEL:g", "RRA#3:+5"]),
        ("types.rrd", &["DEL:a", "DEL:b", "DEL:d", "DEL:e"]),
        ("types.rrd", &["RRA:MIN:0.5:2:20"]),
        ("types.rrd", &["RRA:MAX:0.5:1:30"]),
        ("types.rrd", &["RRA:LAST:0.5:5:8"]),
        ("types.rrd", &["RRA#2:+7"]),
        ("types.rrd", &["RRA#3:+4"]),
        ("types.rrd", &["RRA#4:+9"]),
        ("types.rrd", &["RRA#6:=2"]),
        (
            "types.rrd",
            &[
                "DS:q:DDERIVE:300:U:U",
                "DEL:e",
                "RRA#8:-2",
                "RRA:LAST:0.5:1:5",
            ],
        ),
        (
            "types.rrd",
            &["DS:n:DCOUNTER:300:0:U", "RRA#1:+5", "RRA:AVERAGE:0.5:2:30"],
        ),
        ("types.rrd", &["RRA:AVERAGE:0.5:1h:1d"]),
        ("types.rrd", &["--heartbeat", "g:300", "DEL:a"]),
        ("ties.rrd", &["RRA:AVERAGE:0.5:2:20"]),
        ("ties.rrd", &["RRA#1:+10"]),
        ("ties.rrd", &["RRA#5:+10"]),
        ("ties.rrd", &["RRA#9:+3"]),
        ("ties.rrd", &["DELRRA:2", "RRA#5:+3"]),
        (
            "ties.rrd",
            &["DS:c:GAUGE:15:U:U", "RRA:AVERAGE:0.5:2:20", "RRA#3:+6"],
        ),
        ("fresh.rrd", &["RRA#0:+5", "RRA:LAST:0.5:1:40"]),
        ("fresh.rrd", &["DS:new:DCOUNTER:600:U:U"]),
        ("sparse.rrd", &["RRA#1:+5", "RRA:AVERAGE:0.5:6:10"]),
        ("sparse.rrd", &["DEL:in", "RRA#2:=1"]),
    ];
    for (case, (base, args)) in cases.iter().enumerate() {
        assert_same_tune(&sides, base, case, args, true);
    }
}

// rrd_modify_r2 (rrd_modify.c:936-1005): --step stretches every archive
// before the other changes, and must divide the old step.
#[test]
fn tune_modify_step_stretches_like_rrd_modify() {
    let Some(sides) = fixtures() else {
        return;
    };
    let cases: &[(&str, &[&str])] = &[
        ("types.rrd", &["--step", "30", "RRA#0:+3"]),
        ("types.rrd", &["--step", "20", "DEL:a"]),
        ("types.rrd", &["--step", "7", "DEL:a"]),
        ("types.rrd", &["--step", "60", "DEL:a"]),
        ("types.rrd", &["--step", "30"]),
        (
            "types.rrd",
            &["-t", "15", "RRA:AVERAGE:0.5:2:10", "DS:k:GAUGE:120:U:U"],
        ),
        ("ties.rrd", &["--step", "5", "RRA:AVERAGE:0.5:2:20"]),
    ];
    for (case, (base, args)) in cases.iter().enumerate() {
        assert_same_tune(&sides, base, 100 + case, args, true);
    }
}

// handle_modify parses every word before changing anything, and
// add_dss refuses a name that is already taken (rrd_modify.c:347-351,
// 1299-1433). The archive-less result matches too, though RRDtool's own
// info and dump abort on it, so only the bytes are compared there.
#[test]
fn tune_modify_errors_match_rrd_modify() {
    let Some(sides) = fixtures() else {
        return;
    };
    let cases: &[&[&str]] = &[
        &["DS:g=newg:GAUGE:600:U:U"],
        &["DS:new:GAUGE:600:U:U", "DS:new:GAUGE:600:U:U"],
        &["RRA#0:*3"],
        &["RRA#x"],
        &["RRA#"],
        &["FOO"],
        &["DS:"],
        &["DELRRA:-1"],
        &["RRA:FOO:0.5:1:3"],
        &["RRA:AVERAGE:1.5:1:5"],
        &["RRA:AVERAGE:0.5:1"],
        &["DS:m:FOO:600:U:U", "RRA:AVERAGE:0.5:1:3"],
        &["DS:m:FOO:600:U:U", "RRA:BOGUS:0.5:1:3"],
        &["--step", "7", "RRA#0:+1"],
        &["DEL:g", "FOO"],
    ];
    for (case, args) in cases.iter().enumerate() {
        assert_same_tune(&sides, "base.rrd", 200 + case, args, true);
    }
    assert_same_tune(&sides, "fresh.rrd", 299, &["DELRRA:0", "DELRRA:1"], false);
}

// A DS definition that fails to parse does not stop add_dss: upstream
// writes the definition's uninitialized bytes and then reports the error.
// Rondi reports the same error and leaves the file as it was. COMPUTE
// data sources and Holt-Winters archives stay refused, as in create.
#[test]
fn tune_modify_refuses_without_writing_where_upstream_writes_garbage() {
    let Some(sides) = fixtures() else {
        return;
    };
    for (args, refusal) in [
        (&["DS:bad"][..], None),
        (&["DS:9bad:GAUGE:x:U:U"], None),
        (&["DS:z:GAUGE:600:5:1"], None),
        (
            &["DS:y:COMPUTE:g,2,*"],
            Some("ERROR: COMPUTE data sources are not supported by Rondi\n"),
        ),
        (
            &["RRA:HWPREDICT:100:0.1:0.0035:10"],
            Some(
                "ERROR: Holt-Winters consolidation function HWPREDICT is not supported by Rondi\n",
            ),
        ),
    ] {
        for dir in [&sides.up, &sides.ro] {
            std::fs::copy(dir.join("base.rrd"), dir.join("refused.rrd")).unwrap();
        }
        let mut command = vec!["tune", "refused.rrd"];
        command.extend_from_slice(args);
        let (up, ro) = sides.both(&command, &[]);
        match refusal {
            None => {
                assert_eq!(ro, up, "{args:?}");
                assert_ne!(up.status, Some(0), "{args:?}");
            }
            Some(message) => {
                assert_eq!(up.status, Some(0), "{args:?}: {up:?}");
                assert_eq!(ro.stderr, message, "{args:?}");
                assert_eq!(ro.status, Some(1), "{args:?}");
            }
        }
        assert!(
            std::fs::read(sides.up.join("refused.rrd")).unwrap()
                != std::fs::read(sides.up.join("base.rrd")).unwrap(),
            "upstream left the file alone for {args:?}"
        );
        assert!(
            std::fs::read(sides.ro.join("refused.rrd")).unwrap()
                == std::fs::read(sides.ro.join("base.rrd")).unwrap(),
            "rondi changed the file for {args:?}"
        );
    }
}

// write_rrd (rrd_create.c:1406) writes a temporary file, gives it the old
// file's mode and renames it over the old name.
#[test]
fn tune_modify_keeps_the_file_mode_and_replaces_the_inode() {
    let Some(sides) = fixtures() else {
        return;
    };
    for dir in [&sides.up, &sides.ro] {
        std::fs::set_permissions(dir.join("base.rrd"), std::fs::Permissions::from_mode(0o640))
            .unwrap();
        std::fs::hard_link(dir.join("base.rrd"), dir.join("link.rrd")).unwrap();
    }
    sides.assert_same(&["tune", "base.rrd", "DEL:g"]);
    sides.assert_same_file("base.rrd");
    sides.assert_same_file("link.rrd");
    for dir in [&sides.up, &sides.ro] {
        let mode = std::fs::metadata(dir.join("base.rrd"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_ne!(
            std::fs::read(dir.join("base.rrd")).unwrap(),
            std::fs::read(dir.join("link.rrd")).unwrap()
        );
    }
}
