use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rondi"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn local_cli_create_update_fetch_persists_across_processes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("storage");
    let created = run(
        &root,
        &[
            "create",
            "temperature",
            "--step",
            "10",
            "--heartbeat",
            "25",
            "--rows",
            "6",
            "--start",
            "1700000000",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = run(&root, &["update", "temperature", "1700000010", "21.5"]);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let fetched = run(&root, &["fetch", "temperature"]);
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&fetched.stdout).unwrap();
    assert_eq!(json["points"][0]["value"], 21.5);
}
