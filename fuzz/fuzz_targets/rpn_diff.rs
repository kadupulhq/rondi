//! Differential RPN check: run the same `xport` against rrdtool.
//!
//! Uses the `rpn_xport` input format but only the xport modes. Each input is
//! rendered by Rondi and by `$RRDTOOL` (default `rrdtool`) over identical seed
//! files; divergences are appended to `$RONDI_DIFF_LOG` (default stderr), and
//! abort the run when `RONDI_DIFF_PANIC` is set. Spawning rrdtool makes this
//! slow, so run it over an existing corpus with `-runs=0` rather than as a
//! long campaign.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;
use std::process::Command;
use std::sync::OnceLock;

fn root() -> &'static std::path::Path {
    static ROOT: OnceLock<std::path::PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = rondi_fuzz::scratch().join("diff");
        std::fs::create_dir_all(&root).expect("diff root");
        for (name, bytes) in rondi_fuzz::SEED_RRDS {
            std::fs::write(root.join(name), bytes).expect("seed");
        }
        std::fs::canonicalize(&root).expect("canonical")
    })
}

fn report(kind: &str, args: &[String], ours: &str, theirs: &str) {
    let text = format!("=== {kind}\nargs: {args:?}\n--- rondi\n{ours}\n--- rrdtool\n{theirs}\n");
    match std::env::var_os("RONDI_DIFF_LOG") {
        Some(path) => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("diff log");
            let _ = file.write_all(text.as_bytes());
        }
        None => eprint!("{text}"),
    }
    if std::env::var_os("RONDI_DIFF_PANIC").is_some() {
        panic!("rpn divergence: {kind}");
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode & 1 != 0 {
        return;
    }
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    // Only expression arguments; options, files and graph elements are
    // handled differently by the two xport front ends on purpose.
    if !text.lines().all(|line| {
        ["CDEF:", "VDEF:", "XPORT:"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            && !line.contains('\0')
    }) {
        return;
    }
    let root = root();
    let g = root.join("g.rrd").display().to_string();
    let c = root.join("c.rrd").display().to_string();
    let mut args = vec![
        "--start".to_owned(),
        "1000000000".to_owned(),
        "--end".to_owned(),
        "1000000080".to_owned(),
        format!("DEF:a={g}:g:AVERAGE"),
        format!("DEF:b={c}:c:AVERAGE"),
        format!("DEF:m={g}:g:MAX:step=20"),
    ];
    args.extend(text.lines().map(str::to_owned));
    if mode & 2 != 0 {
        args.push("--json".to_owned());
    }
    let ours = rondi_fuzz::cli::fuzz_xport(&args);
    let rrdtool = std::env::var("RRDTOOL").unwrap_or_else(|_| "rrdtool".to_owned());
    let output = Command::new(rrdtool)
        .arg("xport")
        .args(&args)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn rrdtool");
    let theirs_out = String::from_utf8_lossy(&output.stdout).into_owned();
    let theirs_err = String::from_utf8_lossy(&output.stderr).into_owned();
    match (ours, output.status.success()) {
        (Ok(ours), true) => {
            if ours != theirs_out {
                report("output", &args, &ours, &theirs_out);
            }
        }
        (Ok(ours), false) => report("rondi accepts, rrdtool rejects", &args, &ours, &theirs_err),
        (Err(ours), true) => report("rondi rejects, rrdtool accepts", &args, &ours, &theirs_out),
        (Err(_), false) => {}
    }
});
