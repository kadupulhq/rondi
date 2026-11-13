//! A small benchmark harness built on the standard library.
//!
//! Each benchmark warms up for [`WARM_UP`], then takes [`SAMPLES`] samples
//! that together last at least [`MEASUREMENT`]. It reports the mean time per
//! iteration with a 95% confidence interval from Student's t distribution.
//!
//! Arguments (after `cargo bench -p rondi --bench NAME --`):
//!
//! - `FILTER`: run only benchmarks whose name contains `FILTER`.
//! - `--save-baseline NAME`: store the results as baseline `NAME`.
//! - `--baseline NAME`: compare the results with baseline `NAME`.
//!
//! Baselines are tab-separated files under Cargo's target `tmp` directory.
//! Without `--bench`, which `cargo bench` passes and `cargo test --benches`
//! does not, each benchmark runs once as a smoke test.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WARM_UP: Duration = Duration::from_secs(1);
const MEASUREMENT: Duration = Duration::from_secs(3);
const SAMPLES: u32 = 50;
/// Two-sided 95% quantile of Student's t distribution with `SAMPLES - 1`
/// degrees of freedom.
const T_95: f64 = 2.0096;

pub struct Harness {
    measure: bool,
    filter: Option<String>,
    save_baseline: Option<String>,
    baseline: Option<BTreeMap<String, f64>>,
    results: Vec<(String, f64)>,
}

impl Harness {
    pub fn from_args() -> Self {
        let mut harness = Self {
            measure: false,
            filter: None,
            save_baseline: None,
            baseline: None,
            results: Vec::new(),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--bench" => harness.measure = true,
                "--save-baseline" => {
                    harness.save_baseline =
                        Some(args.next().expect("--save-baseline needs a name"));
                }
                "--baseline" => {
                    let name = args.next().expect("--baseline needs a name");
                    harness.baseline = Some(load_baseline(&name));
                }
                flag if flag.starts_with('-') => {}
                filter => harness.filter = Some(filter.to_owned()),
            }
        }
        harness
    }

    pub fn bench<T>(&mut self, name: &str, mut routine: impl FnMut() -> T) {
        if self
            .filter
            .as_deref()
            .is_some_and(|filter| !name.contains(filter))
        {
            return;
        }
        if !self.measure {
            drop(routine());
            println!("{name}: ok");
            return;
        }

        let mut warm_up_iterations = 0u64;
        let warm_up_start = Instant::now();
        while warm_up_start.elapsed() < WARM_UP {
            drop(routine());
            warm_up_iterations += 1;
        }
        let estimate = warm_up_start.elapsed().as_secs_f64() / warm_up_iterations as f64;
        let per_sample = MEASUREMENT.as_secs_f64() / f64::from(SAMPLES);
        let iterations = ((per_sample / estimate).ceil() as u64).max(1);

        // Time each sample's results being dropped too, as callers do.
        let samples = (0..SAMPLES)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..iterations {
                    drop(routine());
                }
                start.elapsed().as_secs_f64() / iterations as f64
            })
            .collect::<Vec<_>>();
        let mean = samples.iter().sum::<f64>() / f64::from(SAMPLES);
        let variance = samples
            .iter()
            .map(|sample| (sample - mean).powi(2))
            .sum::<f64>()
            / f64::from(SAMPLES - 1);
        let margin = T_95 * (variance / f64::from(SAMPLES)).sqrt();

        let mut line = format!(
            "{name:<28} {:>12}  [{} .. {}]",
            format_time(mean),
            format_time(mean - margin),
            format_time(mean + margin),
        );
        if let Some(previous) = self.baseline.as_ref().and_then(|base| base.get(name)) {
            let _ = write!(
                line,
                "  {:+.1}% vs baseline",
                (mean / previous - 1.0) * 100.0
            );
        }
        println!("{line}");
        self.results.push((name.to_owned(), mean));
    }

    pub fn finish(self) {
        let Some(name) = self.save_baseline else {
            return;
        };
        let path = baseline_path(&name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents = self
            .results
            .iter()
            .map(|(bench, mean)| format!("{bench}\t{mean:e}\n"))
            .collect::<String>();
        std::fs::write(&path, contents).unwrap();
        println!("saved baseline {name} to {}", path.display());
    }
}

fn baseline_path(name: &str) -> PathBuf {
    assert!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "baseline names use letters, digits, '-', '_' and '.'"
    );
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("bench-baselines")
        .join(format!("{name}.tsv"))
}

fn load_baseline(name: &str) -> BTreeMap<String, f64> {
    let path = baseline_path(name);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read baseline {}: {error}", path.display()));
    contents
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(bench, mean)| (bench.to_owned(), mean.parse().unwrap()))
        .collect()
}

fn format_time(seconds: f64) -> String {
    let (value, unit) = match seconds {
        s if s < 1e-6 => (s * 1e9, "ns"),
        s if s < 1e-3 => (s * 1e6, "µs"),
        s if s < 1.0 => (s * 1e3, "ms"),
        s => (s, "s"),
    };
    format!("{value:.3} {unit}")
}
