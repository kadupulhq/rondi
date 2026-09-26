# Versioning

Kadupul follows [Semantic Versioning 2.0.0](https://semver.org/). Releases are
tagged `vMAJOR.MINOR.PATCH`, and the tag is the only source of truth for what a
release contains.

## What the version number covers

Semantic versioning only means something once you say what the public interface
is. For Kadupul it is these five things, and nothing else:

1. **The plugin API.** Hook names, their arguments, and the functions a plugin
   is expected to call.
2. **The database schema**, as far as a plugin or an external report reads it.
3. **The command line interface**: script names, their flags, exit codes, and
   the shape of what they print to stdout.
4. **The configuration file** and the settings stored in the database.
5. **The HTTP API**, once there is one.

The web interface, internal functions, file layout, and anything marked
experimental are not covered. They can change in a patch release.

## What forces a major release

- Removing or renaming a plugin hook, or changing what it is passed.
- A schema migration a plugin cannot survive without a code change.
- Removing a CLI flag, or changing what an existing one does.
- Raising the minimum PHP, MySQL or RRDtool version.
- Changing a default in a way that alters what gets polled or stored.

## What forces a minor release

New hooks, new CLI flags, new settings, new graph or data source types, and
anything else additive. A plugin written against the previous minor keeps
working.

## What is a patch

Bug fixes and security fixes that keep the interface identical.

## Security releases

A security fix ships as a patch on every supported branch. It is never bundled
with a feature, so an operator can take the fix without taking anything else.

## Pre-1.0

Until `v1.0.0` there is no compatibility promise. The minor number carries
breaking changes, which is what semantic versioning specifies for a zero major.

## Commits and releases

Commits follow [Conventional Commits](https://www.conventionalcommits.org/).
`feat:` implies a minor, `fix:` a patch, and a `!` or a `BREAKING CHANGE:`
footer implies a major. The mapping is a guide for the maintainer cutting the
release, not an automation that tags on its own. Releases are deliberate.
