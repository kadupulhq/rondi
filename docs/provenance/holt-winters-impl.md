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
  RRDtool in output, exit status and bytes. The Task 6 review ran 264
  random series on all 22 `hw` inputs: 253 matched, and the 11 that did
  not (all `hwnan`) were specified and fixed in Task 6b, whose reruns on
  `hwnan` and `mhwsm10` matched 288 of 288.
- Review: separate sessions, black-box only.

## Commits (`git log 47cca1f..HEAD`)

`124f620`, `6a91879`, `8c90ecb` (specification and vectors);
`ce9f929` (raw words), `29228af` (info), `59d5a8c` (dump);
`03d0865`, `a3b3a0b`, `2c17daa` (runs of one PDP, refusal reporting,
HW-17 wording); `20d830b`, `ef7ed2a` (runs and smoothing pass);
`ebc1ed1`, `93c8be7`, `cd870be` (unknown intercept and slope,
non-positive divisors, window below the period).

## Vectors

`cases.tsv` holds 217 cases: 111 `same`, 100 `unsupported` (all `tune`;
promoted to `same` by the create and tune work below),
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

## Create and tune (0.2)

- Branch: `feat/holt-winters-create-tune`, based on `origin/main` at
  `1fadbd7`; plan `docs/superpowers/plans/2026-11-10-holt-winters-create-tune.md`.
- Method: one inline session, from `create.md` section 8 (CR-32 to
  CR-36, CR-52), `holt-winters.md` HW-1, HW-2 and HW-30 to HW-35,
  `tune-modify.md` TM-4 and the `create`, `hw` and `rrdcached` vectors.
  The gaps (field readers, check order, implicit group links, template
  state, history byte counts, which archives each option tunes, the
  version raise) were settled by black-box runs of RRDtool 1.11.0 on
  macOS arm64 and the `rondi-oracle` Linux x86_64 and aarch64 images,
  written into the specification, then into vectors, then into code.
- Code: `crates/rondi/src/rrd_definitions.rs` (Holt-Winters archive
  definitions), `rrd_create.rs` (layout, HW-1 state, templates),
  `hw_tune.rs` (new; HW-30 to HW-35), the CLI `tune` arms and rrdcached's
  `TUNE` (`crates/rondi-server/src/rrdcached/tune.rs`), which share the
  library code.
- HW-2 choice: Rondi stores `seasonal_smooth_idx` 0, where RRDtool stores
  a value that depends on the creation time; the `create` vectors mask
  that slot.
- CR-35 rejections: RRDtool accepts four malformed archive definitions
  and writes files it cannot use (a zero period, links to missing
  archives, extra fields). Rondi rejects them with its own errors
  (`create.md` CR-div-hw-defects); cases 317, 318 and 332 are
  `clean-error`, and case 314, whose RRDtool result is not
  deterministic, stays `undefined` with a unit test of the rejection.
- Vectors after the change: `create` cases with Holt-Winters archives are
  `same` except the CR-35 ones; the 100 `hw` `tune` cases are `same`
  (211 `same`, 6 `clean-error`, 0 `unsupported`, 0 `divergent`);
  rrdcached `create-046` expects RRDtool's success, and
  `tune_holt_winters_options_like_rrdcached` compares 14 `TUNE` sessions
  with upstream rrdcached.
- Commits: `847c74c` (specification and vectors), `40ed51c` (create),
  `7f23f5a` (tune options and rrdcached `TUNE`).
- Divergent cases: none beyond the CR-35 rejections and HW-2.
- Exposure: none. No context opened RRDtool C source.

## Remaining refusals

- `tune` structure changes (`DS:`, `RRA:`, `DEL:`, `DELRRA:`, `RRA#`,
  `--step`) on files with Holt-Winters archives, or adding them (TM-60).
- `update` of files whose prediction, SEASONAL or DEVSEASONAL archive
  links to an archive of the wrong type (`rrdtool create` can produce a
  misnumbered index, so these are not only crafted).

## Verification

`python3 -I scripts/check-provenance.py` and
`cargo test -p rondi-cli --test hw_spec_vectors` with
`RONDI_REQUIRE_ORACLE=1`; results in the task 7 report.
