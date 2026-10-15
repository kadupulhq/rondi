//! Evaluation tests. Results depend on the time zone, so the zone-specific
//! checks run in a child copy of this test binary started with a fixed
//! `TZ`; changing `TZ` inside the shared test process would race with
//! other tests.

use super::*;

/// Set in the child process so the zone-specific tests know `TZ` is fixed.
const CHILD: &str = "RONDI_TIME_ZONE_CHILD";

/// 2001-09-09 01:46:40 UTC, a Sunday.
const NOW: i64 = 1_000_000_000;

fn run_in_zone(zone: &str, test: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TZ", zone)
        .env(CHILD, zone)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{test} in {zone}:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn in_child(zone: &str) -> bool {
    std::env::var(CHILD).is_ok_and(|value| value == zone)
}

fn at(text: &str) -> i64 {
    at_now(text, NOW)
}

fn at_now(text: &str, now: i64) -> i64 {
    parse_time_spec(text, now)
        .unwrap_or_else(|error| panic!("{text}: {error}"))
        .absolute()
        .unwrap_or_else(|| panic!("{text} is relative"))
}

fn err(text: &str) -> String {
    parse_time_spec(text, NOW).unwrap_err().to_string()
}

fn range(start: &str, end: &str) -> Result<(i64, i64), TimeSpecError> {
    resolve_start_end(
        &parse_time_spec(start, NOW).unwrap(),
        &parse_time_spec(end, NOW).unwrap(),
    )
}

#[test]
fn utc() {
    run_in_zone("UTC", "time::tests::utc_child");
}

#[test]
#[ignore = "run by `utc` with TZ=UTC"]
fn utc_child() {
    if !in_child("UTC") {
        return;
    }
    // References and current-time defaults.
    assert_eq!(at("now"), NOW);
    assert_eq!(at("now-1d"), NOW - 86_400);
    assert_eq!(at("-1h"), NOW - 3600);
    assert_eq!(at("noon yesterday"), 999_950_400);
    assert_eq!(at("noon yesterday -3hours"), 999_950_400 - 3 * 3600);
    assert_eq!(at("noon sun"), 1_000_036_800);
    assert_eq!(at("noon sat"), 1_000_555_200);
    assert_eq!(at("noon"), 1_000_036_800);
    assert_eq!(at("9am-1day"), 1_000_026_000 - 86_400);
    assert_eq!(at("midnight + 1 day"), 999_993_600 + 86_400);
    assert_eq!(at("Jan 2 2003"), 1_041_472_000, "keeps the time of day");
    assert_eq!(
        at("01/02/2003"),
        1_041_465_600,
        "numeric dates are midnight"
    );
    assert_eq!(at("Jan 2 2003 12"), 1_041_508_800);
    assert_eq!(at("12:00 1000000000"), 1_000_000_000);

    // Calendar arithmetic: months and years normalize with carries; leap
    // years.
    assert_eq!(at("Jan 31 2003 10:00 +1mon"), 1_046_685_600);
    assert_eq!(at("Jan 31 2024 10:00 +1mon"), 1_709_373_600);
    assert_eq!(at("Jan 31 2003 10:00 +1mon-1mon"), 1_044_007_200);
    assert_eq!(at("May 31 2003 10:00 -1mon-1mon"), 1_049_104_800);
    assert_eq!(at("May 31 2003 10:00 -1mon"), 1_051_783_200);
    assert_eq!(at("Feb 29 2024 10:00 +1y"), 1_740_823_200);
    assert_eq!(at("Feb 29 2024 10:00 -1y"), 1_677_664_800);
    assert_eq!(at("Feb 30 2003 10:00"), at("Mar 2 2003 10:00"));
    assert_eq!(at("Jan 0 2003 12:00"), at("Dec 31 2002 12:00"));
    assert_eq!(at("02/30/2003"), at("03/02/2003"));
    assert_eq!(at("Dec 31 2003 12:00 +1d"), 1_072_958_400);
    assert_eq!(at("12:00 Jan 1 2004 -2w"), 1_072_958_400 - 14 * 86_400);
    assert_eq!(at("+1d-24h"), NOW);

    // Exact offsets wrap per amount; the sum does not.
    assert_eq!(at("1000000000+4294967296s"), NOW);
    assert_eq!(at("1000000000+2000000000s+2000000000s"), 5_000_000_000);

    // Year rules (TS-26, TS-33, TS-34).
    assert_eq!(at("13:00 Jan 2"), 884_541_474_000);
    assert_eq!(at("5 Jan 2"), 884_541_445_200);
    assert_eq!(at("1/2/37"), 2_114_467_200);
    assert_eq!(at("1/2/100"), 946_771_200);
    assert_eq!(at("13:00 Jan 2 99999"), 3_093_496_578_000);
    assert_eq!(
        at("Jan 2 2147485547 13:00"),
        67_768_036_160_227_200 + 46_800
    );

    // `epoch` (TS-16): the platforms differ before 1900.
    if cfg!(target_os = "macos") {
        assert_eq!(at("epoch+1000000000"), 999_999_999);
        assert_eq!(at("epoch-1"), -2);
    } else {
        assert_eq!(at("epoch+1000000000"), -1_209_075_200);
    }
    assert_eq!(at("epoch+1d+1000000000s"), -1_208_988_800);

    // start and end.
    assert_eq!(range("end-1d", "1000086400"), Ok((NOW, 1_000_086_400)));
    assert_eq!(
        range("1000000000", "start+1d+90min"),
        Ok((NOW, NOW + 86_400 + 5400))
    );
    assert_eq!(range("1000000000", "s+1s 2s"), Ok((NOW, NOW + 3)));
    assert_eq!(
        range("start+1", "now"),
        Err(TimeSpecError::StartRelativeToItself)
    );
    assert_eq!(
        range("start+1", "end-1"),
        Err(TimeSpecError::StartRelativeToItself)
    );
    assert_eq!(range("now", "e+1"), Err(TimeSpecError::EndRelativeToItself));
    assert_eq!(range("e-1h", "s+1h"), Err(TimeSpecError::MutuallyRelative));
}

#[test]
fn new_york() {
    // Without the zone file the C library silently uses UTC.
    if !std::path::Path::new("/usr/share/zoneinfo/America/New_York").is_file() {
        eprintln!("skipping new_york: zoneinfo not installed");
        return;
    }
    run_in_zone("America/New_York", "time::tests::new_york_child");
}

#[test]
#[ignore = "run by `new_york` with TZ=America/New_York"]
fn new_york_child() {
    if !in_child("America/New_York") {
        return;
    }
    // A wall time in the spring-forward gap moves forward (TS-48).
    assert_eq!(at("Mar 9 2025 02:30"), 1_741_505_400);
    assert_eq!(at("Mar 9 2025 02:30"), at("Mar 9 2025 03:30"));
    // Exact amounts cross the change as seconds (TS-43, TS-49).
    assert_eq!(at("8:00 Mar 8 2025 +48 hours"), 1_741_611_600);
    assert_eq!(at("8:00 Mar 8 2025 +2 days"), 1_741_608_000);
    assert_eq!(at("Mar 9 2025 02:30 -1h"), 1_741_505_400 - 3600);
    // A timestamp keeps its UTC offset through a calendar shift (TS-47).
    assert_eq!(at("1041397200+180d"), 1_056_949_200);
    assert_eq!(at("1041397200+180d"), at("Jun 30 2003 1:00"));
    assert_eq!(at("1741500000+1d"), at("1741500000+24h"));
    assert_eq!(
        range("end-1d", "1741550000"),
        Ok((1_741_550_000 - 86_400, 1_741_550_000))
    );
    // A written date takes the offset in force at that date.
    assert_eq!(
        at("noon 07/01/2003") - at("noon 01/01/2003"),
        181 * 86_400 - 3600
    );
}

#[test]
fn errors_do_not_depend_on_the_zone() {
    assert_eq!(err("foo"), "unparsable time: foo");
    assert_eq!(
        err("12:30:45 01/02/2003"),
        "unparsable trailing text: '...:45 01/02/2003'"
    );
    assert_eq!(
        err("1/2/1970"),
        "invalid year 1970 (should be either 00-99 or >1900)"
    );
    assert_eq!(
        parse_time_spec("end-1h", NOW).unwrap().anchor(),
        Some(Anchor::End)
    );
    assert_eq!(parse_time_spec("end-1h", NOW).unwrap().absolute(), None);
    assert_eq!(
        String::from(TimeSpecError::OutOfRange),
        "the specified time is incorrect (out of range?)"
    );
}

#[test]
fn window_defaults_and_prefixes() {
    let window = (315_360_000, 315_360_001);
    assert_eq!(resolve_window(None, None, window, NOW), Ok(window));
    assert_eq!(
        resolve_window(Some("1000000000"), Some("start+2h"), window, NOW),
        Ok((NOW, NOW + 7200))
    );
    assert_eq!(
        resolve_window(Some("end-2h"), None, window, NOW),
        Ok((315_360_001 - 7200, 315_360_001))
    );
    assert_eq!(
        resolve_window(Some("foo"), Some("1"), window, NOW),
        Err("start time: unparsable time: foo".to_owned())
    );
    assert_eq!(
        resolve_window(None, Some("bar"), window, NOW),
        Err("end time: unparsable time: bar".to_owned())
    );
    assert_eq!(
        resolve_window(Some("end-2h"), Some("start+2h"), window, NOW),
        Err("the start and end times cannot be specified relative to each other".to_owned())
    );
}

/// A small deterministic generator, so a failure reproduces.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[test]
fn random_input_never_panics() {
    const PIECES: &[&str] = &[
        "now",
        "n",
        "start",
        "s",
        "end",
        "e",
        "epoch",
        "midnight",
        "noon",
        "teatime",
        "am",
        "pm",
        "yesterday",
        "today",
        "tomorrow",
        "jan",
        "February",
        "may",
        "sun",
        "Monday",
        "sec",
        "min",
        "m",
        "h",
        "d",
        "w",
        "mon",
        "y",
        "years",
        "x",
        "+",
        "-",
        ":",
        ".",
        "/",
        " ",
        "_",
        ",",
        "@",
        "#",
        "é",
        "0",
        "1",
        "12",
        "23",
        "24",
        "59",
        "60",
        "009",
        "315360000",
        "315360001",
        "19700102",
        "20030102",
        "24000100",
        "4294967296",
        "2147483647",
        "2147485547",
        "2147485548",
        "9223372036854775807",
        "99999999999999999999999999",
        "67767976233532800",
    ];
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    let nows = [0, NOW, -1, i64::MAX, i64::MIN, 67_767_976_233_532_800];
    for round in 0..20_000 {
        let mut text = String::new();
        for _ in 0..rng.below(8) {
            text.push_str(PIECES[rng.below(PIECES.len())]);
            if rng.below(3) == 0 {
                text.push(' ');
            }
        }
        if round % 7 == 0 {
            let byte = (rng.next() % 128) as u8;
            text.push(char::from(byte));
        }
        let now = nows[round % nows.len()];
        let parsed = parse_time_spec(&text, now);
        if let Ok(spec) = parsed {
            let other =
                parse_time_spec(PIECES[rng.below(PIECES.len())], now).unwrap_or(TimeSpec::at(now));
            let _ = resolve_start_end(&spec, &other);
            let _ = resolve_start_end(&other, &spec);
        }
        let _ = resolve_window(Some(&text), None, (now, now), now);
        let _ = parse_update_timestamp(&text, now as f64);
    }
}

#[test]
fn extreme_values_never_panic() {
    let long_digits = "9".repeat(100_000);
    let long_spec = "+1d".repeat(50_000);
    for text in [
        long_digits.as_str(),
        long_spec.as_str(),
        "now+9999999999999999999999999d",
        "now+2147483647y+2147483647y",
        "now-2147483648mon",
        "Jan 99999999999999999999 99999999999999999999",
        "99999999999999999999/99999999999999999999/99999999999999999999",
        "12:00 9223372036854775806",
        "9223372036854775806+1y",
        "epoch+2147483647y",
        "epoch-2147483647y",
    ] {
        for now in [0, NOW, i64::MAX, i64::MIN] {
            let _ = parse_time_spec(text, now);
        }
    }
}
