## What this changes

<!-- One or two sentences. What behaviour is different after this merges? -->

## Why

<!-- The problem being solved. Link the issue if there is one. -->

## Verification

<!-- Commands you ran and what they printed. A claim without output is not evidence. -->

```
```

## Notes for the reviewer

<!-- Risk, rollback, anything upstream Cacti does differently. Delete if empty. -->

---

- [ ] Commits are signed off (`git commit -s`)
- [ ] Tests cover the changed behaviour, including the failure paths
- [ ] No security boundary is widened

## Provenance

See [docs/PROVENANCE.md](../docs/PROVENANCE.md).

- [ ] Implementation came from documentation/specification/test behavior.
- [ ] No RRDtool C code was translated.
- [ ] No GPL comments were copied.
- [ ] Internal architecture was independently designed.
- [ ] Compatibility quirks have tests.
- [ ] Source-inspection-derived behavior is documented.
- [ ] Implementation is idiomatic Rust rather than C-shaped Rust.
- [ ] Any uncertain provenance has been flagged.
