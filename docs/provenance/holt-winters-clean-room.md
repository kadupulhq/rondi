# Holt-Winters archives

- Branch: `feat/holt-winters-spec` (specification and vectors); the
  implementation follows on its own branch after the update-path rewrite
  and completes this record.
- Pull request: specification phase in this pull request; implementation to
  follow.
- Replaces: new feature. Rondi refuses Holt-Winters archives today
  (`create.md` CR-80), so there is no existing module.

## Investigation context

- Who: an AI agent session (Claude) that also wrote the `COMPUTE`
  specification. It will not write the Holt-Winters implementation, which
  goes to a fresh context.
- Read: the man pages `rrdcreate`, `rrdtune`, `rrdgraph`, `rrdupdate`,
  `rrdinfo` and `rrddump`, the published method (Brutlag, USENIX LISA
  2000), and the specifications in `docs/compatibility/`, mainly
  `create.md`, `restore.md` and `update.md`. It ran RRDtool 1.11.0 on macOS
  arm64 and in the Linux aarch64 and x86_64 oracle containers, reading the
  resulting files' raw bytes, `info` and `dump`. It read no RRDtool C
  source. Rondi has no Holt-Winters update code to read.
- Produced: `docs/compatibility/holt-winters.md` HW-1, HW-2 and HW-10 to
  HW-19, the `hw` cases in `scripts/gen-create-resize-vectors.py`,
  `crates/rondi-cli/tests/data/hw/` (76 cases, x86_64 overrides) and
  `crates/rondi-cli/tests/hw_spec_vectors.rs`.

## Implementation context

Not started. The implementation pull request fills in this section.

## Exposure

None. Slot and parameter names (`intercept`, `seasonal_smooth_idx`,
`init_flag` and so on) are those printed by `info` and `dump` and already
used in `restore.md` RS-53.

## Verification

- `cargo test -p rondi-cli --test hw_spec_vectors`: 3 passed on macOS
  arm64 with the oracle on `PATH`.
- `scripts/gen-create-resize-vectors.py overrides` in the Linux oracle
  containers: no aarch64 overrides, 42 x86_64 overrides (committed).
- The black-box model check described in `holt-winters.md` "Vectors".
