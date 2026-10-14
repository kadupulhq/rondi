//! A client byte stream through the rrdcached connection loop.
//! First byte selects whether a `-P` permission list is active.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, input)) = data.split_first() else {
        return;
    };
    let permissions = (mode & 1 == 1).then(|| {
        ["UPDATE", "FLUSH", "BATCH", "FETCH", "INFO", "PENDING"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    let root = rondi_fuzz::fresh_root();
    rondi_fuzz::runtime().block_on(rondi_fuzz::server::fuzz_rrdcached(
        &root,
        input,
        permissions,
    ));
});
