//! Raw HTTP/1 request bytes through the JSON API service.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let root = rondi_fuzz::fresh_root();
    rondi_fuzz::runtime().block_on(rondi_fuzz::server::fuzz_http(&root, data));
});
