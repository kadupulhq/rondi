# Agent instructions

These rules apply to every human and AI contributor working in this
repository. `docs/PROVENANCE.md` explains the development model behind them.

## Independent implementation and licensing rules

This project is an independently implemented, behavior-compatible replacement
for RRDtool. Target license: MIT OR Apache-2.0. RRDtool's GPL implementation is
a behavioral reference of last resort, not an implementation source.

### General rule

Implement observable behavior, not RRDtool's implementation. Allowed primary
sources:

- public documentation
- published specifications
- command help
- file-format documentation
- black-box execution of RRDtool
- independently constructed test vectors
- compatibility tests
- observed inputs and outputs

### Reference source inspection

RRDtool C source may be inspected only when externally observable behavior
cannot reasonably be determined from documentation or black-box testing. When
source inspection is necessary:

1. Determine the behavioral requirement.
2. Record that requirement in implementation-neutral language.
3. Add a compatibility test where practical.
4. Implement the behavior independently and idiomatically in Rust.

Do not:

- translate C into Rust
- port functions line by line
- ask an LLM to translate RRDtool source
- copy comments
- copy internal names unless required by a public interface
- copy implementation-specific data structures
- preserve C control flow merely because RRDtool uses it
- copy error strings unless exact text is required for compatibility
- copy non-interface implementation details
- paste RRDtool C source into implementation prompts
- use source inspection as the normal development method

### AI separation rule

When investigating RRDtool C source, separate investigation from
implementation. Workflow:

observed incompatibility -> documentation / black-box investigation ->
reference-source inspection only if necessary -> implementation-neutral
behavioral specification -> compatibility test -> fresh implementation
context -> independent Rust implementation

The implementation agent should receive the behavioral specification and
tests, not the corresponding RRDtool C implementation.

### Compatibility testing

Prefer differential black-box testing: give the same input to the RRDtool
reference executable and to the Rust implementation, and compare only
externally meaningful behavior. Binary compatibility tests may compare
resulting RRD files where appropriate.

### Graph compatibility

Prefer comparing calculated values, dimensions, metadata, graph commands, and
pixel or output behavior using documented tolerances. Do not reproduce
graphing implementation internals merely to obtain identical output.

### Provenance

New compatibility behavior discovered through reference-source inspection must
be documented without copying source expression. Use one of these comment
forms:

```rust
// Compatibility: RRDtool treats an unknown DS as an update error.
// Provenance: black-box compatibility test.
```

```rust
// Compatibility: preserve RRDtool behavior for this edge case.
// Provenance: reference-source-inspection; see docs/compatibility/update.md.
```

Do not include GPL source excerpts, C line numbers, copied comments, or
translated pseudocode.

### When uncertain

Stop and flag the implementation for provenance review. Do not solve
licensing uncertainty by cosmetically rewriting code.

### Existing code

Code written before this policy is being audited; results go in
docs/provenance-audit.md. Legacy modules and notes whose documentation says
they port RRDtool C (for example, parts of `docs/rrd-source-notes.md`) are
flagged for that audit, not endorsed as a model for new work.

### Contributor checklist

- [ ] Implementation came from documentation/specification/test behavior.
- [ ] No RRDtool C code was translated.
- [ ] No GPL comments were copied.
- [ ] Internal architecture was independently designed.
- [ ] Compatibility quirks have tests.
- [ ] Source-inspection-derived behavior is documented.
- [ ] Implementation is idiomatic Rust rather than C-shaped Rust.
- [ ] Any uncertain provenance has been flagged.
