# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](VERSIONING.md).

## [Unreleased]

### Added

- Support graph print base 1000/1024, shared SI-unit state, and UTC event-time printing with distinct console/export behavior matching RRDtool.

- Accept named and mixed PRINT/GPRINT arguments with C-compatible positional fallback and tested duplicate/unused-argument diagnostics.

- Support PRINT/GPRINT value timestamps and millisecond durations, preserving distinct console and XML/JSON metadata behavior from RRDtool.
- Add Criterion fetch and graph calculation benchmarks for small/large archives, including sequential PREV expressions and saved baseline comparisons.

- Draw formatted GPRINT values in PNG legends, with explicit multiline row alignment and bounded canvas sizing.

- Substitute prior VDEF aggregates as CDEF scalars, including PREV(VDEF) and TREND duration operands.

- Support graph/xport SHIFT with integer or VDEF offsets, step normalization, replacement shifts, and ordered CDEF/VDEF evaluation.

- Accept graph COMMENT and TEXTALIGN elements with positional/named arguments and basic PNG comment rendering.

- Support previously defined VDEF aggregates in graph HRULE and VRULE elements, with upstream-compatible invalid-reference diagnostics.

### Fixed

- Validate SHIFT conversion by comparing end pointers without dereferencing the parser output.
- Preserve target-specific C NaN bits for generated unknown VDEF aggregates, including x86-64 print output.

- Match tested fixed-point PRINT/GPRINT metadata rounding and padding from the C formatter.

- Buffer native RRD fetches and seek only at ring boundaries, preserving shared locks and fetch semantics. The measured four-source, 7,200-row workload improved by about 23 times.
- Reuse CDEF tokens and evaluation stack across rows. Paired graph calculation benchmarks improved by about 1.55 times.
- Apply small/upward legend spacing controls and preserve header/footer comments under bottomup2.

- Preserve fragment order when reversing legend rows and accept the separate-argument legend-direction option.

- Reduce legacy graph PRINT values over native buffers including right-edge calculations.
- Match tested scientific PRINT/GPRINT metadata scaling, rounding, signs, and padding separately from console printf.

- Preserve graph metadata order when GPRINT entries are interleaved with comments, alignment, and series.

- Paint graph rules after data elements regardless of their position in the graph script.
- Preserve daemon FETCH NaNs and infinities, parse decimal values with RRDtool's conversion, and use host C formatting.
- Match Darwin's `isinf` result in VDEF percentile sorting while preserving glibc behavior.
- Supply the disposable RRA directory to Kadupul's maintenance lease in the poller smoke harness.
- Correct the daemon fractional-update test's macOS canonical-path expectation.
- Evaluate CDEFs at their input-derived sampling interval before export resampling, preserving mixed-resolution prediction history and COUNT/PREV behavior.
- Limit the initial CodeQL matrix to workflow analysis until application source is added.
