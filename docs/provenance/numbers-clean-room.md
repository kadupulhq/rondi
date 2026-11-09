# Decimal numbers

- Branch: `feat/numbers-spec` (specification and vectors, workstream W5's
  first phase). Whether `crates/rondi/src/rrd_number.rs` and
  `crates/rondi-cli/src/cparse.rs` are later rewritten or reworded depends
  on maintainer decision D3 in `docs/provenance-audit.md`.
- Pull request: specification phase in this pull request.
- Replaces: nothing yet.

## Investigation context

- Who: an AI agent session (Claude) that also wrote the `COMPUTE` and
  Holt-Winters specifications.
- Read: the man pages `rrdupdate`, `rrdcreate` and `rrdtune`, and the
  specifications in `docs/compatibility/`, mainly `restore.md` RS-34 to
  RS-39a, `update.md`, `create.md` and `tune-modify.md`. It ran RRDtool
  1.11.0 on macOS arm64 and in the Linux aarch64 and x86_64 oracle
  containers, reading the resulting files' raw bytes. It read no RRDtool C
  source and no body of `rrd_number.rs` or `cparse.rs` (the W5 targets);
  of `rrd_number.rs` it read only the doc comments and signatures of
  `parse_rrd_number` and `rrd_strtodbl`.
- After deriving NM-6 from black-box runs, it read the `--minimum` and
  `--maximum` arm of `tune` in `crates/rondi-cli/src/main.rs` to locate
  Rondi's divergence. That arm is legacy code (`provenance-audit.md`,
  High tier). NM-6 is stated from the black-box results only.
- Produced: `docs/compatibility/numbers.md` NM-1 to NM-6, the `numbers`
  cases in `scripts/gen-create-resize-vectors.py`,
  `crates/rondi-cli/tests/data/numbers/` (196 cases, x86_64 overrides) and
  `crates/rondi-cli/tests/numbers_spec_vectors.rs`.
- Also changed: the `--minimum` and `--maximum` arm of `tune` now converts
  its bound with the public `rondi::parse_rrd_number`, the conversion
  `update` and `create` already use, instead of a second routine that
  differed on arm64 for long digit strings. No new conversion code was
  written.

## Exposure

None from RRDtool. The 40-character cap and the character set of NM-6 were
found by black-box runs before the Rondi arm was read.

## Verification

- `cargo test -p rondi-cli --test numbers_spec_vectors --test tune_modify_spec --test rrdtool_command_options`: 3, 14 and 4 passed on macOS
  arm64.
- A model written from RS-38 and RS-39 alone reproduces the 179
  conversion cases on macOS (fused) and on the Linux x86_64 overrides
  (separate arithmetic).
