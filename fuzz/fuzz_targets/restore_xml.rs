//! Arbitrary text through `restore`, then read back what it wrote.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(xml) = std::str::from_utf8(data) else {
        return;
    };
    let out = rondi_fuzz::scratch().join("restored.rrd");
    let _ = std::fs::remove_file(&out);
    for range_check in [false, true] {
        if rondi::restore_rrd_file(xml, &out, true, range_check).is_err() {
            continue;
        }
        // A restored file must be readable by our own reader.
        let info = rondi::inspect_rrd_file(&out).expect("restore wrote an unreadable file");
        for archive in &info.archives {
            let _ = rondi::fetch_rrd_file(
                &out,
                &archive.consolidation,
                info.last_update.saturating_sub(3_600),
                info.last_update,
                1,
            );
        }
        let _ = rondi::dump_rrd_file(&out);
    }
});
