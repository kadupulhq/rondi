use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rondi::graph::{GraphImage, XportData};
use rondi::{create_rrd_file, fetch_rrd_file, update_rrd_values};
use std::{hint::black_box, path::PathBuf, time::Duration};

struct Fixture {
    _root: tempfile::TempDir,
    path: PathBuf,
    start: i64,
    end: i64,
    rows: u64,
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
        Self {
            _root: root,
            path,
            start: end - rows as i64,
            end,
            rows,
        }
    }

    fn script(&self, expression: &str) -> Vec<String> {
        let mut script = (0..4)
            .map(|index| format!("DEF:x{index}={}:x{index}:AVERAGE", self.path.display()))
            .collect::<Vec<_>>();
        script.push(format!("CDEF:result={expression}"));
        script.push("VDEF:maximum=result,MAXIMUM".into());
        script.push("XPORT:result:Result".into());
        script
    }

    /// Runs the graph engine's script parsing, data_fetch, data_calc and
    /// xport steps, as `rrdtool xport` does after option parsing.
    fn calculate(&self, script: &[String]) -> (GraphImage, XportData) {
        let mut image = GraphImage::new(self.start, self.end, 1);
        let resolve = |_: Option<&str>, _: Option<&str>, start: i64, end: i64| Ok((start, end));
        image.graph_script(script, &resolve).unwrap();
        image.data_fetch(&mut |_, _| Ok(None)).unwrap();
        image.data_calc().unwrap();
        let data = image.xport(false).unwrap();
        (image, data)
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

    let mut graphs = criterion.benchmark_group("graph_calc");
    for fixture in &fixtures {
        graphs.throughput(Throughput::Elements(fixture.rows));
        for (name, expression) in [
            ("arithmetic", "x0,x1,+,x2,+,x3,+,4,/"),
            ("prev", "x0,PREV,ADDNAN"),
        ] {
            let script = fixture.script(expression);
            let (image, data) = fixture.calculate(&script);
            assert_eq!(data.rows.len() as u64, fixture.rows);
            let maximum = image
                .gdes
                .iter()
                .find(|element| element.vname == "maximum")
                .unwrap();
            assert!(maximum.vf.val.is_finite());
            graphs.bench_with_input(
                BenchmarkId::new(name, fixture.rows),
                fixture,
                |bencher, fixture| {
                    bencher.iter(|| black_box(fixture.calculate(black_box(&script))));
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
