# Clean-room workstream records

Each clean-room workstream adds one file here, named after its branch (for
example `graph-engine-clean-room.md`), in the same pull request as the code.
The record states who worked in each context and what each one read, so a
later audit can check the separation in [`AGENTS.md`](../../AGENTS.md)
against a written account instead of memory.

Write what happened, not what should have happened. If the implementation
context saw something it should not have, say so in "Exposure" and flag the
pull request for provenance review.

## Template

```markdown
# <workstream name>

- Branch: `<branch>`
- Pull request: #<n>
- Replaces: `<old module paths>`, or "new feature"

## Investigation context

- Who: <person, or AI tool and session identifier>
- Read: <docs, man pages, oracle runs, vector scripts; name any RRDtool C
  source inspected, by file only, and the spec section it settled>
- Produced: `<spec path>` sections <ids>, `<vector file>` (<n> vectors)

## Implementation context

- Who: <a different person, or a fresh AI session>
- Given: the spec sections and vector files above, and <anything else>
- Did not open: the module being replaced, its `git log -p`, RRDtool C source
- Produced: `<new module paths>`

## Exposure

<"None." or what was seen, when, and how it was handled>

## Verification

<commands run and their results; the provenance check output>
```
