use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rondi::{
    RrdXportCdef, RrdXportColumn, RrdXportDefinition, RrdXportOperation, VdefFunction,
    create_rrd_file, fetch_rrd_file, fetch_xport_with_operations, update_rrd_values,
};
use std::{hint::black_box, path::PathBuf, time::Duration};

struct Fixture {
    _root: tempfile::TempDir,
    path: PathBuf,
    start: i64,
    end: i64,
    rows: u64,
    definitions: Vec<RrdXportDefinition>,
}

impl Fixture {
    fn new(rows: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queries.rrd");
        let initial = 1_700_000_000;
        let sources = (0..4)
            .map(|index| format!("DS:x{index}:GAUGE:10000:U:U"))
            .collect::<Vec<_>>();
        create_rrd_file(
            &path,
            initial,
            1,
            &sources,
            &[format!("RRA:AVERAGE:0.5:1:{rows}")],
            true,
        )
        .unwrap();
        let end = initial + (rows + rows / 5) as i64;
        for (timestamp, value) in [
            (initial + rows as i64 / 3, 1.0),
            (initial + 2 * rows as i64 / 3, 2.0),
            (end, 3.0),
        ] {
            update_rrd_values(&path, timestamp, &[Some(value); 4]).unwrap();
        }
        let definitions = (0..4)
            .map(|index| RrdXportDefinition {
                name: format!("x{index}"),
                file: path.clone(),
                data_source: format!("x{index}"),
                consolidation: "AVERAGE".into(),
            })
            .collect();
        Self {
            _root: root,
            path,
            start: end - rows as i64,
            end,
            rows,
            definitions,
        }
    }

    fn calculate(
        &self,
        operations: &[RrdXportOperation],
        columns: &[RrdXportColumn],
    ) -> rondi::RrdXportResult {
        fetch_xport_with_operations(
            &self.definitions,
            operations,
            columns,
            self.start,
            self.end,
            1,
            self.rows,
        )
        .unwrap()
    }
}

fn rrd_queries(criterion: &mut Criterion) {
    // Setup and durable writes stay outside timed query loops. Both fixtures
    // contain known value blocks and have rolled past their initial ring row.
    let fixtures = [Fixture::new(120), Fixture::new(7200)];
    let mut fetches = criterion.benchmark_group("rrd_fetch");
    for fixture in &fixtures {
        let result =
            fetch_rrd_file(&fixture.path, "AVERAGE", fixture.start, fixture.end, 1).unwrap();
        assert_eq!(result.rows.len() as u64, fixture.rows + 1);
        assert!(
            result.rows[..fixture.rows as usize]
                .iter()
                .all(|row| row.values.iter().all(Option::is_some))
        );
        fetches.throughput(Throughput::Elements((fixture.rows + 1) * 4));
        fetches.bench_with_input(
            BenchmarkId::from_parameter(fixture.rows),
            fixture,
            |bencher, fixture| {
                bencher.iter(|| {
                    black_box(
                        fetch_rrd_file(
                            &fixture.path,
                            "AVERAGE",
                            black_box(fixture.start),
                            black_box(fixture.end),
                            1,
                        )
                        .unwrap(),
                    )
                });
            },
        );
    }
    fetches.finish();

    let columns = [RrdXportColumn {
        variable: "result".into(),
        legend: "Result".into(),
    }];
    let mut graphs = criterion.benchmark_group("graph_calc");
    for fixture in &fixtures {
        graphs.throughput(Throughput::Elements(fixture.rows));
        for (name, expression) in [
            ("arithmetic", "x0,x1,+,x2,+,x3,+,4,/"),
            ("prev", "x0,PREV,ADDNAN"),
        ] {
            let operations = [
                RrdXportOperation::Cdef(RrdXportCdef {
                    name: "result".into(),
                    expression: expression.into(),
                }),
                RrdXportOperation::Vdef {
                    name: "maximum".into(),
                    variable: "result".into(),
                    function: VdefFunction::Maximum,
                    percentile: None,
                },
            ];
            let result = fixture.calculate(&operations, &columns);
            assert_eq!(result.rows.len() as u64, fixture.rows);
            assert!(result.vdef_values["maximum"].value.is_finite());
            graphs.bench_with_input(
                BenchmarkId::new(name, fixture.rows),
                fixture,
                |bencher, fixture| {
                    bencher.iter(|| black_box(fixture.calculate(black_box(&operations), &columns)));
                },
            );
        }
    }
    graphs.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3));
    targets = rrd_queries
}
criterion_main!(benches);
