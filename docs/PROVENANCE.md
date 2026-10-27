# Provenance

Rondi is an independently implemented, behavior-compatible replacement for
RRDtool, licensed MIT OR Apache-2.0. RRDtool is licensed under the GPL, so its
C source is a behavioral reference of last resort, not an implementation
source. This document explains how code in this repository is expected to be
written so that its origin can be shown. The binding rules are in
[`AGENTS.md`](../AGENTS.md); this file gives the reasoning and the workflow.

This is engineering provenance hygiene. It is not a legal conclusion, and it
does not replace advice from a lawyer on any specific question.

## Development model

Behavior flows through five stages, in order:

1. **Documentation and specification.** Start from RRDtool's public manual
   pages, command help, file-format documentation, and published
   specifications.
2. **Black-box compatibility tests.** Run the RRDtool reference executable on
   chosen inputs and record what it does: output, exit status, error text,
   and the bytes of any RRD file it writes.
3. **Behavioral specification.** Write down the required behavior in
   implementation-neutral language, under `docs/compatibility/`.
4. **Independent Rust implementation.** Design and write the Rust code from
   the specification and the tests, using Rust idioms and Rondi's own
   architecture.
5. **Differential validation against RRDtool.** Feed the same inputs to
   RRDtool and to Rondi and compare only externally meaningful behavior. For
   files, compare the resulting RRD bytes where that is the contract. For
   graphs, compare values, dimensions, metadata, and output within documented
   tolerances.

## Reference-source inspection

Sometimes the documentation is silent and black-box testing cannot settle a
question in reasonable time, for example an edge case that depends on an
input you cannot easily construct. In that case a contributor may read the
relevant RRDtool C source to find out what the observable behavior is.

That reading answers one question: what does RRDtool do? It does not
authorize translating the C into Rust, porting functions line by line,
keeping RRDtool's control flow, data structures, internal names, or comments,
or copying error strings that are not part of the observable contract. The
outcome of an inspection is a behavioral requirement, written in neutral
language, plus a test where practical. The implementation is then written
from that requirement, not from the C.

Source inspection is an exception. It is not the normal way to develop Rondi.

## Separating investigation from implementation

AI tools make it easy to blur reading and writing, so the two are kept in
separate contexts:

- **Investigation context.** Starts from an observed incompatibility, works
  through documentation and black-box runs, and inspects RRDtool source only
  if needed. Its output is an implementation-neutral behavioral specification
  and a compatibility test. It produces no Rust implementation.
- **Implementation context.** A fresh session, or a different agent, that
  receives the behavioral specification and the tests. It never receives the
  corresponding RRDtool C, excerpts of it, or a summary that restates its
  structure. Do not paste RRDtool source into implementation prompts, and do
  not ask an LLM to translate it.

A human contributor following the same discipline writes the specification
first and implements from it, not from an open C file.

## Provenance comments

When code exists to match a specific RRDtool behavior, say so next to the
code, using one of these two forms:

```rust
// Compatibility: RRDtool treats an unknown DS as an update error.
// Provenance: black-box compatibility test.
```

```rust
// Compatibility: preserve RRDtool behavior for this edge case.
// Provenance: reference-source-inspection; see docs/compatibility/update.md.
```

The first line states the behavior. The second says how it was established
and, for source inspection, points to the specification. Do not include GPL
source excerpts, C function bodies, C line numbers, copied comments, or
translated pseudocode.

## Related documents

- [`docs/compatibility/`](compatibility/README.md): behavioral
  specifications, one per area. These are being written.
- [`docs/provenance-audit.md`](provenance-audit.md): an audit of code
  written before this policy. Modules whose documentation says they port
  RRDtool C are flagged there for review, not endorsed.
- [`docs/compatibility.md`](compatibility.md) and
  [`docs/COMPATIBILITY_DEFECTS.md`](COMPATIBILITY_DEFECTS.md): the current
  compatibility matrix and known gaps.

## When uncertain

Stop and flag the change for provenance review in the pull request. Do not
resolve licensing uncertainty by cosmetically rewriting code; renaming
variables or reordering statements does not change where the code came from.

## Contributor checklist

Every pull request carries this checklist:

- [ ] Implementation came from documentation/specification/test behavior.
- [ ] No RRDtool C code was translated.
- [ ] No GPL comments were copied.
- [ ] Internal architecture was independently designed.
- [ ] Compatibility quirks have tests.
- [ ] Source-inspection-derived behavior is documented.
- [ ] Implementation is idiomatic Rust rather than C-shaped Rust.
- [ ] Any uncertain provenance has been flagged.
