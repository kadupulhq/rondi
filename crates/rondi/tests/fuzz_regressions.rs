//! Inputs found by the cargo-fuzz targets in `fuzz/`.

/// fuzz: rrd_file seed `386_gauge_v3.rrd` (rrdtool 1.7.2 on i386, where the
/// float cookie sits at offset 12). rrd_open.c:465-470 checks the float
/// cookie before it reads the 64-bit counts, so RRDtool 1.11.0 reports the
/// foreign architecture instead of a layout error.
#[test]
fn foreign_architecture_file_fails_on_the_float_cookie() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("i386.rrd");
    std::fs::write(
        &path,
        include_bytes!("../../../fuzz/seeds/rrd_file/386_gauge_v3.rrd"),
    )
    .unwrap();
    assert_eq!(
        rondi::inspect_rrd_file(&path).unwrap_err().to_string(),
        "This RRD was created on another architecture"
    );
}
