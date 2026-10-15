// crates.io packages only contain files under the crate directory, so each
// crate carries copies of the root license files. This keeps the copies from
// drifting. The tests directory is not packaged, so the test only runs in the
// workspace.
use std::path::Path;

const FILES: [&str; 4] = [
    "COPYRIGHT",
    "LICENSE-APACHE",
    "LICENSE-GPL-2.0",
    "LICENSE-MIT",
];
const CRATES: [&str; 3] = ["rondi", "rondi-server", "rondi-cli"];

#[test]
fn crate_license_files_match_the_workspace_root() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    for name in FILES {
        let expected = std::fs::read(root.join(name)).unwrap();
        for krate in CRATES {
            let copy = root.join("crates").join(krate).join(name);
            let actual =
                std::fs::read(&copy).unwrap_or_else(|err| panic!("{}: {err}", copy.display()));
            assert!(
                actual == expected,
                "{} differs from {name} at the repository root; copy it again",
                copy.display()
            );
        }
    }
}
