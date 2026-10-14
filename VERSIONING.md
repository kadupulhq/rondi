# Versioning

Rondi follows [Semantic Versioning 2.0.0](https://semver.org/). The workspace
crates (`rondi`, `rondi-cli`, `rondi-server`) share one version and are
released together. Releases are tagged `vMAJOR.MINOR.PATCH`, and the tag is
the only source of truth for what a release contains.

## What the version number covers

1. **Rondi's own interfaces.** The `rondi` command line (subcommands, flags,
   exit codes, stdout), the `/v1` HTTP API, and the `.rondi` file format and
   its `version` field.
2. **The RRDtool-compatible surfaces.** The `.rrd` files Rondi reads and
   writes, the `rrdtool` and `rrdcached` command lines, and the rrdcached
   protocol. Their contract is the behavior of the pinned RRDtool release
   (1.11.0), not a Rondi design. A change that brings Rondi closer to that
   release is a fix, even when output changes. A change of the pinned
   release is a major release.

The Rust library API of the `rondi` crate is not covered before `v1.0.0`
and can change in any release. Internal modules, file layout, log format,
and benchmarks are never covered.

## What forces a major release

- Removing or renaming a `rondi` subcommand, flag, or HTTP endpoint, or
  changing what an existing one does.
- A `.rondi` format change that older releases cannot read.
- Moving to a different pinned RRDtool release.
- Raising the minimum supported Rust version for building from source in a
  way that drops a supported distribution toolchain.

## What forces a minor release

New subcommands, flags, endpoints, or newly supported RRDtool commands,
options, data source types, or consolidation functions.

## What is a patch

Bug fixes, security fixes, and compatibility fixes toward the pinned
RRDtool release.

## Pre-1.0

Until `v1.0.0` there is no compatibility promise for Rondi's own interfaces.
The minor number carries breaking changes, which is what semantic versioning
specifies for a zero major.

## Commits and releases

Commits follow [Conventional Commits](https://www.conventionalcommits.org/).
`feat:` implies a minor, `fix:` a patch, and a `!` or a `BREAKING CHANGE:`
footer implies a major. The mapping is a guide for the maintainer cutting the
release, not an automation that tags on its own.
