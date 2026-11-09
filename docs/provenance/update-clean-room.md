# Update path (Workstream 4)

- Branch: `feat/update-spec` (specification and vectors); the implementation
  follows on its own branch and completes this record.
- Pull request: specification phase in this pull request; implementation to
  follow.
- Replaces: the `update` path in `crates/rondi/src/rrd_binary.rs`, from
  argument checks through PDP and CDP state to row writes.

## Investigation context

- Who: an AI agent session (Claude), dispatched by the controller session
  for this workstream, separate from the later implementation context.
- Read: the man pages `rrdupdate`, `rrdcreate`, `rrd_pdpcalc`, `rrdtutorial`,
  `rrd-beginners`, `rrdinfo`, `rrddump`, `rrdfetch`, and the specifications
  in `docs/compatibility/`, including `create.md`, `time-spec.md`, `cli.md`
  and `compute.md`. It ran RRDtool 1.11.0 on macOS arm64, in the Linux
  aarch64 oracle container and in the Linux x86_64 oracle container
  (emulated), plus the Rondi binary as a black box. It used the existing
  vector generators and tests as patterns. It read no RRDtool C source, and
  it did not read the bodies or history of the update-path functions in
  `rrd_binary.rs`.
- Produced: `docs/compatibility/update.md` UP-1 to UP-36, a CLI-69
  correction in `docs/compatibility/cli.md`, `scripts/gen-update-vectors.py`,
  `crates/rondi-cli/tests/data/update/` (150 vectors on 36 seeds) and
  `crates/rondi-cli/tests/update_spec_vectors.rs`.

## Implementation context

Not started. The implementation pull request fills in this section.

## Exposure

- The first draft of `update.md` used four term names that coincide with
  names RRDtool uses internally. The investigation agent reports that they
  came from its prior familiarity with RRDtool's vocabulary, not from any
  file read in the session. An independent reviewer flagged them, and they
  were replaced with neutral terms defined by meaning before this pull
  request was published.
- While checking for those names, the independent reviewer of the
  specification grepped the repository. The results showed about 15 lines
  from the update-path functions in `rrd_binary.rs`, including variable
  names and one arithmetic line. The reviewer read no further and based no
  finding on them: its findings come from its own black-box runs. The
  reviewer does not take part in the implementation, and nothing it saw is
  passed to the implementation context.

## Verification

- `RONDI_REQUIRE_ORACLE=1 cargo test -p rondi-cli --test update_spec_vectors`:
  3 passed on macOS arm64, and in the Linux aarch64 and x86_64 (emulated)
  oracle containers. Rondi has no known divergences in these vectors.
- `python3 scripts/gen-update-vectors.py` reruns without changes.
- `python3 -I scripts/check-provenance.py`: no findings.
