//! AT-style time specifications and numeric parsers.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = rondi_fuzz::scratch();
    let (spec, other) = text.split_once('\n').unwrap_or((text, "end-1h"));
    rondi_fuzz::cli::fuzz_time(spec, other);
});
