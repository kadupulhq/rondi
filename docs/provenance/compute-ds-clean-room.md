# COMPUTE data sources

- Branch: `feat/compute-ds-impl`
- Pull request: see the pull request that adds this file
- Replaces: new feature (Rondi previously refused `DS:<name>:COMPUTE`)

## Investigation context

- Who: Claude Code session (rondi-23), working for the maintainer
- Read: `rrdcreate(1)` and `rrdgraph_rpn(1)`; Rondi's existing CDEF parser
  and the create, restore and rrdcached specifications; black-box runs of
  RRDtool 1.11.0 (Homebrew on macOS arm64, and the pinned Linux build on
  arm64 and x86_64). No RRDtool C source was inspected.
- Produced: `docs/compatibility/compute.md` CP-1 to CP-20; the `compute`
  vector suite (54 cases), COMPUTE cases appended to the `create` suite,
  and `walk-cdef-*` cases in the `restore` suite.

## Implementation context

- Who: a fresh Claude Code subagent in its own worktree, with no access to
  the investigation session's scratch files
- Given: `docs/compatibility/compute.md`, the vector suites above, the
  repository's AGENTS.md and CLAUDE.md, and the Rondi modules it had to
  change
- Did not open: RRDtool C source or summaries of it,
  `docs/rrd-source-notes.md`, or the investigation scratch files
- Produced: `crates/rondi/src/rpn/compute.rs`; changes to
  `rrd_definitions.rs`, `rrd_create.rs`, `rrd_binary.rs`, `rrd_restore.rs`
  and `rondi-cli/src/main.rs`. Further black-box probes refined CP-11 and
  CP-19 and added CP-21 and CP-22, each with vectors except CP-22 (an
  RRDtool crash).

## Exposure

None.

## Verification

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, and `(cd fuzz && cargo check)`: clean.
- `RONDI_REQUIRE_ORACLE=1 cargo test --workspace --no-fail-fast`: 55
  suites, 844 tests passed, 0 failed.
- Linux override vectors regenerated on arm64 and x86_64: no changes beyond
  the known nondeterministic `dup-database` restore case, whose committed
  override was kept.
- `scripts/check-provenance.py`: no errors; warnings only for the legacy
  `crates/rondi/src/graph.rs`, which PR #141 replaces.
