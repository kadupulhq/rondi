# Rondi

[![Security](https://github.com/kadupulhq/rondi/actions/workflows/security.yml/badge.svg?branch=main)](https://github.com/kadupulhq/rondi/actions/workflows/security.yml)
[![CodeQL](https://github.com/kadupulhq/rondi/actions/workflows/codeql.yml/badge.svg?branch=main)](https://github.com/kadupulhq/rondi/actions/workflows/codeql.yml)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange.svg)](https://github.com/kadupulhq/rondi/pulls)

Rondi is a Rust implementation project to replace RRDtool/librrd, `rrdcached`, and Cacti RRDProxy for the Kadupul monitoring platform.

> **Status:** Rondi is not a drop-in replacement. The default branch still contains repository scaffolding; the implementation is being developed in the [open pull request series](https://github.com/kadupulhq/rondi/pulls). Compatibility claims remain limited to behavior verified against pinned RRDtool 1.11.0.

## Project goals

- Match existing RRDtool behavior and preserve interoperability with `.rrd` files.
- Keep storage, data-source semantics, consolidation, queries, and compatibility logic in a reusable Rust library.
- Provide one executable with `rrdtool`, `rrdcached`, and RRDProxy compatibility entry points.
- Keep Kadupul's Symfony/PHP application and user workflows separate from Rust monitoring and time-series processing.
- Keep GraphQL optional and graph rendering independently scoped.

## Implementation under review

The current implementation work is in [PR #27](https://github.com/kadupulhq/rondi/pull/27), stacked on earlier foundation and compatibility pull requests. It includes a Rust workspace with a storage library, CLI, and server; a native `.rondi` format; and a partial compatibility path for existing `.rrd` files.

The `.rondi` format is distinct from `.rrd`. The compatibility executable updates supported `.rrd` files in place, but broad RRDtool command, graph, daemon protocol, RRDProxy protocol, and C library compatibility are still incomplete or unverified. See the pull request series for the current compatibility matrix and defects list.

## Try the implementation branch

Use GitHub CLI to check out the current stacked implementation branch, then build the executable:

```sh
gh pr checkout 27
mise exec -- cargo build --release -p rondi-cli --locked
```

Run basic native storage commands:

```sh
./target/release/rondi create temperature --step 10 --heartbeat 25 --rows 6 --start 1700000000
./target/release/rondi update temperature 1700000010 21.5
./target/release/rondi update temperature 1700000020 22
./target/release/rondi fetch temperature
```

The full differential and workspace checks use a disposable Linux container with pinned RRDtool 1.11.0:

```sh
docker build --progress=plain -f docker/Dockerfile.test -t rondi-test .
```

## Architecture

The implementation is organized as one Cargo workspace and coordinated release:

- `rondi`: reusable storage, processing, and compatibility library.
- `rondi-cli`: the `rondi` executable and compatibility command entry points.
- `rondi-server`: daemon, journaling, buffering, and storage coordination.

The server coordinates access to managed storage through a versioned internal API. The local API uses HTTP/JSON over a Unix socket. Remote access, rendering, GraphQL, and a C ABI have separate compatibility and deployment requirements; none should be assumed from the current storage implementation.

## Security and dependencies

The default branch runs Semgrep and CodeQL analysis for GitHub Actions. The implementation pull request adds RustSec `cargo-audit` and `cargo-deny` checks for known Rust advisories, dependency licenses, and registry sources. The Cargo policy allows crates.io sources and warns about duplicate dependency versions.

## Releases

Version tags (`vMAJOR.MINOR.PATCH`, with optional SemVer prerelease or build metadata) use GitHub-generated release notes, grouped by the pull request labels in [.github/release.yml](.github/release.yml). Label pull requests with `enhancement`, `bug`, or `documentation` so they appear under the matching section; unlabeled changes appear under “Other Changes.”

## Contributing

Changes are reviewed through pull requests using the repository's [pull request template](.github/PULL_REQUEST_TEMPLATE.md). The current implementation is experimental; report compatibility differences with the exact RRDtool version, command, output, exit status, and a disposable `.rrd` fixture where possible.

## License

The implementation pull request declares GPL-2.0-only and includes the corresponding license file. That license is not yet present on the default branch; refer to the [implementation pull request](https://github.com/kadupulhq/rondi/pull/1) for the proposed project license until it lands.
