# Plan: pure Rust graph rendering (M5)

Status: agreed 2026-11-13, not started. Milestone M5 in [ROADMAP.md](../ROADMAP.md). Dependency rules and reviews: [DEPENDENCIES.md](../DEPENDENCIES.md).

## Goal

`rrdtool graph` and `graphv` output that RRDtool users and tools accept unchanged, written entirely in Rust:

- **Exact:** `graphv` keys and values, PRINT values, image width and height, the data formats (XML, JSON, CSV, TSV, SSV), exit status and error text, for the full option and element grammar.
- **Perceptual:** PNG within SSIM 0.98 of RRDtool 1.11.0 for every image in a graph corpus rendered in the pinned container, and SVG with identical text content.
- **Not a goal:** byte-identical PNGs. They would need RRDtool's exact Cairo, Pango, FreeType and font versions. The AGENTS.md graph rule already prefers measured tolerances over reproducing rendering internals.

## Decisions

| Area | Decision |
|---|---|
| Rasterizer | Written in Rondi, clean-room from behaviour, no rasterizer crate (`tiny-skia` rejected for dependency count). |
| Font parsing, outlines, hinting | `skrifa`, under the conditions in DEPENDENCIES.md. |
| Shaping | `harfrust`, under the conditions in DEPENDENCIES.md. |
| Text layout | Written in Rondi. RRDtool lays out single lines only, so `parley` is not used. |
| Font discovery | Written in Rondi on top of `skrifa`. `fontdb` and `fontique` are rejected. |
| PNG encoding | The `png` crate Rondi already uses. |
| SVG, PDF, EPS | Written in Rondi from the same drawing commands, with no new crates. PDF text embedding is an open question below. |
| Graph semantics | Clean-room from `docs/compatibility/graph.md` and black-box tests, per AGENTS.md. |

## Architecture

A new `rondi-graph` crate (the M4 prerequisite (3) in the roadmap) holds graph and xport, so the CLI and a later librrd shim share one implementation. Inside it:

1. **Front end** (existing, moving from `rondi-cli`): option parsing (`graph_args.rs`), element parsing and data preparation (the clean-room replacement of `graph.rs`, already in the roadmap), layout (`layout`).
2. **Scene:** a list of drawing commands in canvas coordinates (fill path, stroke path with width, dash and caps, text run at a position with a font and size, gradient fill, clip). Layout and element painting produce the scene; nothing below it knows about RRDtool.
3. **Text:** font discovery, font loading behind the safety checks, shaping, measuring, and glyph outlines turned into scene paths. Measuring is the same code layout uses, so text sizes in `graphv` and in pixels agree.
4. **Back ends:** the raster back end (rasterizer, then PNG), and the SVG, PDF and EPS writers, each consuming the same scene.

## Components

### Rasterizer

Behaviour to implement and test:

