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

/// rrd_create.c:1136 reads `%18[^:]` as bytes, so a bound whose 18th byte
/// falls inside a multibyte character is cut mid-character. A cut min no
/// longer meets its ':' and a cut max fails strtod; both are errors, not a
/// slice panic.
#[test]
fn create_with_bound_cut_inside_a_character_is_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("cut.rrd");
    let create = |bounds: &str| {
        rondi::create_rrd_file(
            &path,
            1_000_000_000,
            10,
            &[format!("DS:x:GAUGE:20:{bounds}")],
            &["RRA:AVERAGE:0.5:1:8".to_owned()],
            true,
        )
        .unwrap_err()
        .to_string()
    };
    assert_eq!(
        create("00000000000000000\u{e9}:U"),
        "failed to parse data source 20:00000000000000000\u{e9}:U: failed to extract min:max"
    );
    let error = create("U:00000000000000000\u{e9}");
    assert!(error.contains("parsing max val"), "{error}");
    assert!(!path.exists());
}
