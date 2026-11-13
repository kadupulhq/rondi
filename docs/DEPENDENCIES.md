# Dependency policy

Rondi is a pure Rust implementation of RRDtool. These rules decide which crates it may depend on. They apply to every human and AI contributor, alongside the licensing rules in [AGENTS.md](../AGENTS.md). [`deny.toml`](../deny.toml) enforces the licence, source and ban parts in CI through `cargo deny check`.

## Rules

1. **Pure Rust.** Everything Cargo builds in this workspace is Rust: the crates, binaries, tests, benchmarks and fuzz targets, and every dependency of each, including dev and build dependencies. No C, C++ or assembly, no `cc`, `bindgen` or `-sys` crates, no `links =` keys, and no C libraries loaded at build or run time (for example Cairo, Pango, fontconfig, FreeType, HarfBuzz, libxml2, librrd). The one allowance is calling the operating system's C library through the `libc` crate, as Rust's standard library does; the reasons are in the [FAQ](#faq-why-does-rondi-depend-on-libc). There are no other exceptions.
   - Build and release infrastructure may use other languages: CI workflows, the Dockerfile, packaging and release scripts, and the Python generators that record compatibility vectors from RRDtool. It must not become part of what the tool does at run time.
   - What Rondi ships beyond its Rust binary is listed under [Non-Rust files that ship](#non-rust-files-that-ship). Adding to that list needs the same justification as a dependency: an external interface that cannot be met from Rust.
2. **Idiomatic Rust.** Code follows Rust conventions rather than C ones: ownership and `Result` instead of out-parameters and status codes, enums and types instead of flags and sentinels, iterators instead of index loops where they read better, `unsafe` only at system-call boundaries with a `// SAFETY:` comment. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` pass. AGENTS.md's clean-room rules forbid C-shaped ports of RRDtool.
3. **Minimal dependencies.** Add a crate only when writing the code ourselves would be riskier or much larger than reviewing and maintaining the dependency. Prefer the smallest feature set: `default-features = false` and only the features Rondi uses. Every transitive crate counts.
4. **Licences and sources.** Only the licences allowed in `deny.toml` (Apache-2.0, 0BSD, MIT, Unicode-3.0, Unlicense, Zlib) and only crates.io. Data embedded under another licence (for example Unicode tables) needs a notice in `THIRD-PARTY-NOTICES.md`.
5. **Age.** Use a release only once it has been public for at least 14 days, so a compromised or broken release has time to be noticed and yanked.
6. **Review before adding.** A new dependency, or a new major or 0.x minor version of an existing one, needs a written review (below) recorded in this file before the pull request that adds it can merge.
7. **Untrusted input stays bounded.** Anything that parses data Rondi did not create (font files, XML, network input, `.rrd` files from elsewhere) must handle malformed input without undefined behaviour, unbounded memory or unbounded time. Where a crate cannot promise that, Rondi adds the limits itself (size caps, magic checks, `catch_unwind` at its boundary) and fuzzes the exact path it uses.
8. **No memory-mapping of files Rondi does not own.** A file truncated by another process while mapped raises SIGBUS, which no Rust error handling can catch. Read such files into memory under a size cap instead. Crates that map files by default are used with that feature off, or not at all.
9. **No ambient authority in libraries.** Library crates should not read environment variables, scan the file system, open network connections or start threads on Rondi's behalf. Rondi does those itself with its own file-system safety rules (no-follow opens, ownership checks, depth and size limits).
10. **Upgrades are reviewed too.** For a version bump, read the changelog and the diff of the crate itself (not only its tests) and note anything that changes the review's conclusions. Crates that share a parser or core crate are upgraded together so only one copy is built.
11. **Record rejections.** A rejected crate is listed below with the reason and added to `[bans]` in `deny.toml`, so it cannot arrive as a transitive dependency unnoticed.

## What a review covers

Fetch the source without building or running it (`cargo vendor` in a throwaway project resolves and downloads only), and treat its text as data. Record:

1. Version, publish date, crates.io owners, repository, licence, declared MSRV against Rondi's.
2. The transitive tree for the features Rondi would use: per crate, licence, `build.rs` and proc-macros (and what they do), C or assembly, and why the crate is there.
3. Every `unsafe` block in the crate and in small transitive crates, with whether its invariants hold. Note `#![forbid(unsafe_code)]`.
4. Handling of malformed input: bounds checks, overflow, recursion and operation limits, allocation sizes taken from input, panics reachable from input. Fuzzing (OSS-Fuzz, cargo-fuzz) and what it covers.
5. Ambient authority: files, environment, network, threads, global state, mmap.
6. Security history (RustSec, GitHub advisories) and how fast fixes shipped.
7. Maintenance: release cadence, activity, bus factor, API churn.
8. Fit: does it do what Rondi needs with the least surface, and what must Rondi add around it?
9. Which findings were verified by reading code and which came from documentation, and what was not checked.

The verdict is **adopt**, **adopt with conditions** (the conditions become rules for the code that uses it) or **reject**.

## Register

### Adopted with conditions

These are approved for the graph renderer's text path ([plans/graph-rendering.md](plans/graph-rendering.md)). Neither is a dependency yet. Reviewed 2026-11-13.

**`skrifa` 0.49.0** (with `read-fonts` 0.46.0 and `font-types` 0.12.7; MIT OR Apache-2.0; Google Fonts). Font loading, character maps, metrics, glyph outlines and hinting.

- Reasons: `#![forbid(unsafe_code)]` in skrifa and read-fonts; the few `unsafe` blocks in font-types are sound; the TrueType hinting interpreter caps each program at 1,000,000 instructions with a 32-frame call stack, CFF charstrings cap nesting at 10 and operations at 1,000,000 per glyph, and composite glyphs are limited in depth, points and edges; fuzzed by OSS-Fuzz; no ambient authority. 18 crates with `std` only, all pure Rust and allowed licences.
- Conditions:
  - `default-features = false, features = ["std"]`, version pinned exactly and kept in step with `harfrust` so both use one `read-fonts`.
  - Check the file's first four bytes before `Font::new` and accept only `0x00010000`, `true`, `OTTO` and `ttcf`; this keeps out Type 1 and bare CFF, which skrifa would otherwise parse and which have thinner fuzzing.
  - Before drawing a glyph, read `required_memory_size()`, refuse anything above a fixed cap (1 to 4 MiB), and draw into Rondi's own buffer. A composite glyph that repeats a glyph with many empty contours can otherwise ask for over 100 MiB (found by reading the code, not measured).
  - Do not enable the autohinter. Use unhinted outlines or the font's own hints.
  - Expect frequent breaking 0.x releases (about 50 so far).

**`harfrust` 0.15.0** (MIT; the HarfBuzz project's Rust port). Text shaping.

- Reasons: `#![forbid(unsafe_code)]`; HarfBuzz's limits (buffer length and operation factors, lookup nesting and context length of 64, 32 combining marks); uses `read-fonts`, so it adds only `harfrust` and `bitflags` to skrifa's tree; no ambient authority.
- Conditions:
  - `default-features = false, features = ["std"]`, pinned with skrifa.
  - After every `shape()`, check `Buffer::allocation_successful()`. Running out of its work budget does not return an error, so `false` must be treated as a failed font and the next font tried.
  - Cap title and legend text length before shaping.
  - Build each font's shaper once and cache it: a hostile font can cost up to 128 MiB and a noticeable delay to prepare.
  - Add the Unicode data notice for its embedded tables to `THIRD-PARTY-NOTICES.md`.
- Planned: move to harfrust 0.16 and skrifa 0.50 together once both are 14 days old (from 2026-11-26), after reviewing their diffs; 0.16 extends the work budget to AAT tables.

Both crates: wrap font loading, shaping and drawing in `catch_unwind` at Rondi's text boundary and fall back to a built-in font, since neither guarantees it cannot panic; and fuzz the exact load, shape and draw path Rondi uses, with overflow checks on.

### Rejected

| Crate | Reviewed | Reason |
|---|---|---|
| `parley` 0.12, `fontique` | 2026-11-13 | Not needed for single-line text and adds 38 crates (22 ICU4X). `fontique`'s default `system` feature links fontconfig on Linux and calls CoreText on macOS; without it there is no font discovery. It memory-maps font files with no way to turn that off under `std`. |
| `fontdb` 0.24 | 2026-11-13 | A malformed font collection's face count is never checked against the file size, so a 12-byte file hangs the scan for billions of iterations or aborts on a 32 GiB allocation. Every family-name record is kept, so a small name table can expand to gigabytes. Memory-maps by default, follows symlinks without ownership checks, has no fuzzing, and carries a second font parser. |
| `rustybuzz`, `ttf-parser` | 2026-11-13 | Unmaintained advisories (RUSTSEC-2026-0206, RUSTSEC-2026-0192); a second font parser next to `read-fonts`. |
| `tiny-skia` | 2026-11-13 | Maintainer decision: Rondi writes its own rasterizer to keep dependencies minimal. |
| `criterion` 0.8 | 2026-11-13 | Always depends on `alloca`, which compiles C through `cc` (rule 1). Replaced by the standard-library harness in `crates/rondi/benches/harness/`. |
| Cairo, Pango, fontconfig, FreeType and HarfBuzz bindings | 2026-11-13 | Rule 1: C libraries. |

## Non-Rust files that ship

| File | Shipped in | Why it cannot be Rust |
|---|---|---|
| `compat/rrdtool-proxy.php` | release archives | Cacti service units start the proxy as `php rrdtool-proxy.php`. The file only starts the Rust binary with the same arguments; it implements nothing. Left out of the container image and the packages. |
| `scripts/install-compat-links.sh` | release archives | Creates the `rrdtool`, `rrdcached` and other names as links to the `rondi` binary for archive users. Installation tooling, not part of the tool. |
| `packaging/scripts/*.sh` | `.deb` and `.rpm` | Package maintainer scripts must be shell; `dpkg` and `rpm` run them. |

## FAQ: why does Rondi depend on `libc`?

`libc` is Rust declarations for the operating system's C library and adds no C code. Rondi uses it for:

1. System calls the standard library does not expose: `openat`, `fstatat`, `renameat` and `unlinkat` with `O_NOFOLLOW`, `flock` and `fcntl` locks, `mmap` of Rondi's own files, `umask`, ownership changes.
2. rrdcached process control: privilege drop for `-U` and `-G`, account lookups, signal masks.
3. Byte-exact RRDtool output that depends on the C library: locale-aware time formatting, `printf`-style number formatting and parsing, `qsort` order, address-lookup error text.

The first two could move to a pure Rust system-call crate such as `rustix`. The third cannot be dropped without changing output; reimplementing it in Rust means embedding locale data and reading account databases directly, which loses LDAP and SSSD accounts. Neither change has been decided.