- Anti-aliased fill of paths made of lines and quadratic and cubic curves (curves flattened to a fixed tolerance), with non-zero and even-odd rules.
- Strokes: width, butt, round and square caps, miter, round and bevel joins with a miter limit, dash arrays and dash offset (RRDtool's `dashes=` and `dash-offset=` already parse in Rondi).
- Linear gradients for AREA's gradient form.
- RGBA source-over compositing in premultiplied form, and the 8-bit sRGB PNG canvas RRDtool writes.
- Rectangular clipping to the graph area.
- Glyph outlines drawn through the same fill path.

Approach: a coverage-accumulation scanline rasterizer (signed area per cell, then prefix sum per row), which is standard, small and exact enough for SSIM targets. Measure each primitive against RRDtool output in the pinned container before and after, so drift is visible.

### Text

- **Font discovery.** Walk the font directories (`/usr/share/fonts`, `/usr/local/share/fonts`, `~/.local/share/fonts`, `~/.fonts` on Linux; `/System/Library/Fonts`, `/Library/Fonts`, `~/Library/Fonts` on macOS; optionally the `<dir>` entries of `fonts.conf`, read with the existing `roxmltree`). Use Rondi's file-system rules: no-follow opens, ownership checks, a depth cap, a file size cap, and a cap on faces per collection of `(len - 12) / 4` and a small absolute limit. Read only each file's table directory and its `name` and `OS/2` tables through skrifa, keeping one family name per language. Cache nothing across processes unless a later measurement says start-up cost matters.
- **Font selection.** Parse RRDtool's font specifications (`--font TAG:size:font`, `RRD_DEFAULT_FONT`, Pango-style strings such as `DejaVu Sans Mono,monospace 8`) into a family list, weight, style and size, and match case-insensitively. The exact RRDtool rules come from a black-box specification written first (below).
- **Loading.** Read the chosen file into memory under the size cap (never memory-mapped), check its magic, build the skrifa `Font` and the harfrust shaper once per face and keep them for the process.
- **Shaping and layout.** Cap the text length, split the string into runs by font coverage (the first face in the list whose character map has the character, with the built-in font last), shape each run with harfrust, check `allocation_successful()`, then sum advances scaled by units per em. Bidirectional reordering needs `unicode-bidi`; it is left out until a corpus case needs it, and then reviewed like any other crate.
- **Outlines.** Ask `required_memory_size()` before drawing each glyph, refuse above the cap, draw into Rondi's buffer, unhinted or with the font's own hints (no autohinter), and emit scene paths.
- **Failure.** Font loading, shaping and drawing run inside `catch_unwind`. Any failure falls back to the built-in font (the current 8x8 bitmap face, kept for this purpose), so a bad font degrades the image instead of failing the graph.

### Output formats

- **PNG:** the rasterized canvas through `png`, with RRDtool's `--imginfo` and `--zoom` behaviour (already implemented).
- **SVG:** scene paths as `<path>`, text as `<text>` with the family and size, so text content matches RRDtool's.
- **PDF and EPS:** scene paths as vector operators. Text either as outlines (simplest, no font embedding) or as embedded fonts; see open questions.

## Clean-room and testing

1. **Specify first.** Before implementing, extend `docs/compatibility/graph.md` (or add `graph-render.md`) by black-box runs of RRDtool 1.11.0: which font RRDtool picks for each specification, default font sizes per tag, line caps and joins per element, dash geometry, gradient direction, grid line pixel positions, legend sample shapes, SVG structure. Record requirements in implementation-neutral language with test vectors, per AGENTS.md. Do not read Cairo or Pango source to copy their algorithms.
2. **Corpus.** A graph corpus (command lines plus `.rrd` fixtures) rendered by RRDtool in the pinned container; Rondi's output is compared by SSIM for PNG and by text for `graphv`, data formats and SVG. Start from the existing 2,213 graph specification cases and add Cacti, Munin and Smokeping templates.
3. **Primitive tests.** Unit tests for the rasterizer (coverage of known shapes, stroke widths, joins, dashes) independent of RRDtool.
4. **Fuzzing.** cargo-fuzz targets for font loading through glyph drawing on arbitrary bytes, for shaping arbitrary text with arbitrary fonts, and for the font specification parser, built with overflow checks.
5. **Regressions.** CI fails when a corpus image drops below the SSIM bar or any exact output changes.

## Phases

Each phase is one or more pull requests, specification before code.

1. **Spec and corpus:** the black-box rendering specification and the corpus harness with SSIM scoring against the current renderer (the baseline).
2. **`rondi-graph` crate and scene:** move graph and xport out of `rondi-cli` and introduce the scene, with the current renderer drawing from it. No output change.
3. **Rasterizer:** fills, strokes, dashes, gradients, clipping; switch PNG to it. Corpus SSIM should rise.
4. **Text:** font discovery and selection, loading with the safety conditions, shaping, outlines, fallback; add skrifa and harfrust at the versions then approved; fuzz targets. Add the Unicode notice.
5. **SVG, then PDF and EPS.**
6. **Exit:** M5's criteria in the roadmap pass in CI.

## Open questions

- PDF and EPS text: outlines (no embedding, text not selectable) or embedded font subsets (selectable, more code). Decide in phase 5.
- Hinting: unhinted outlines may match RRDtool's Cairo output better or worse than the font's own hints at small sizes; measure in phase 4.
- Whether RTL legends occur in real templates, which decides `unicode-bidi`.
- Whether to read `fonts.conf` at all, or only the standard directories, depending on what the specification shows RRDtool finds on stock systems.
- Upgrading to harfrust 0.16 and skrifa 0.50 (eligible from 2026-11-26) before phase 4 starts.
