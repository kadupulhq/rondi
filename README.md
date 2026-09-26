# Template

Starting point for a new repository in this organisation. Create from it with
**Use this template**, or:

```
gh repo create kadupulhq/NAME --private --template kadupulhq/template
```

What you get: the versioning policy, a seeded changelog, the release workflow
that publishes a tag as a GitHub release, Dependabot for actions, editor and
attribute config, the pull request template, and CODEOWNERS.

What you add: the language CI workflow, and the matching Dependabot ecosystem
in `.github/dependabot.yml`.

CodeQL scans GitHub Actions workflows initially. When adding application code,
add its supported languages to the matrix in `.github/workflows/codeql.yml`.
Enable the dependency graph in repository security settings for dependency review.

Community health files (contributing, conduct, security, issue forms) come from
`kadupulhq/.github` and do not need copying. Add one here only to override it.

Replace this README with the real one.
