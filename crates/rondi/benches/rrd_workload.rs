use rondi::{
    RrdXportCdef, RrdXportColumn, RrdXportDefinition, RrdXportOperation, VdefFunction,
    create_rrd_file, fetch_rrd_file, fetch_xport_with_operations, update_rrd_values,
};
use std::time::Instant;

fn main() {
    let iterations = std::env::var("RONDI_BENCH_FETCHES")
        .map(|value| value.parse::<usize>().expect("positive fetch count"))
        .unwrap_or(200);
    assert!(iterations > 0);
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("load.rrd");
    let start = 1_700_000_000;
    let sources = (0..4)
        .map(|index| format!("DS:x{index}:GAUGE:2:U:U"))
        .collect::<Vec<_>>();
    create_rrd_file(
        &path,
        start,
        1,
        &sources,
        &["RRA:AVERAGE:0.5:1:7200".into()],
        true,
    )
    .unwrap();
    let begun = Instant::now();
    for index in 1..=300 {
        update_rrd_values(&path, start + index, &[Some(index as f64); 4]).unwrap();
    }
    println!(
        "rrd_update: operations=300 elapsed_us={}",
        begun.elapsed().as_micros()
    );
    println!(
        "rrd_fetch: pid={} operations={iterations} rows=7201 sources=4",
        std::process::id()
    );
    let begun = Instant::now();
    for _ in 0..iterations {
        let result = fetch_rrd_file(&path, "AVERAGE", start + 300 - 7200, start + 300, 1).unwrap();
        assert_eq!(result.rows.len(), 7201);
        std::hint::black_box(result);
    }
    println!(
        "rrd_fetch: elapsed_us={} ops_per_sec={:.1}",
        begun.elapsed().as_micros(),
        iterations as f64 / begun.elapsed().as_secs_f64()
    );
    let graph_iterations = std::env::var("RONDI_BENCH_GRAPHS")
        .map(|value| value.parse::<usize>().expect("positive graph count"))
        .unwrap_or(200);
    assert!(graph_iterations > 0);
    let definitions = (0..4)
        .map(|index| RrdXportDefinition {
            name: format!("x{index}"),
            file: path.clone(),
            data_source: format!("x{index}"),
            consolidation: "AVERAGE".into(),
        })
        .collect::<Vec<_>>();
    let operations = [
        RrdXportOperation::Cdef(RrdXportCdef {
            name: "mean".into(),
            expression: "x0,x1,+,x2,+,x3,+,4,/".into(),
        }),
        RrdXportOperation::Vdef {
            name: "maximum".into(),
            variable: "mean".into(),
            function: VdefFunction::Maximum,
            percentile: None,
        },
    ];
    let columns = [RrdXportColumn {
        variable: "mean".into(),
        legend: "Mean".into(),
    }];
    println!(
        "graph_calc: pid={} operations={graph_iterations} rows=7200 defs=4 cdefs=1 vdefs=1",
        std::process::id()
    );
    let begun = Instant::now();
    for _ in 0..graph_iterations {
        let result = fetch_xport_with_operations(
            &definitions,
            &operations,
            &columns,
            start + 300 - 7200,
            start + 300,
            1,
            7200,
        )
        .unwrap();
        assert_eq!(result.rows.len(), 7200);
        assert_eq!(result.vdef_values["maximum"].value, 300.0);
        std::hint::black_box(result);
    }
    let elapsed = begun.elapsed();
    println!(
        "graph_calc: elapsed_us={} ops_per_sec={:.1}",
        elapsed.as_micros(),
        graph_iterations as f64 / elapsed.as_secs_f64()
    );
}
