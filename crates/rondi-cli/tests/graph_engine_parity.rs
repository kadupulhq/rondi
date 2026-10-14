#![cfg(unix)]
//! Differentials for the graph data engine (`data_fetch`, `rrd_reduce_data`,
//! `data_calc`, `rpn_calc` and `vdef_calc` in rrd_graph.c and
//! rrd_rpncalc.c) against pinned RRDtool 1.11.0.

mod common;

use common::{Outcome, Sides};

/// `t.rrd`: a GAUGE `a` and a COUNTER `b` at a 300-second step with
/// AVERAGE, MAX, MIN and LAST archives plus an hourly AVERAGE. The samples
/// land off the step boundary so the stored values carry full mantissas,
/// and the gauge is a small wave on a large offset so variance and slope
/// arithmetic cancels enough to expose the last bit of rounding.
fn sides() -> Option<Sides> {
    let create: &[&str] = &[
        "create",
        "t.rrd",
        "--start",
        "1699999000",
        "--step",
        "300",
        "DS:a:GAUGE:600:U:U",
        "DS:b:COUNTER:600:U:U",
        "RRA:AVERAGE:0.5:1:600",
        "RRA:MAX:0.5:1:600",
        "RRA:MIN:0.5:1:600",
        "RRA:LAST:0.5:1:600",
        "RRA:AVERAGE:0.5:12:100",
    ];
    let mut update = vec![String::from("update"), String::from("t.rrd")];
    let mut counter = 0_i64;
    for i in 1..=500_i64 {
        counter += (i * 7919) % 30_011;
        let gauge = 1_000_000 + (i % 80 - 40).abs() * 731 + (i * 7919) % 500;
        update.push(format!(
            "{}:{}.{:03}:{counter}",
            1_699_999_000 + i * 300 + 77,
            gauge / 1000,
            gauge % 1000
        ));
    }
    let update: Vec<&str> = update.iter().map(String::as_str).collect();
    Sides::new(&[create, &update], &[])
}

/// Status, stdout and stderr match, and an image written by both sides has
/// the same PNG signature and dimensions. Rondi draws with its own
/// renderer, so the pixels themselves are not compared.
fn assert_same(sides: &Sides, args: &[&str], env: &[(&str, &str)]) {
    for side in [&sides.up, &sides.ro] {
        let _ = std::fs::remove_file(side.join("out.png"));
    }
    let (up, ro): (Outcome, Outcome) = sides.both(args, env);
    assert_eq!(ro, up, "args: {args:?} env: {env:?}");
    let header = |dir: &std::path::Path| {
        std::fs::read(dir.join("out.png"))
            .ok()
            .map(|bytes| bytes[..bytes.len().min(24)].to_vec())
    };
    assert_eq!(
        header(&sides.ro),
        header(&sides.up),
        "out.png differs: {args:?}"
    );
}

fn split(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

// rrd_rpncalc.c:1110, 1131, 1157 and 1302 and rrd_graph.c:6236-6264: the
// pinned aarch64 build fuses the PREDICT and STDEV accumulators, the
// PREDICTSIGMA variance, the PREDICTPERC interpolation and the
// least-squares sums, slope and intercept, so the last bits differ from
// separately rounded arithmetic.
#[test]
fn rpn_and_vdef_accumulators_round_like_the_pinned_build() {
    let Some(sides) = sides() else { return };
    let window = "-s 1700000000 -e 1700140000";
    for cdef in [
        "300,1,300,a,PREDICTSIGMA",
        "300,600,900,3,1800,a,PREDICTSIGMA",
    ] {
        let line = format!("xport {window} DEF:a=t.rrd:a:AVERAGE CDEF:r={cdef} XPORT:r:r");
        assert_same(&sides, &split(&line), &[]);
    }
    // xport rounds to ten digits, so print a VDEF over the CDEF in full.
    for (cdef, vdef) in [
        ("300,1,300,a,PREDICTSIGMA", "AVERAGE"),
        ("300,1,3000,71.7,a,PREDICTPERC", "MINIMUM"),
        ("300,1,3000,71.7,a,PREDICTPERC", "STDEV"),
        ("a,PREV,a,3,STDEV", "MAXIMUM"),
    ] {
        let line = format!(
            "graphv - {window} DEF:a=t.rrd:a:AVERAGE CDEF:r={cdef} VDEF:v=r,{vdef} PRINT:v:%.17le"
        );
        assert_same(&sides, &split(&line), &[]);
    }
    for (ds, function) in [
        ("a", "LSLSLOPE"),
        ("a", "LSLINT"),
        ("a", "LSLCORREL"),
        ("a", "STDEV"),
        ("b", "LSLSLOPE"),
        ("b", "LSLINT"),
    ] {
        let line = format!(
            "graphv - {window} DEF:a=t.rrd:{ds}:AVERAGE VDEF:v=a,{function} PRINT:v:%.17le"
        );
        assert_same(&sides, &split(&line), &[]);
    }
}
