use rondi::graph::{Export, Graph};
use rondi::{create_rrd_file, fetch_rrd_file, update_rrd_values};
use std::{hint::black_box, path::PathBuf};

mod harness;
use harness::Harness;

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

    /// Runs the script parsing, the data fetch, the CDEF and VDEF calculation and the
    /// export steps, as `rrdtool xport` does after option parsing.
    fn calculate(&self, script: &[String]) -> (Graph, Export) {
        let mut image = Graph::new(self.start, self.end, 1);
        let resolve = |_: Option<&str>, _: Option<&str>, start: i64, end: i64| Ok((start, end));
        image.parse_elements(script, &resolve).unwrap();
        image.fetch(&mut |_, _| Ok(None)).unwrap();
        image.compute().unwrap();
        let data = image.export(false).unwrap();
        (image, data)
    }
}

fn main() {
    let mut harness = Harness::from_args();
    // Setup and durable writes stay outside timed query loops. Both fixtures
    // contain known value blocks and have rolled past their initial ring row.
    let fixtures = [Fixture::new(120), Fixture::new(7200)];
    for fixture in &fixtures {
        let result =
            fetch_rrd_file(&fixture.path, "AVERAGE", fixture.start, fixture.end, 1).unwrap();
        assert_eq!(result.rows.len() as u64, fixture.rows + 1);
        assert!(
            result.rows[..fixture.rows as usize]
                .iter()
                .all(|row| row.values.iter().all(Option::is_some))
        );
        harness.bench(&format!("rrd_fetch/{}", fixture.rows), || {
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
    }

    for fixture in &fixtures {
        for (name, expression) in [
            ("arithmetic", "x0,x1,+,x2,+,x3,+,4,/"),
            ("prev", "x0,PREV,ADDNAN"),
        ] {
            let script = fixture.script(expression);
            let (image, data) = fixture.calculate(&script);
            assert_eq!(data.rows.len() as u64, fixture.rows);
            let maximum = image
                .elements
                .iter()
                .find(|element| element.name.as_deref() == Some("maximum"))
                .unwrap();
            assert!(maximum.vdef.value.is_finite());
            harness.bench(&format!("graph_calc/{name}/{}", fixture.rows), || {
                black_box(fixture.calculate(black_box(&script)))
            });
        }
    }
    harness.finish();
}
