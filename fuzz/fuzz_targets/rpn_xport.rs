//! RPN/CDEF/VDEF/PRINT expressions through xport and graph.
//!
//! Input lines are appended as arguments after fixed DEFs over the seed
//! files. The first byte picks xport or graph and the output format.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    // LINE widths beyond a few hundred pixels hang the PNG renderer (see
    // crates/rondi-cli/tests/fuzz_regressions.rs); set RONDI_FUZZ_WIDE_LINES
    // to keep exploring that path.
    if std::env::var_os("RONDI_FUZZ_WIDE_LINES").is_none()
        && text.lines().any(|line| {
            line.strip_prefix("LINE")
                .and_then(|rest| rest.split(':').next())
                .and_then(|width| width.parse::<f64>().ok())
                .is_some_and(|width| !(width <= 64.0))
        })
    {
        return;
    }
    let root = rondi_fuzz::fresh_root();
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
    if mode & 1 == 0 {
        if mode & 2 != 0 {
            args.push("--json".to_owned());
        }
        let _ = rondi_fuzz::cli::fuzz_xport(&args);
    } else {
        let format = ["JSON", "XML", "PNG", "JSONTIME"][usize::from(mode >> 1) % 4];
        args.splice(0..0, ["--imgformat".to_owned(), format.to_owned()]);
        let _ = rondi_fuzz::cli::fuzz_graph(&args);
    }
});
