//! `rrdtool update`/`updatev`/`create` argument vectors (one per line).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let root = rondi_fuzz::fresh_root();
    let args = text.lines().map(str::to_owned).collect::<Vec<_>>();
    match mode % 4 {
        0 => drop(rondi_fuzz::cli::fuzz_update(
            &root.join("g.rrd"),
            &args,
            false,
        )),
        1 => drop(rondi_fuzz::cli::fuzz_update(
            &root.join("c.rrd"),
            &args,
            true,
        )),
        2 => drop(rondi_fuzz::cli::fuzz_update(
            &root.join("d.rrd"),
            &args,
            false,
        )),
        _ => drop(rondi_fuzz::cli::fuzz_create(&root.join("new.rrd"), &args)),
    }
});
