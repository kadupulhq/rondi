# Holt-Winters info, dump and update (implementation)

- Branch: `feat/holt-winters-impl`, based on `origin/main` at `47cca1f`.
- Pull request: not yet opened.
- Replaces: new feature. Completes the implementation context of
  [`holt-winters-clean-room.md`](holt-winters-clean-room.md), which covers
  the specification phase.

## Method

Behavior-first, as in `AGENTS.md`. The specification
(`docs/compatibility/holt-winters.md`) and the `hw` vectors
(`crates/rondi-cli/tests/data/hw`) came first; each implementation task
received them and the task plan, and nothing else about RRDtool.
Gaps found during implementation went back to a black-box investigation,
then into the specification, then into a vector, and only then into code.

## Sources

- The specification sections HW-1 to HW-21 and HW-30 to HW-36, and
  `update.md`, `create.md`, `restore.md`.
- The published method (Brutlag, USENIX LISA 2000) and the man pages.
- Black-box runs of RRDtool 1.11.0 on macOS arm64 (and the Linux aarch64
  oracle container): output, exit status and file bytes.
- Independently constructed inputs: byte-patched copies of the committed
  `hw` inputs (versions, NaN parameters, window lengths, dependency
  links) and random series.

No RRDtool C source was opened by any context. No other session's model
or the old update path was read.

## Investigation and implementation separation

- Investigation (specification author, a separate session): HW-3 and
  HW-4 `info` and `dump` output, the HW-18 version rule, HW-5 window
  limit, HW-12a (non-positive divisors) and the unknown intercept and
  slope rules (HW-12), all black-box.
- Implementation (one fresh AI agent session per task, dispatched by the
  controller): tasks 2 to 6b, from the specification and vectors.
  Task 6 also probed the dependency links black-box, patching the link
  of one or two archives in the committed inputs: 262 runs (68
  single-link variants, 194 DEVPREDICT by FAILURES pairs) matched
  RRDtool in output, exit status and bytes. Task 6b's 264 random series
  matched after the HW-12 and HW-12a fixes.
- Review: separate sessions, black-box only.

## Commits (`git log 47cca1f..HEAD`)

`124f620`, `6a91879`, `8c90ecb` (specification and vectors);
`ce9f929` (raw words), `29228af` (info), `59d5a8c` (dump);
`03d0865`, `a3b3a0b`, `2c17daa` (runs of one PDP, refusal reporting,
HW-17 wording); `20d830b`, `ef7ed2a` (runs and smoothing pass);
`ebc1ed1`, `93c8be7`, `cd870be` (unknown intercept and slope,
non-positive divisors, window below the period).

## Vectors

`cases.tsv` holds 217 cases: 111 `same`, 100 `unsupported` (all `tune`),
6 `clean-error` (window lengths above 28, HW-5) and 0 `divergent`.
The Linux x86_64 overrides for cases 205 to 216 are pending: docker was
unavailable. The x86_64 CI job will arbitrate.

## Exposure

None known. No context saw RRDtool C source.

## Documented divergences

- Smoothing window f of 2 or more (crafted files only): RRDtool smooths
  with a result Rondi does not reproduce; Rondi leaves the archive
  unsmoothed.
- Damaged files: SEASONAL rows past the end of the file are read as
  unknown and the update fails with the `writing rrd` error of UP-38.
  Season moves in runs are not checked against the row count; unpinned.

## Remaining refusals

- `tune` on files with Holt-Winters archives (HW-36).
- `create` of Holt-Winters archives (CR-80).
- `update` of files whose prediction, SEASONAL or DEVSEASONAL archive
  links to an archive of the wrong type (`rrdtool create` can produce a
  misnumbered index, so these are not only crafted).

## Verification

`python3 -I scripts/check-provenance.py` and
`cargo test -p rondi-cli --test hw_spec_vectors` with
`RONDI_REQUIRE_ORACLE=1`; results in the task 7 report.
