//! Arbitrary bytes as an .rrd file through every read path, then one update.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let path = rondi_fuzz::scratch().join("in.rrd");
    std::fs::write(&path, data).expect("write");
    let Ok(info) = rondi::inspect_rrd_file(&path) else {
        return;
    };
    let last = info.last_update;
    let step = info.step;
    for (index, archive) in info.archives.iter().enumerate() {
        let _ = rondi::first_rrd_time(&path, index);
        for (start, end, resolution) in [
            (last.saturating_sub(3_600), last, 1),
            (last.saturating_sub(86_400), last.saturating_add(600), step),
            (i64::MIN, i64::MAX, u64::MAX),
            (last, last, 0),
        ] {
            let _ = rondi::fetch_rrd_file(&path, &archive.consolidation, start, end, resolution);
        }
    }
    let _ = rondi::first_rrd_time(&path, usize::MAX);
    for header in [rondi::RrdDumpHeader::None, rondi::RrdDumpHeader::Xsd] {
        if let Ok(xml) = rondi::dump_rrd_file_with_header(&path, header) {
            // What dump emits, restore must at least not crash on.
            let out = rondi_fuzz::scratch().join("roundtrip.rrd");
            let _ = rondi::restore_rrd_file(&xml, &out, true, true);
        }
    }
    let values = info
        .data_sources
        .iter()
        .map(|_| Some("12345"))
        .collect::<Vec<_>>();
    let _ = rondi::update_rrd_raw_values_verbose(&path, last.saturating_add(step as i64), &values);
    let _ = rondi::update_rrd_raw_values_precise(&path, last.saturating_add(1), 500_000, &values);
    let _ = rondi::fetch_rrd_file(&path, "AVERAGE", last - 600, last + 600, 1);
    let resized = rondi_fuzz::scratch().join("resize.out");
    let _ = std::fs::remove_file(&resized);
    let _ = rondi::resize_rrd_file(&path, &resized, 0, rondi::RrdResizeAction::Grow, 3);
    let _ = std::fs::remove_file(&resized);
    let _ = rondi::resize_rrd_file(&path, &resized, 0, rondi::RrdResizeAction::Shrink, 1);
});
