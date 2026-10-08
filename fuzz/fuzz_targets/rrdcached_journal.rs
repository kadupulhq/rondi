//! Arbitrary journal contents through daemon start-up replay and flush.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let root = rondi_fuzz::fresh_root();
    // Journals name absolute paths; let inputs say ROOT instead.
    let text = String::from_utf8_lossy(data).replace("ROOT", &root.display().to_string());
    rondi_fuzz::server::fuzz_journal(&root, text.as_bytes());
});
