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

/// Restored files carry a random rra_ptr (rrd_restore.c:450); compare
/// them in logical row order.
fn assert_same_restored(sides: &common::Sides, name: &str) {
    let up = std::fs::read(sides.up.join(name)).ok();
    let ro = std::fs::read(sides.ro.join(name)).ok();
    assert_eq!(ro.is_some(), up.is_some(), "{name} exists on one side only");
    if let (Some(up), Some(ro)) = (up, ro) {
        assert!(
            common::masked_rrd(&ro) == common::masked_rrd(&up),
            "{name} differs"
        );
    }
}

/// libxml2's reader reports the parser's line, which runs ahead of the node
/// by a buffer-dependent amount; Rondi approximates it.
fn without_line_numbers(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("lin") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let word = if tail.starts_with("line ") {
            "line "
        } else if tail.starts_with("ling ") {
            "ling "
        } else {
            out.push_str("lin");
            rest = &tail[3..];
            continue;
        };
        out.push_str(word);
        out.push('N');
        rest = tail[word.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

const SOURCE: &[&[&str]] = &[
    &[
        "create",
        "s.rrd",
        "--start",
        "1000000000",
        "--step",
        "10",
        "DS:a:GAUGE:20:0:100",
        "DS:b:COUNTER:20:U:U",
        "RRA:AVERAGE:0.5:1:5",
        "RRA:MAX:0.5:2:4",
    ],
    &[
        "update",
        "s.rrd",
        "1000000010:5:100",
        "1000000020:50:300",
        "1000000030:7:310",
    ],
    &["dump", "s.rrd", "s.xml"],
];

// D1: rrd_resize opens resize.rrd through rrd_open with RRD_CREAT, so an
// existing file is truncated and replaced (rrd_resize.c:105,
// rrd_open.c:284-293); the copy follows rrd_resize.c:183-298, including the
// warning for a version 0001 file whose output is sized for a time_t live
// head and the zero-filled output left behind by an unsupported version.
#[test]
fn d1_resize_writes_resize_rrd_like_rrd_resize() {
    let Some(sides) = common::Sides::new(SOURCE, &[("resize.rrd", b"stale\n")]) else {
        return;
    };
    let base = std::fs::read(sides.up.join("s.rrd")).unwrap();
    let live = 128 + 2 * 120 + 2 * 120;
    let mut legacy = base[..4].to_vec();
    legacy.extend_from_slice(b"0001");
    legacy.extend_from_slice(&base[8..live + 8]);
    legacy.extend_from_slice(&base[live + 16..]);
    sides.write("v1.rrd", &legacy);
    let mut v5 = base.clone();
    v5[4..8].copy_from_slice(b"0005");
    sides.write("v5.rrd", &v5);
    for args in [
        &["resize", "s.rrd", "0", "GROW", "2"][..],
        &["resize", "s.rrd", "0", "SHRINK", "4"],
        &["resize", "s.rrd", "1", "SHRINK", "2"],
        &["resize", "s.rrd", "1", "GROW", "3"],
        &["resize", "v1.rrd", "0", "GROW", "1"],
        &["resize", "v1.rrd", "1", "SHRINK", "1"],
        &["resize", "v5.rrd", "0", "GROW", "1"],
    ] {
        sides.write("resize.rrd", b"stale\n");
        sides.assert_same(args);
        sides.assert_same_file("resize.rrd");
    }
}

// D7: restore takes -r and -f (rrd_restore.c:1384-1385).
#[test]
fn d7_restore_accepts_short_options() {
    let Some(sides) = common::Sides::new(SOURCE, &[]) else {
        return;
    };
    sides.assert_same(&["restore", "-r", "s.xml", "r.rrd"]);
    assert_same_restored(&sides, "r.rrd");
    sides.assert_same(&["restore", "-f", "s.xml", "s.rrd"]);
    assert_same_restored(&sides, "s.rrd");
}

// D9: get_xml_double (rrd_restore.c:296-339) converts with rrd_strtodbl,
// whose result can differ from correctly rounded parsing in the last bits.
#[test]
fn d9_restore_numbers_use_rrd_strtod() {
    let rows = [
        "1.4507475627e-193",
        "8.2380098498e+188",
        "6.0412473305e-118",
        "-3.2195486218e+05",
        "0.5961240497818927871",
        "2.4791866429e+250",
    ]
    .iter()
    .map(|v| format!("<row><v>{v}</v></row>"))
    .collect::<String>();
    let xml = format!(
        "<rrd><version>0003</version><step>1</step><lastupdate>1000000300</lastupdate>\
         <ds><name>a</name><type>GAUGE</type><minimal_heartbeat>20</minimal_heartbeat>\
         <min>NaN</min><max>NaN</max><last_ds>U</last_ds><value>0.0</value>\
         <unknown_sec>0</unknown_sec></ds><rra><cf>LAST</cf><pdp_per_row>1</pdp_per_row>\
         <params><xff>0.5</xff></params><cdp_prep><ds><primary_value>0.1234567890123456789</primary_value>\
         <secondary_value>0</secondary_value><value>NaN</value>\
         <unknown_datapoints>0</unknown_datapoints></ds></cdp_prep><database>{rows}</database></rra></rrd>"
    );
    let Some(sides) = common::Sides::new(&[], &[("in.xml", xml.as_bytes())]) else {
        return;
    };
    sides.assert_same(&["restore", "in.xml", "out.rrd"]);
    assert_same_restored(&sides, "out.rrd");
}

// D10: write_file writes the target "-" to stdout (rrd_restore.c:1334).
#[test]
fn d10_restore_dash_target_writes_stdout() {
    let Some(sides) = common::Sides::new(SOURCE, &[]) else {
        return;
    };
    let [up, ro] = [std::path::Path::new("rrdtool"), sides.rondi()].map(|program| {
        std::process::Command::new(program)
            .args(["restore", "s.xml", "-"])
            .current_dir(&sides.up)
            .output()
            .unwrap()
    });
    assert_eq!(
        (ro.status.code(), &ro.stderr),
        (up.status.code(), &up.stderr)
    );
    assert!(common::masked_rrd(&ro.stdout) == common::masked_rrd(&up.stdout));
}

// D11 and fuzz C6: the pull parser matches tags case-insensitively
// (xmlStrcasecmp), keeps the first white-space-delimited word of a text node
// (get_xml_text), treats any text containing "nan" or "inf" as special,
// reads integers with strtoul/strtoll base 0 (get_xml_ulong, get_xml_time_t),
// leaves missing fields zero, accepts any version and rejects unknown tags,
// each with rrd_restore.c's text (rrd_restore.c:183-350, 1049-1157).
#[test]
fn d11_restore_xml_reading_matches_the_pull_parser() {
    let Some(sides) = common::Sides::new(SOURCE, &[]) else {
        return;
    };
    let xml = std::fs::read_to_string(sides.up.join("s.xml")).unwrap();
    let variants = [
        xml.replace("<v>7.0000000000e+00</v>", "<v>7.0000000000e+00 junk</v>"),
        xml.replace("<cf>AVERAGE</cf>", "<CF>AVERAGE</CF>"),
        xml.replacen("<v>NaN</v>", "<v>banana</v>", 1),
        xml.replacen("<v>NaN</v>", "<v>-Infinity</v>", 1),
        xml.replace("<step>10</step>", "<step>0x0a</step>"),
        xml.replace("<step>10</step>", "<step>012</step>"),
        xml.replace("<step>10</step>", "<step>-10</step>"),
        xml.replace("<lastupdate>", "<lastupdate>0x")
            .replacen("0x1000000030", "0x3b9aca1e", 1),
        xml.replace("<step>10</step>", "<step>99999999999999999999999</step>"),
        xml.lines()
            .filter(|line| !line.contains("<unknown_sec>"))
            .collect::<Vec<_>>()
            .join("\n"),
        xml.replace("<min>0.0000000000e+00</min>", ""),
        xml.replace("<version>0003</version>", "<version>0004</version>"),
        xml.replace("<version>0003</version>", "<version>00055</version>"),
        xml.replace("<version>0003</version>", ""),
        xml.replace(
            "<unknown_sec> 0 </unknown_sec>",
            "<unknown_sec> 0 </unknown_sec><bogus>1</bogus>",
        ),
        xml.replacen("<xff>5.0000000000e-01</xff>", "", 1),
        xml.replacen(
            "<xff>5.0000000000e-01</xff>",
            "<xff>5.0000000000e-01</xff><bogus>1</bogus>",
            1,
        ),
        xml.replacen("<xff>5.0000000000e-01</xff>", "<xff>q</xff>", 1),
        xml.replacen("<v>NaN</v>", "<v>abc</v>", 1),
        xml.replacen("<v>NaN</v>", "<v>12abc</v>", 1),
        xml.replacen("<v>NaN</v>", "<v/>", 1),
        xml.replacen("<v>NaN</v>", "<v>1<!--c-->2</v>", 1),
        xml.replace("<cf>AVERAGE</cf>", "<cf><![CDATA[x]]>AVERAGE</cf>"),
        xml.replace("<name> a </name>", "<name> abcdefghijklmnopqrstu </name>"),
        xml.replace("<cf>AVERAGE</cf>", "<cf>AVG</cf>"),
        xml.replace("<type> GAUGE </type>", "<type> GAUGES </type>"),
        xml.replacen("</rra>", "</rra><ds><name>c</name></ds>", 1),
        xml.replace("<rrd>", "<rrdx>").replace("</rrd>", "</rrdx>"),
        xml.replacen("</row>", "</row><bogus>1</bogus>", 1),
    ];
    for (index, variant) in variants.iter().enumerate() {
        let name = format!("v{index}.xml");
        sides.write(&name, variant.as_bytes());
        let target = format!("v{index}.rrd");
        let (up, ro) = sides.both(&["restore", &name, &target], &[]);
        assert_eq!(
            (ro.status, without_line_numbers(&ro.stderr)),
            (up.status, without_line_numbers(&up.stderr)),
            "{name}"
        );
        assert_same_restored(&sides, &target);
    }
    sides.assert_same(&["restore", "missing.xml", "m.rrd"]);
}

// write_file (rrd_restore.c:1318-1336) opens the target O_WRONLY|O_CREAT,
// adding O_EXCL without -f, with mode 0666, and writes it in place.
#[test]
fn restore_writes_the_target_in_place() {
    use std::os::unix::fs::MetadataExt;
    let Some(sides) = common::Sides::new(SOURCE, &[]) else {
        return;
    };
    sides.assert_same(&["restore", "s.xml", "s.rrd"]);
    for dir in [&sides.up, &sides.ro] {
        std::fs::copy(dir.join("s.rrd"), dir.join("t.rrd")).unwrap();
    }
    let inode = |dir: &std::path::Path| std::fs::metadata(dir.join("t.rrd")).unwrap().ino();
    let before = (inode(&sides.up), inode(&sides.ro));
    sides.assert_same(&["restore", "-f", "s.xml", "t.rrd"]);
    assert_eq!((inode(&sides.up), inode(&sides.ro)), before);
    assert_same_restored(&sides, "t.rrd");
    let mode =
        |dir: &std::path::Path| std::fs::metadata(dir.join("new.rrd")).unwrap().mode() & 0o777;
    for (program, dir) in [
        ("rrdtool".to_owned(), &sides.up),
        (sides.rondi().display().to_string(), &sides.ro),
    ] {
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("umask 027; exec \"$0\" restore s.xml new.rrd")
            .arg(program)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }
    assert_eq!(mode(&sides.ro), mode(&sides.up));
}

// D12, an intentional divergence: write_file does not truncate, so restoring
// over a longer file keeps its tail in RRDtool. Rondi truncates the target
// to the restored length.
#[test]
fn d12_restore_over_a_longer_file_truncates_it() {
    let Some(sides) = common::Sides::new(SOURCE, &[]) else {
        return;
    };
    let mut longer = std::fs::read(sides.ro.join("s.rrd")).unwrap();
    longer.extend_from_slice(&[0xab; 4096]);
    sides.write("t.rrd", &longer);
    sides.assert_same(&["restore", "-f", "s.xml", "t.rrd"]);
    let up = std::fs::read(sides.up.join("t.rrd")).unwrap();
    let ro = std::fs::read(sides.ro.join("t.rrd")).unwrap();
    assert_eq!(up.len(), longer.len());
    assert_eq!(ro.len(), longer.len() - 4096);
    assert!(common::masked_rrd(&ro) == common::masked_rrd(&up[..ro.len()]));
}

// Deliberate safety deviation: RRDtool follows a symbolic link at resize.rrd,
// at a restore target and at a dump output file and writes through it. Rondi
// opens those with O_NOFOLLOW and refuses links and hard-linked files, and
// the file they point at is left untouched.
#[test]
fn outputs_refuse_symbolic_and_hard_links() {
    let Some(sides) = common::Sides::new(SOURCE, &[]) else {
        return;
    };
    let dir = &sides.ro;
    let rondi = |args: &[&str]| {
        std::process::Command::new(sides.rondi())
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    };
    std::fs::write(dir.join("victim"), b"keep").unwrap();
    for name in ["resize.rrd", "link.rrd", "link.xml"] {
        std::os::unix::fs::symlink(dir.join("victim"), dir.join(name)).unwrap();
    }
    std::fs::hard_link(dir.join("victim"), dir.join("hard.rrd")).unwrap();
    for args in [
        &["resize", "s.rrd", "0", "GROW", "1"][..],
        &["restore", "-f", "s.xml", "link.rrd"],
        &["restore", "s.xml", "link.rrd"],
        &["restore", "-f", "s.xml", "hard.rrd"],
        &["dump", "s.rrd", "link.xml"],
    ] {
        let output = rondi(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("ERROR: "),
            "{args:?}: {output:?}"
        );
        assert_eq!(
            std::fs::read(dir.join("victim")).unwrap(),
            b"keep",
            "{args:?}"
        );
    }
    assert!(
        std::fs::symlink_metadata(dir.join("resize.rrd"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
