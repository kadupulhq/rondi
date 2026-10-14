# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](VERSIONING.md).

## [Unreleased]

### Added

- Nothing yet.

### Fixed

- Check Unix socket directory ownership and shared-write permissions for unprivileged daemons as well as root.
- Publish the JSON daemon socket from private staging with mode `0600` already set. JSON library startup now uses `prepare_server` before threads and async `run` afterwards.
- Keep daemon RRD reads on checked `O_NOFOLLOW` descriptors through inspection, fetch, and dump; reuse checked descriptors for update and tune metadata.

- Limit the initial CodeQL matrix to workflow analysis until application source is added.
