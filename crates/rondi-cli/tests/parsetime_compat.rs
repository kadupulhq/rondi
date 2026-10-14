#![cfg(unix)]
//! Differential checks of the `rrd_parsetime.c` port. Every case runs pinned
//! RRDtool and the Rondi alias with the same arguments and TZ, then compares
//! stdout, stderr and exit status.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

const ZONES: [&str; 5] = [
    "UTC",
    "America/New_York",
    "Europe/Berlin",
    "Asia/Kolkata",
    "Australia/Lord_Howe",
];

type Outcome = (Option<i32>, String, String);

struct Fixture {
    temp: tempfile::TempDir,
    alias: PathBuf,
}

fn fixture() -> Option<Fixture> {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping parsetime differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    for zone in ZONES {
        // A missing zone silently falls back to UTC in both implementations.
        assert!(
            zone == "UTC" || Path::new("/usr/share/zoneinfo").join(zone).exists(),
            "time zone data for {zone} is not installed"
        );
    }
    let temp = tempfile::tempdir().unwrap();
    for (file, step) in [("a.rrd", "10"), ("h.rrd", "3600")] {
        let status = Command::new("rrdtool")
            .current_dir(temp.path())
            .args([
                "create",
                file,
                "--start",
                "1000000000",
                "--step",
                step,
                "DS:x:GAUGE:30:U:U",
                "RRA:AVERAGE:0.5:1:200",
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    Some(Fixture { temp, alias })
}

impl Fixture {
    fn run(&self, program: &Path, tz: &str, args: &[&str]) -> Outcome {
        let output = Command::new(program)
            .current_dir(self.temp.path())
            .env("TZ", tz)
            .args(args)
            .output()
            .unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Specs such as `teatime` or `Jan 1 04` depend on the current time, so a
    /// second boundary can fall between the two runs. Upstream runs again
    /// after Rondi and either neighbour must match.
    fn assert_matches(&self, label: &str, resolve: impl Fn(&Path) -> Outcome) {
        let before = resolve(Path::new("rrdtool"));
        let rondi = resolve(&self.alias);
        if rondi != before {
            assert_eq!(rondi, resolve(Path::new("rrdtool")), "{label}");
        }
    }

    fn assert_same(&self, tz: &str, args: &[&str]) {
        self.assert_matches(&format!("TZ={tz} {args:?}"), |program| {
            self.run(program, tz, args)
        });
    }

    /// `create --start SPEC` followed by `last` exposes the exact parsed time.
    fn assert_create_start(&self, tz: &str, spec: &str) {
        self.assert_matches(&format!("TZ={tz} create --start {spec:?}"), |program| {
            let _ = std::fs::remove_file(self.temp.path().join("t.rrd"));
            let created = self.run(
                program,
                tz,
                &[
                    "create",
                    "t.rrd",
                    "--start",
                    spec,
                    "DS:x:GAUGE:30:U:U",
                    "RRA:AVERAGE:0.5:1:10",
                ],
            );
            if created.0 != Some(0) {
                return created;
            }
            self.run(program, tz, &["last", "t.rrd"])
        });
    }
}

#[test]
fn bare_special_times_and_offsets_after_them() {
    let Some(fx) = fixture() else { return };
    for spec in ["teatime", "midnight", "NOON", "midnight + 1 day"] {
        fx.assert_create_start("UTC", spec);
    }
}

#[test]
fn time_of_day_and_date_forms() {
    let Some(fx) = fixture() else { return };
    for spec in [
        "16:30 01/02/2003",
        "4:30pm 01/02/2003",
        "12am 01/02/2003",
        "noon 20030102",
        "1/2/03",
        "Jan 1 04",
        "Jan 1 99",
        "31.12.2024 23:59",
        "8:00 today",
        "noon yesterday",
        "teatime tomorrow",
        "midnight sun",
        // tod() after a date reads whatever token follows, and the NUMBER
        // case keeps its year sentinel when no year is given.
        "01/02/2003 pm",
        "01/02/2003 +1d",
        "12:00 Jan 1",
        "16:30 01/02",
        "12:4294967295",
        "99999999999999999999",
    ] {
        fx.assert_create_start("UTC", spec);
    }
}

#[test]
fn numeric_epoch_reference_with_offset() {
    let Some(fx) = fixture() else { return };
    fx.assert_create_start("UTC", "1000000000+1d");
    fx.assert_create_start("UTC", "1000000000 + 1 day");
    fx.assert_create_start("UTC", "1000000000+1h-30min 5s");
    // localtime_r() keeps tm_isdst of the epoch; the day offset is then
    // normalized with that flag, so the result is one hour later in summer.
    fx.assert_create_start("America/New_York", "1041397200+180d");
}

#[test]
fn calendar_offsets_accumulate_before_a_single_mktime() {
    let Some(fx) = fixture() else { return };
    fx.assert_create_start("UTC", "Jan 31 2003 10:00 +1mon-1mon");
    fx.assert_create_start("UTC", "May 31 2003 10:00 -1mon-1mon");
    // Seconds/minutes/hours are added after mktime, so their position among
    // day offsets matters across a DST change.
    fx.assert_create_start("America/New_York", "Apr 7 2003 03:30 -1h-1d");
}

#[test]
fn months_minutes_guess_resets_at_each_sign() {
    let Some(fx) = fixture() else { return };
    fx.assert_create_start("UTC", "Jan 1 2004 12:00 -1h-5m");
    fx.assert_create_start("UTC", "Jan 1 2004 12:00 -1d-10m");
    fx.assert_create_start("UTC", "Jan 1 2004 12:00 -1h 5m");
}

#[test]
fn unknown_unit_word_is_seconds() {
    let Some(fx) = fixture() else { return };
    fx.assert_create_start("UTC", "Jan 1 2004 12:00 +1x");
    fx.assert_create_start("UTC", "now-1-1");
}

#[test]
fn iso_dates_are_rejected_upstream() {
    let Some(fx) = fixture() else { return };
    fx.assert_create_start("UTC", "2003-01-02 10:00");
    fx.assert_create_start("UTC", "2003-01-02");
}

#[test]
fn parser_error_messages() {
    let Some(fx) = fixture() else { return };
    for spec in [
        "12:60",
        "13pm",
        "01/32/2003",
        "13/01/2003",
        "01/02/1969",
        "01/02/50",
        "Jan 1 1970",
        "1 jan",
        "now foo",
        "now+",
        "end-1h",
        "epoch",
        "1000000000.5",
        "Jan 1 2004 noon",
        "@x",
        "25",
        "0019700102",
    ] {
        fx.assert_create_start("UTC", spec);
    }
}

#[test]
fn fetch_time_option_errors_carry_start_and_end_prefixes() {
    let Some(fx) = fixture() else { return };
    fx.assert_same("UTC", &["fetch", "a.rrd", "AVERAGE", "--start", "foo"]);
    fx.assert_same(
        "UTC",
        &[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "bar",
        ],
    );
    fx.assert_same("UTC", &["fetch", "a.rrd", "AVERAGE", "--start", "100"]);
    fx.assert_same(
        "UTC",
        &[
            "xport",
            "--start",
            "foo",
            "DEF:x=a.rrd:x:AVERAGE",
            "XPORT:x",
        ],
    );
}

#[test]
fn fetch_small_integers_are_hours_or_months_not_epochs() {
    let Some(fx) = fixture() else { return };
    // "5" is 05:00 today; "315360000" is not above the 10-year epoch cutoff.
    fx.assert_same("UTC", &["fetch", "h.rrd", "AVERAGE", "--start", "5"]);
    fx.assert_same(
        "UTC",
        &["fetch", "h.rrd", "AVERAGE", "--start", "315360000"],
    );
}

#[test]
fn fetch_leading_sign_offset_without_reference() {
    let Some(fx) = fixture() else { return };
    fx.assert_same(
        "UTC",
        &[
            "fetch", "h.rrd", "AVERAGE", "--start", "-1d", "--end", "now",
        ],
    );
    fx.assert_same(
        "UTC",
        &[
            "fetch", "h.rrd", "AVERAGE", "--start", "-1mon", "--end", "+1h",
        ],
    );
}

#[test]
fn relative_range_keeps_the_anchor_dst_flag() {
    let Some(fx) = fixture() else { return };
    // rrd_proc_start_end() calls localtime_r() on the anchor and does not
    // reset tm_isdst before mktime(), so end-1d across a DST change is
    // exactly 86400 s.
    fx.assert_same(
        "America/New_York",
        &[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "-r",
            "10",
            "--start",
            "end-1d",
            "--end",
            "1049630400",
        ],
    );
    fx.assert_same(
        "America/New_York",
        &[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "-r",
            "10",
            "--start",
            "1049544000",
            "--end",
            "start+2d",
        ],
    );
}

#[test]
fn create_start_forms_across_dst_edges() {
    let Some(fx) = fixture() else { return };
    for zone in ZONES {
        for spec in [
            "1041397200+180d",
            "1741500000+1d",
            "1741500000 + 24h",
            "1743868800+1d",
            "Apr 7 2003 03:30 -1h-1d",
            "Mar 9 2025 02:30",
            "Mar 30 2025 02:30",
            "Apr 6 2025 02:30",
            "Oct 5 2025 02:15",
            "Oct 26 2025 02:30",
            "Nov 2 2025 01:30",
            "Mar 8 2025 02:30 +1d",
            "Apr 5 2025 02:30 +1d",
            "Nov 1 2025 01:30 +1 day",
            "Oct 5 2025 01:59 +1min",
            "Oct 5 2025 2:00am -30min",
            "Mar 30 2025 12:00 -1w",
            "midnight Mar 9 2025",
            "20250309",
            "20251026 02:30",
            "4/6/25 2:30am",
            "12pm Apr 6 2025",
        ] {
            fx.assert_create_start(zone, spec);
        }
    }
}

#[test]
fn fetch_ranges_across_dst_edges() {
    let Some(fx) = fixture() else { return };
    // Instants just after the 2003 and 2025 transitions of each tested zone.
    let anchors = [
        "1049630400",
        "1743307200",
        "1743915600",
        "1759635000",
        "1761451200",
        "1762074000",
    ];
    for zone in ZONES {
        for anchor in anchors {
            for (start, end) in [
                ("end-1d", anchor),
                ("end-1w-3h", anchor),
                ("end-1mon", anchor),
                (anchor, "start+2d"),
                (anchor, "start+1d+90min"),
            ] {
                fx.assert_same(
                    zone,
                    &[
                        "fetch", "h.rrd", "AVERAGE", "-r", "3600", "--start", start, "--end", end,
                    ],
                );
            }
        }
    }
}

#[test]
fn update_at_style_times() {
    let Some(fx) = fixture() else { return };
    for value in [
        "noon@5",
        "foo@5",
        "end-1h@5",
        "Jan 1 2004 12:00@5",
        "1000000100+1d@5",
    ] {
        fx.assert_matches(&format!("update {value:?}"), |program| {
            let _ = std::fs::remove_file(fx.temp.path().join("u.rrd"));
            let created = fx.run(
                Path::new("rrdtool"),
                "UTC",
                &[
                    "create",
                    "u.rrd",
                    "--start",
                    "1000000000",
                    "DS:x:GAUGE:300000:U:U",
                    "RRA:AVERAGE:0.5:1:10",
                ],
            );
            assert_eq!(created.0, Some(0));
            let updated = fx.run(program, "UTC", &["update", "u.rrd", value]);
            let last = fx.run(Path::new("rrdtool"), "UTC", &["last", "u.rrd"]);
            (updated.0, updated.1 + &last.1, updated.2)
        });
    }
}
