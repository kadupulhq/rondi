use rondi::{VdefFunction, evaluate_vdef};
use std::process::Command;

#[test]
fn vdef_aggregates_match_pinned_rrdtool_graphv() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping VDEF differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("vdef.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            database.to_str().unwrap(),
            "1000000010:10000000000000000",
            "1000000020:1",
            "1000000030:-10000000000000000",
            "1000000040:3",
            "1000000050:7",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );

    // RRDtool's graph buffer includes one unknown right-edge boundary slot;
    // the emitted rows are [1, -1e16, 3, 7, unknown]. Cancellation makes a
    // different summation order visible in the formatted aggregate result.
    let values = [1.0, -1.0e16, 3.0, 7.0, f64::NAN];
    let definitions = [
        ("maximum", "x,MAXIMUM", VdefFunction::Maximum, None),
        ("minimum", "x,MINIMUM", VdefFunction::Minimum, None),
        ("average", "x,AVERAGE", VdefFunction::Average, None),
        ("stdev", "x,STDEV", VdefFunction::Stdev, None),
        ("percent", "x,95,PERCENT", VdefFunction::Percent, Some(95.0)),
        (
            "percent_mid",
            "x,50,PERCENT",
            VdefFunction::Percent,
            Some(50.0),
        ),
        (
            "percent_nan",
            "x,0,PERCENT",
            VdefFunction::Percent,
            Some(0.0),
        ),
        (
            "percentnan",
            "x,95,PERCENTNAN",
            VdefFunction::PercentNan,
            Some(95.0),
        ),
        (
            "percentnan_zero",
            "x,0,PERCENTNAN",
            VdefFunction::PercentNan,
            Some(0.0),
        ),
        ("total", "x,TOTAL", VdefFunction::Total, None),
        ("first", "x,FIRST", VdefFunction::First, None),
        ("last", "x,LAST", VdefFunction::Last, None),
        ("slope", "x,LSLSLOPE", VdefFunction::LslSlope, None),
        ("intercept", "x,LSLINT", VdefFunction::LslIntercept, None),
        (
            "correlation",
            "x,LSLCORREL",
            VdefFunction::LslCorrelation,
            None,
        ),
    ];
    let mut command = Command::new("rrdtool");
    command
        .arg("graphv")
        .arg(temp.path().join("vdef.svg"))
        .args(["--start", "1000000010", "--end", "1000000050"])
        .arg(format!("DEF:x={}:x:AVERAGE", database.display()));
    for (name, definition, _, _) in definitions {
        command.arg(format!("VDEF:{name}={definition}"));
        command.arg(format!("PRINT:{name}:%0.17le"));
    }
    let oracle = command.output().unwrap();
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let output = String::from_utf8(oracle.stdout).unwrap();
    let expected: Vec<f64> = output
        .lines()
        .filter_map(|line| {
            line.split_once("= \"")
                .map(|(_, value)| value.trim_end_matches('"'))
        })
        .map(|value| value.parse::<f64>().unwrap())
        .collect();
    assert_eq!(expected.len(), definitions.len(), "{output}");
    for ((name, _, function, percentile), upstream) in definitions.into_iter().zip(expected) {
        let actual = evaluate_vdef(function, percentile, &values, 1_000_000_010, 10).unwrap();
        if upstream.is_nan() {
            assert!(actual.value.is_nan(), "{name}: {} != NaN", actual.value);
        } else if matches!(
            function,
            VdefFunction::Average | VdefFunction::Stdev | VdefFunction::Total
        ) {
            assert_eq!(actual.value.to_bits(), upstream.to_bits(), "{name}");
        } else {
            assert!(
                (actual.value - upstream).abs() <= 1e-8 * upstream.abs().max(1.0),
                "{name}: {} != {upstream}",
                actual.value
            );
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let alias = temp.path().join("rrdtool");
        symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
        for (name, expression) in [
            ("average", "x,AVERAGE"),
            ("stdev", "x,STDEV"),
            ("total", "x,TOTAL"),
        ] {
            let output_path = temp.path().join(format!("{name}.svg"));
            let definition = format!("DEF:x={}:x:AVERAGE", database.display());
            let vdef = format!("VDEF:{name}={expression}");
            let print = format!("PRINT:{name}:%0.17le");
            let command_args = [
                "graphv",
                output_path.to_str().unwrap(),
                "--start",
                "1000000010",
                "--end",
                "1000000050",
            ];
            let expected = Command::new("rrdtool")
                .args(command_args)
                .arg(&definition)
                .arg(&vdef)
                .arg(&print)
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .args(command_args)
                .arg(definition)
                .arg(vdef)
                .arg(print)
                .output()
                .unwrap();
            assert!(
                expected.status.success(),
                "{}",
                String::from_utf8_lossy(&expected.stderr)
            );
            assert!(
                actual.status.success(),
                "{}",
                String::from_utf8_lossy(&actual.stderr)
            );
            let print_line = |output: &[u8]| {
                String::from_utf8_lossy(output)
                    .lines()
                    .find(|line| line.starts_with("print[0] = "))
                    .unwrap_or_default()
                    .to_owned()
            };
            assert_eq!(
                print_line(&actual.stdout),
                print_line(&expected.stdout),
                "{name}"
            );
        }
    }
}
