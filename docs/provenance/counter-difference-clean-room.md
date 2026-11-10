# COUNTER and DERIVE difference

- Branch: `fix/clean-room-counter-difference`
- Pull request: see the pull request that adds this file
- Replaces: `rrd_diff` in `crates/rondi/src/rrd_number.rs`

## Investigation context

- Who: an AI coding session (Claude Code), the same session as the
  implementation. The specification it worked from already existed.
- Read: `docs/compatibility/update.md` UP-12, UP-13 and UP-16,
  `docs/compatibility/numbers.md` NM-1 and NM-5, the `update` vector cases
  for UP-16, and black-box runs of `/opt/homebrew/bin/rrdtool` 1.11.0 on
  macOS arm64 (step-1 `COUNTER` and `DERIVE` files, two updates, last row
  read with `rrdtool dump`). No RRDtool C source was opened.
- Produced: `docs/compatibility/numbers.md` NM-7 (sign of a difference of
  two negative `DERIVE` readings).

## Implementation context

- Who: the same session. The separation relied on never reading the old
  function, not on a second context.
- Given: the spec sections above and the existing tests.
- Did not open: the body or doc comments of `rrd_diff`, the git history of
  those lines, RRDtool C source. The function was found with `grep` (signature
  line and the one call site only) and deleted by line range; the range end
  came from a brace-matching script that printed only line numbers.
- Produced: `decimal_text_difference` and its helpers in
  `crates/rondi/src/update/amount.rs`, with unit tests.

## Exposure

The session read the rest of `rrd_number.rs` and the old caller in
`update/amount.rs`. That caller carried a one-line comment saying the old
function documented a limit on digits; nothing else about the old function
was seen.

## Verification

See the pull request body: `cargo test` for `rondi`, `rondi-cli` and
`rondi-server` with `RONDI_REQUIRE_ORACLE=1`, the update, numbers, compute
and hw spec vector suites, `csrc_update`, `rrdtool_differential`, `cargo fmt
--check`, `cargo clippy --workspace --all-targets -- -D warnings`, the fuzz
crate check, and `python3 -I scripts/check-provenance.py`.
