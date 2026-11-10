# Update path (Workstream 4)

- Branches: `feat/update-spec` (specification and vectors) and
  `feat/update-clean-room` (implementation and this record).
- Pull request: specification phase in the earlier pull request;
  implementation in the pull request for `feat/update-clean-room`.
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
- Produced: `docs/compatibility/update.md` UP-1 to UP-36 (UP-37 and UP-38
  were added during implementation, see below), a CLI-69
  correction in `docs/compatibility/cli.md`, `scripts/gen-update-vectors.py`,
  `crates/rondi-cli/tests/data/update/` (150 vectors on 36 seeds) and
  `crates/rondi-cli/tests/update_spec_vectors.rs`.

## Implementation context

Implementation contexts were separate from the investigation context.
Each received the behavioral specification, the vectors and tests, and the
task plan, and was told not to open RRDtool C source or the bodies and
history of the old update path.

- Who: AI agent sessions (Claude), one per task, dispatched by the
  controller session.
- Given: `docs/compatibility/update.md` (UP-1 to UP-38),
  `docs/compatibility/compute.md` CP-10 to CP-16, `docs/compatibility/create.md`,
  the vectors and tests, and the plan.
- Not opened: RRDtool C source (no implementation context read any), and the
  bodies and history of the old update-path items in `rrd_binary.rs`.
  `rrd_number.rs` (W5 legacy) was used by signature and documented contract
  only, for its decimal conversion and big-integer difference functions.
- Produced: `crates/rondi/src/update/` with `amount.rs`, `pdp.rs`,
  `consolidate.rs`, `arguments.rs` and `mod.rs`, written from the
  specification. The public update entry points are re-exported from it
  with their existing signatures.
- Removal: the old update path (about 30 items, about 1,237 lines of
  `rrd_binary.rs`) was deleted from a signature listing, without its bodies
  being read. `require_rows_in_file` stays in `rrd_binary.rs`; the update
  path no longer uses it.

### Specification changes during implementation

The Task 5 implementer found six behaviors through the live-oracle tests
and its own black-box probes. The specification author verified each
independently by black-box runs: four held as stated and two needed
corrections. The author also corrected the UP-20 lead formula and added
UP-37 (step-count width) and UP-38 (damaged files), in commits `533def9`
and `8a9a832`. The UP-8 example was corrected in `84038dc`.

### Documented divergences

- A COMPUTE source whose stored program does not decode evaluates to unknown
  in Rondi. RRDtool 1.11.0 fails the update with an RPN error, or crashes
  when the end marker is missing. This arises only with crafted files.
- Additive public API: `update_rrd_text_quiet` and
  `update_rrd_text_file_quiet`.
- The numeric entry points (`update_rrd_values*`) format each value as
  `{x}` when that fits in 29 bytes and in exponent form otherwise. RRDtool
  has no numeric API, so no compatibility choice was involved.

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

Implementation-phase exposures:

- The Task 1 implementer saw a few body lines of `rrd_number.rs`
  (`rrd_strtodbl` and `rrd_diff`). That module is W5 legacy. The
  `amount.rs` code uses those functions only by signature and documented
  contract.
- During Task 5, a `sed` command aimed at `require_rows_in_file` also
  printed four closing lines of the deleted row-writing function (closing
  braces and the final `Ok(())`) and the one-line doc comment of the deleted
  end-of-file predicate, which states when the last queued row ends past the
  file and writing fails. The damaged-file behavior (UP-38) was then
  derived and verified by black-box probes and the specification author's
  independent oracle runs, not from those lines.
- No RRDtool C source was read by any implementation context.

## Verification

- `RONDI_REQUIRE_ORACLE=1 cargo test -p rondi-cli --test update_spec_vectors`:
  3 passed on macOS arm64, and in the Linux aarch64 and x86_64 (emulated)
  oracle containers. Rondi has no known divergences in these vectors.
- `python3 scripts/gen-update-vectors.py` reruns without changes.
- `python3 -I scripts/check-provenance.py`: no findings.
