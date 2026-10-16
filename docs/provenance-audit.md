# Provenance audit

This audit identifies Rondi code that appears to derive from the RRDtool 1.11.0 C implementation rather than from independent design. It is engineering provenance hygiene, not a legal conclusion. Similarity alone does not prove copying: a behavior-compatible reimplementation of a file format, protocol, and command line will share names, constants, error text, and edge-case behavior with the original for legitimate reasons. The findings below separate those reasons from implementation expression that the project cannot currently show was independently written.

Audited revision: `origin/main` at `0fd0cf8`. Reference: RRDtool 1.11.0 release tarball, used only for side-by-side similarity assessment. This report contains no RRDtool source expression.

## Summary

The repository describes itself, in code, documentation, commit messages, and pull request titles, as a port of RRDtool's C implementation. The README states that "Rondi ports behavior and code from RRDtool 1.11.0", while the workspace is licensed `MIT OR Apache-2.0`. Between commit `de820e9` (2026-10-07, "license under RRDtool's terms") and `36346a8` (2026-10-14, "license Rondi under MIT OR Apache-2.0") the project was declared GPL-2.0-or-later with the RRDtool FLOSS exception, and most self-described ports landed in that window. That sequence needs a human and legal decision before any code work.

About 12,400 lines of Rust (38 percent of non-test source) are rated High: modules whose doc comments call them a port of a named C file, which reuse the C function decomposition and internal function names, and which in several cases keep the C local-variable vocabulary. About 15,700 lines are rated Review and about 4,500 lines Low.

| File | Risk tier | Approx. lines | Primary concern |
|---|---|---:|---|
| `crates/rondi/src/time.rs` | Replaced | 1,001 | Declared port; replaced by the clean-room `time/` modules from `docs/compatibility/time-spec.md` |
| `crates/rondi/src/rpn.rs` | Replaced | 1,084 | Declared port; replaced by the clean-room `rpn/` modules from `docs/compatibility/rpn.md` |
| `crates/rondi/src/rrd_modify.rs` | Removed | 981 | Declared port; replaced by the clean-room `tune_modify.rs` |
| `crates/rondi/src/rrd_restore.rs` | Replaced | 912 | Declared port; replaced by a clean-room rewrite from `docs/compatibility/restore.md` |
| `crates/rondi/src/rrd_create.rs` | Removed | 790 | Declared port; replaced by the clean-room `rrd_create.rs` and `rrd_definitions.rs` |
| `crates/rondi/src/rrd_resize.rs` | Removed | 288 | Declared port; replaced by a clean-room `rrd_resize.rs` |
| `crates/rondi/src/graph.rs` | High | 2,357 | Declared port of graph script parsing and data preparation; 18 shared function names |
| `crates/rondi/src/graph_layout.rs` | Replaced | 1,011 | Declared port of image layout functions; replaced by the clean-room `layout/` from `docs/compatibility/graph.md` |
| `crates/rondi-cli/src/graph_options.rs` | Replaced | 880 | Option tables keyed to C line numbers; replaced by the clean-room `graph_args.rs` from `docs/compatibility/graph.md` |
| `crates/rondi-cli/src/optparse.rs` | Removed | 445 | Port of the bundled option parser; replaced by the clean-room `frontend/options.rs` |
| `crates/rondi/src/rrd_snprintf.rs` | Replaced | 702 | Declared port of the portable snprintf; replaced by the clean-room `format_value/` from `docs/compatibility/format-strings.md` and black-box runs |
| `crates/rondi-server/src/rrdcached_listen.rs` | Removed | 926 | Declared port of listener setup and daemonize (GPL-2.0-only file); replaced by the clean-room `rrdcached/` daemon modules |
| `crates/rondi-cli/src/main.rs` (front end, print/time helpers, list) | High | ~1,020 of 7,295 | Ports of the command dispatcher, batch-mode tokenizer, print_calc and helpers, directory listing |
| `crates/rondi-cli/src/help/*.txt` | High (FLAG) | 354 text | Verbatim RRDtool help output |
| `crates/rondi/src/rrd_binary.rs` | Review | 3,046 | Update path follows C helper boundaries with line citations; format reader is format-dictated |
| `crates/rondi-server/src/lib.rs` | Replaced | 5,203 | rrdcached handlers, queue and journal replaced by the clean-room `rrdcached/` protocol modules; the JSON API remains |
| `crates/rondi/src/xml_reader.rs` | Replaced | 1,958 | Modelled libxml2 internals; replaced by a clean-room rewrite from `docs/compatibility/restore.md` and black-box runs |
| `crates/rondi-cli/src/main.rs` (rest except renderer) | Review | ~4,735 | Command handlers with C line citations; rrdcached client ported from an MIT-licensed C file |
| `crates/rondi/src/rrd_number.rs` | Review | 451 | Number parsing algorithm from a BSD-licensed C file |
| `crates/rondi-cli/src/cparse.rs` | Review | 217 | Same as above |
| `crates/rondi/src/rrdc_proto.rs` | Removed | 116 | Two small field-codec ports; replaced by `rrdcached/protocol.rs` |
| `crates/rondi/src/vdef.rs` | Low | 513 | Standard statistics; compiler contraction emulation documented |
| `crates/rondi/src/storage.rs`, `consolidation.rs`, `data_source.rs`, `format.rs`, `import_export.rs`, `lib.rs`, `queries.rs`, `compatibility.rs`, `safe_fs.rs` | Low | 2,143 | Rondi's own store and helpers |
| `crates/rondi-cli/src/main.rs` (PNG renderer, unit tests) | Low | ~1,535 | Independent bitmap renderer |
| `fuzz/src/**` | Low | 270 | Harness only |

Counts (executed):

- Rust source files audited: 29 in `crates/*/src` plus 4 in `fuzz/src` (33). Test, bench and fuzz-target files audited: 51. Markdown files audited: 13. Git commits examined: 433 on all refs.
- Explicit port claims ("port of", "ported from", "Ports", "the port"): 37 in source, 26 in documentation, 0 in tests.
- Mentions of named RRDtool C files: 194 in source, 152 in tests, 111 in documentation.
- C line-number citations (for example `file.c:NNN` or `(NNN-MMM)`): 129 in source, 89 in tests, 5 in documentation.
- "Follows", "mirrors", "as upstream", "like C", "keep the C control flow" style statements: 32 in source, 8 in tests, 49 in documentation.
- Commit messages naming a port, translation or a C file: 36 distinct commits (20 with port wording, 14 naming a C file, 2 with "translation", which refers to rrdcached path translation rather than code translation).
- Pull request titles with port, C-file, or "like RRDtool" wording: 16 of 100 (#50, #54, #56, #58, #59, #63, #64, #68, #69, #70, #72, #73, #76, #91, #95, #97).
- Internal RRDtool function names reused as Rust function names: 91 (after excluding generic names such as `main`, `parse`, `token`).

## Classification used

Each finding is classified as:

1. Behavior required for compatibility (observable output, exit status, error text, accepted input).
2. Structure or constant dictated by an external format or protocol (the `.rrd` on-disk layout, the rrdcached line protocol, the XML dump schema, public option names).
3. Conventional algorithm or math formula.
4. Potentially copyrightable implementation expression (decomposition, naming, control flow, data structures, comments, or text not required by 1 to 3).

Risk tiers:

- Low: categories 1 to 3 only, or code with no RRDtool counterpart.
- Review: mostly categories 1 to 3, with some category 4 signals such as C line citations, mirrored helper boundaries, or quoted C fragments in comments.
- High: the code documents itself as a port or translation, or side-by-side reading shows the same decomposition and naming as the C function it replaces.

"Source of requirement" names where a reimplementer could legitimately learn the behavior: public documentation (man pages), the file format, black-box observation of the pinned binary, or reference-source inspection. Where the only practical source is reading the C, the item is marked FLAG.

## Findings by module

### High

**`crates/rondi/src/time.rs`: `Parser::{token, parse_token, expect2, plus_minus, tod, assign_date, day, numeric_day, parse}`, `rrd_parsetime`, `rrd_proc_start_end`**

- Why: the module doc says the scanner, token tables and the named routines "keep the C control flow, integer widths and messages". Seven function names match the C file. Scanner state fields and saved-state locals use the C names (`scc`, `sct`, `sc_tokid`, `sc_token`, `scc_sv`, `sct_sv`). Keyword tables are ordered and spelled as in the C tables. Classification 4 for structure; 1 for messages and accepted grammar.
- Compatibility requirement: the at-style time grammar (`now-1d`, `noon yesterday`, `start+1h`), its error messages, and DST behavior through `mktime`.
- Likely source: the grammar is documented in the `rrdfetch` man page (AT-STYLE TIME SPECIFICATION); messages are black-box observable; the "m means months or minutes" guess and normalization order were learned by reference-source inspection.
- Remediation: replace by spec and fresh implementation. Write a grammar spec from the man page plus a table of observed messages and edge cases, then reimplement with a different scanner design (for example a token iterator and a small recursive-descent parser). Keep `crates/rondi-cli/tests/parsetime_compat.rs` (15 tests), the `fuzz/fuzz_targets/time_spec.rs` seeds, and the in-module tests as acceptance. FLAG: the C file carries a BSD-style notice from its original authors plus GPL modifications by RRDtool, so attribution obligations differ by layer.

**`crates/rondi/src/rpn.rs`: `rpn_parse`, `rpn_calc`, `RpnStack`, `Node`, `qsort`, `tzoffset`, `find_first_weekday`**

- Why: module doc says "Port of `rpn_parse` and `rpn_calc`"; five function names match; a doc comment maps a Rust field to its C struct member; the stack pointer is named `stptr` as in C; comments describe C undefined-behavior windows that the Rust code reproduces (MEDIAN and PERCENT over an out-of-range window). Classification 4 for structure; 1 for operator semantics and error text; 3 for the arithmetic.
- Compatibility requirement: every RPN operator's result, unknown and infinity handling, row cursor semantics for mixed steps, and error strings such as the garbage-in-RPN message.
- Likely source: operators are documented in `rrdgraph_rpn`; cursor stepping and error text are black-box observable; the UB window and `qsort` tie order came from reference-source inspection.
- Remediation: replace by spec and fresh implementation. Specify each operator as a table (inputs, output, unknown behavior), specify mixed-step cursor advance from observation, and decide whether the UB window should be specified as an observed result or rejected. Tests pinning behavior: `crates/rondi-cli/tests/rpn_differential.rs` (11), `graph_data_differential.rs`, `fuzz/fuzz_targets/rpn_diff.rs` and `rpn_xport.rs`.

**`crates/rondi/src/rrd_modify.rs`: `modify_structure`, `find_candidate_rras`, `sort_candidates`, `populate_row`, `row_for_time`, `end_time_for_row`, `init_cdp`, `calculate_cdp_val`, `stretch_rras`, `positive_mod`**

- Why: module doc says "Port of RRDtool 1.11.0 `rrd_modify.c`"; nine internal function names match; 17 C line citations; comments quote C expressions (around lines 326 and 597); `REALLOC_ZERO_FAILS` reproduces glibc `realloc(ptr, 0)` returning NULL so that a zero-data-source edge case reports the same allocation error. Classification 4; the resampling arithmetic is partly 3.
- Compatibility requirement: the result of `tune` DS and RRA edits (which rows are kept or resampled into new archives, CDP state after edit) and error text.
- Likely source: `rrdtune` man page documents the operations; resampled values are black-box observable; candidate selection order and the allocator quirk came from reference-source inspection.
- Remediation: replace by spec and fresh implementation. The resampling rule can be stated as a mathematical spec (which source rows cover a destination row, and how they combine per CF). Specify the zero-DS case as an observed error rather than emulating the allocator. Existing coverage is thin: `crates/rondi-cli/tests/tune_modify_differential.rs` has 5 tests, so add differential vectors before rewriting.
- Status: removed. `crates/rondi/src/tune_modify.rs` replaces it, written from `docs/compatibility/tune-modify.md` and its vectors without reading this module or the C source.

**`crates/rondi/src/rrd_restore.rs`: `Restore::{get_xml_element, expect_element, expect_element_end, get_xml_text, get_xml_string, get_xml_time_t, get_xml_ulong, get_xml_double, parse_tag_*}`, `layout`**

- Why: module doc says "Port of RRDtool 1.11.0 `rrd_restore.c`: the libxml2 pull-parser walk in `parse_file` and its `parse_tag_*` helpers"; 16 function names match one-for-one; a doc comment quotes a C loop condition (around line 132); the empty-database case reproduces aarch64 division-by-zero behavior. Classification 4 for the walk; 2 for element names; 1 for error text.
- Compatibility requirement: accept the XML dump schema that `rrdtool dump` produces, reject malformed input with RRDtool's messages, and produce an equivalent `.rrd`.
- Likely source: the XML schema is a published external format (category 2) and observable from `dump`; error wording is black-box observable; the walk order and per-tag helper split came from reference-source inspection.
- Remediation: replace by spec and fresh implementation using a schema-driven parser. Tests: `crates/rondi-cli/tests/csrc_mutate.rs` (16), `fuzz/fuzz_targets/restore_xml.rs`, in-module limit tests. Decide separately whether libxml2-exact line numbers in error text (see `xml_reader.rs`) are a requirement.
- Status: replaced. A new `rrd_restore.rs` was written from `docs/compatibility/restore.md`, its vectors and black-box runs of RRDtool 1.11.0, without reading this module or the C source.

**`crates/rondi/src/rrd_create.rs`: `rrd_create_r2`, `create_bytes`, `parse_ds`, `parse_generic_ds`, `parse_rra`, `rrd_init_data`, `write_rrd`, `ErrorSlot`, `mkstemp`, `chmod_like_write_rrd`, `match_ds_re`, `scan_min_max`**

- Why: module doc lists the C functions it ports; four names match; `ErrorSlot` models the C library's single error slot (a data structure not dictated by any format); `mkstemp` reproduces the C temporary-name alphabet; `scan_min_max` reproduces a fixed scanf field width. Classification 4 for structure; 2 for the bytes written; 1 for error text and DS regex acceptance.
- Compatibility requirement: `create` accepts the documented `DS:` and `RRA:` syntax and writes a byte-compatible `.rrd`.
- Likely source: `rrdcreate` man page and the on-disk format; edge cases (18-byte min/max cut, regex acceptance) from reference-source inspection.
- Remediation: replace by spec and fresh implementation. The `.rrd` writer can be rebuilt from the format spec (shared with `rrd_binary.rs`); argument acceptance from a grammar spec. Tests: `crates/rondi-cli/tests/csrc_create.rs` (8) and in-module tests.
- Status: removed. `crates/rondi/src/rrd_create.rs` (new) and `crates/rondi/src/rrd_definitions.rs` replace it, written from `docs/compatibility/create.md`, its vectors and black-box runs without reading this module or the C source.

**`crates/rondi/src/rrd_resize.rs`: resize entry point and the in-memory `rrd_file` model**

- Why: module doc says "Port of RRDtool 1.11.0 `rrd_resize.c`", and the file models the C file-position and clamp rules with line citations so that short and overlong layouts behave the same. Classification 4 for the file-position model; 1 for the row mapping.
- Compatibility requirement: GROW and SHRINK row mapping relative to the ring pointer, fixed `resize.rrd` output name.
- Likely source: `rrdresize` man page plus black-box observation; the short-file behaviour came from reference-source inspection.
- Remediation: replace by spec and fresh implementation; specify the row mapping as a formula and decide whether malformed short-file behaviour is worth matching. Tests: `csrc_mutate.rs`.
- Status: removed. A new `crates/rondi/src/rrd_resize.rs` replaces it, written from `docs/compatibility/resize.md`, its vectors and black-box runs without reading this module or the C source.

**`crates/rondi/src/graph.rs`: `ParsedArgs::parse`, `GraphImage::{new_graph_description, graph_script, parse_def, parse_cvdef, parse_line_area, parse_hvrule, parse_gprint, parse_comment, parse_tick, parse_textalign, parse_shift, parse_xport, data_fetch, data_calc, cdef_calc, vdef_calc}`, `vdef_parse`, `reduce_data`, `legend_shift`, `gf_conv`, `find_var`**

- Why: module doc says it "Ports `rrd_graph_script` and its element parsers" plus `data_fetch`, `data_calc`, `vdef_parse`, `vdef_calc`, `print_calc` and `rrd_xport_fn`; ten "Port of" doc comments; 18 matching function names; fields and parameters keep the C names for the element array, parsed-argument struct and element type (`gdes`, `vidx`, `pa`, `gf`); integer-width comments that track C types. Classification 4 for parser decomposition; 1 for element syntax, error text and data alignment.
- Compatibility requirement: graph element syntax (DEF, CDEF, VDEF, SHIFT, PRINT, GPRINT and the drawing elements), per-DEF fetch windows, step reduction, and export values.
- Likely source: `rrdgraph_data`, `rrdgraph_graph`, `rrdxport` man pages; numeric results black-box observable; element-append ordering and reduce-window integer behaviour from reference-source inspection.
- Remediation: replace by spec and fresh implementation, in two parts: an element grammar spec with error table, and a data-preparation spec (window, step GCD, reduce rule). Tests: `graph_data_differential.rs` (15), `graph_engine_parity.rs` (5), `graph_xport_parity.rs` (10), `vdef_differential.rs` (3), `xport_option_differential.rs`.

**`crates/rondi/src/graph_layout.rs`: `graph_size_location`, `leg_place`, `data_proc`, `si_unit`, `expand_range`, `gfx_get_text_width`, legend position helpers**

- Why: module doc names the C functions it reproduces; six function names match; the layout works in C `int` widths by design. Classification 4 for decomposition; 1 for the reported `graphv` coordinates and value range.
- Compatibility requirement: `graphv` image size, legend positions, value range and SI unit selection.
- Likely source: black-box observation of `graphv` output; text metrics from the font; the step-by-step layout from reference-source inspection.
- Remediation: replace by spec and fresh implementation; derive a layout spec from measured `graphv` outputs across option combinations. Tests: `graph_option_differential.rs` (8), `graph_line_width.rs`, in-module tests.
- Status: replaced. `crates/rondi/src/layout/` (text metrics, legend lines, frame and value range) was written from GR-60 to GR-73 of `docs/compatibility/graph.md`, its vectors and black-box runs of RRDtool 1.11.0, without reading the removed module's bodies or the C source; the module was deleted from a signature listing. `graph_layout.rs` now holds only two enums and the default font sizes that the legacy `graph.rs` still stores, written fresh.

**`crates/rondi-cli/src/graph_options.rs`: `rrd_graph_options`, `bad_format_axis`, `parse_color`, xport option parsing**

- Why: eight doc comments anchor option handling to C line numbers; three function names match. Option names are public interface (category 2), but per-option validation order and messages were transcribed alongside the C. Classification 1 and 4.
- Compatibility requirement: accept and validate every documented `graph`, `graphv` and `xport` option with matching error text.
- Likely source: `rrdgraph` and `rrdxport` man pages; error text black-box observable.
- Remediation: replace by spec and fresh implementation (an option table built from the man pages plus an observed error table). Tests: `graph_option_differential.rs`, `xport_option_differential.rs`, `rrdtool_command_options.rs` (14).
- Status: replaced. `crates/rondi-cli/src/graph_args.rs` was written from `docs/compatibility/graph.md`, its vectors and black-box runs of RRDtool 1.11.0, from the removed module's signatures only, without reading its bodies or the C source. One exposure: a grep for module paths printed seven body lines (the legend direction and position assignments and the line that sets the no-RRDtool-tag flag); none of them shaped the new code beyond what the option table already states.

**`crates/rondi-cli/src/optparse.rs`: `OptParse::{short, long, long_at, finish}`, `ArgType`, `LongOpt`, `DONE`, `ERROR`, `ERRMSG_SIZE`**

- Why: `main.rs` declares the module a "Port of RRDtool 1.11.0 `src/optparse.c`"; doc comments map Rust types to the C enum and struct; `ERRMSG_SIZE` reproduces the C error-buffer size so long option names truncate the same way. Classification 4 for structure; 1 for permutation order and error text.
- Compatibility requirement: argument permutation, `--` handling, bundled short options and error messages for every command.
- Likely source: black-box observation; the bundled parser's behaviour is close to GNU `getopt_long`, which is documented.
- Remediation: replace by spec and fresh implementation; this is small and well pinned by `rrdtool_optparse.rs` (25 tests) and `rrdtool_command_options.rs`. FLAG: the C copy shipped in RRDtool has no file-level notice; its upstream origin and license should be confirmed before deciding whether this is a GPL concern at all.
- Status: removed. `crates/rondi-cli/src/frontend/options.rs` replaces it, written from `docs/compatibility/cli.md`, its vectors and black-box runs of RRDtool 1.11.0 without reading this module or the C source.

**`crates/rondi/src/rrd_snprintf.rs`: `rrd_snprintf`, `fmtflt`, `fmtint`, `fmtstr`, `convert`, `getexponent`, `getnumsep`, `mypow10`, `myround`, `float_digits`, `exponent_digits`, `emit_digits`**

- Why: module doc says "Port of `rrd_snprintf`"; seven function names match; inside `fmtflt` the Rust locals reuse the C names (`iconvert`, `fconvert`, `econvert`, `leadfraczeros`, `omitcount`, `padlen`, `emitpoint`) in the same order of use. Classification 4; the power-of-ten scaling is 3.
- Compatibility requirement: `dump` and `xport` print `%e`/`%f` values that are not correctly rounded, so the C library cannot be used.
- Likely source: black-box observation of dump output reveals the rounding; the algorithm (scale by powers of ten, round the scaled fraction) can be stated without the C, but the current code was taken from reference-source inspection.
- Remediation: replace by spec and fresh implementation. Write the conversion as a short arithmetic spec and verify with a large differential corpus. Tests: `number_format_differential.rs` (2), `csrc_dump.rs` (1), in-module tests; add more vectors first. FLAG: the C file credits Patrick Powell and Holger Weiss under a permissive grant layered with RRDtool's header; legal should confirm which terms apply.
- Status: replaced. `crates/rondi/src/format_value/scaled.rs` implements a behavioral model fitted to black-box runs of the pinned RRDtool (thousands of xport PRINT values and formats on macOS arm64 and Linux x86_64), without reading this module or the C source. `number_format_differential.rs` now also replays random formats through xport and graph PRINT.

**`crates/rondi-server/src/rrdcached_listen.rs`: Unix and network listener setup, `-m`/`-s`/`-P` handling, daemonize and privilege drop**

- Why: module doc says "ported from RRDtool 1.11.0 `src/rrd_daemon.c` (`open_listen_socket_unix`, `open_listen_socket_network`, `open_listen_socket`, and `daemonize`)". The C file is licensed GPL-2.0 only, not "or later". Classification 4 for structure; 1 for option semantics and messages.
- Compatibility requirement: `-l`, `-L`, `-m`, `-s`, `-P`, `-U`, `-G` behaviour, socket permissions, and startup messages.
- Likely source: `rrdcached` man page documents the options; messages black-box observable.
- Remediation: replace by spec and fresh implementation. Tests: `rrdcached_listeners.rs` (13), `rrdcached_privileges.rs` (6), `rrdcached_startup.rs` (7).
- Status: removed. The daemon layer (options, base directory, listeners, pid file, privileges, signals, timers, logging, shutdown) is now `crates/rondi-server/src/rrdcached/` and `crates/rondi-cli/src/rrdcached.rs`, written from `docs/compatibility/rrdcached-daemon.md`, its vectors, the existing daemon tests and black-box runs of rrdcached 1.11.0, without reading this module, the replaced daemon code in `lib.rs` and `main.rs`, or the C source.

**`crates/rondi-cli/src/main.rs`, front end and helpers (about 1,020 lines): `rrdtool_usage`, `print_usage`, `rrdtool_main`, `rrdupdate_main`, `rrdtool_dispatch`, `handle_input_line`, `remote_ls`, `fgetslong`, `count_args`, `create_args`, `rrdtool_batch`, `print_calc`, `strfduration`, `time_clean`, `rrd_list_r`, `rrd_list_rec`, `rrd_mkdir_p`**

- Why: each carries a doc comment naming the C function and line it reproduces, and the names match. The batch tokenizer and line reader reproduce C string handling, including embedded NUL behaviour. Classification 4 for structure; 1 for observable tokenization and output.
- Compatibility requirement: `rrdtool -` stdin mode (used by Cacti and Kadupul pollers), command dispatch, `list` output order, PRINT/GPRINT `%s`, `strftime` and duration formats.
- Likely source: man pages (`rrdtool`, `rrdgraph_graph` for GPRINT and duration formats) and black-box observation.
- Remediation: replace by spec and fresh implementation; the stdin tokenizer and `list` are small. Tests: `alias_modes.rs` (90), `cli_list_and_aliases.rs`, `graph_data_differential.rs`.
- Status: the dispatcher, usage, alias programs and pipe mode (`rrdtool_usage` through `rrdtool_batch`) are replaced by `crates/rondi-cli/src/frontend/`, written from `docs/compatibility/cli.md`, `docs/compatibility/pipe-mode.md`, their vectors and black-box runs without reading the removed functions or the C source. `print_calc`, `strfduration`, `time_clean` and the `list` helpers are not part of that change.
- Status of the print helpers: `print_calc`, `strfduration`, `time_clean` and their formatting helpers (with `format_graph_numeric` and `parse_graph_numeric_format`) are replaced by `crates/rondi-cli/src/print_format.rs` and `crates/rondi/src/format_value/`, written from `docs/compatibility/format-strings.md`, the vectors and black-box runs without reading the removed code or the C source.

**`crates/rondi-cli/src/help/*.txt` and the `RRDCACHED_HELP` string in `main.rs`**

- Why: README and `docs/rrd-source-notes.md` state the help texts are captured verbatim from the RRDtool 1.11.0 executable; `RRDCACHED_HELP` reproduces the rrdcached usage banner including its copyright line. Classification 1 (byte-exact output parity) but the text itself is expression.
- Compatibility requirement: no-argument output of each command, compared byte for byte by tests.
- Likely source: black-box observation.
- Remediation: FLAG for legal review. Options are to keep the text with the upstream notice (as the README currently says) under a separately licensed path, or to write original help text and drop byte parity for help output.

### Review

**`crates/rondi/src/rrd_binary.rs`**

- Format reader (`read_info`, `inspect_parts`, offset helpers): category 2. The `.rrd` header, DS, RRA, live head, PDP and CDP prep layout is an external file format. Keep as is; replace C line citations with references to a written format spec.
- Update path (`apply_update`, `update_text_arguments`, `update_pdp_new`, `process_pdp_steps`, `update_cdp_preps`, `update_cdp`, `write_rra_rows`): written as idiomatic Rust, but each function's doc comment names the C helper it corresponds to, with about 20 C line citations, and `update_cdp` keeps the C name. The arithmetic is the documented RRD consolidation model (category 3) and its exact results are observable (category 1). Remediation: write a consolidation spec (PDP integration, heartbeat, xff, carry-over, split PDP) from `rrdcreate`/`rrdupdate` documentation and observation, then reword comments to cite the spec. A rewrite is optional once the spec exists. Tests: `csrc_update.rs` (17), `rrdtool_differential.rs` (7), `rrd_binary_probe.rs` (30), `fuzz/fuzz_targets/update_args.rs`.
- Fetch (`prepare_fetch`, `choose_archive`): archive selection is documented in `rrdfetch` and observable. Keep, reword citations.
- Dump (`dump_rrd_open_file_bytes`): XML output is category 2 plus 1. Keep; its numbers now come from `format_value::data_value`.
- Locking (`parse_rrd_lock_mode`, `with_rrd_locking`): category 1 interoperability with the C writer. Keep.

**`crates/rondi-server/src/lib.rs`**

- Architecture is independent: tokio tasks, Rust queues, bounded reads, and an HTTP API with no RRDtool counterpart.
- Ported fragments: `flush_owned_rrdcached_path` documented as "Port of queue_thread_main", the UPDATE handler documented as "Port of handle_request_update", `journal_init`/`journal_write`/`journal_replay`/`journal_done` sharing C names, and `cache_tree_*`, an AVL tree added so the `STATS` `TreeDepth` value matches the C daemon's balanced tree. Comments quote C struct fields (around line 629).
- The daemon startup, signal, pid-file and listener code that lived here (`run_rrdcached` and its helpers, `PidFile`, `RrdcachedConfig`) was removed with the daemon-layer rewrite above; the request handlers, queue and journal remain for a second phase and are reached only through `rrdcached/engine.rs`.
- `RRDCACHED_COMMAND_TABLE` copies the command usage strings. These are observable through `HELP` (category 1) but are text expression; treat with the help files above.
- The C daemon is GPL-2.0 only.
- Status: replaced. The line parser, command handlers, cache, write queue, `TreeDepth` tree and journal are now `crates/rondi-server/src/rrdcached/` (`protocol.rs`, `commands.rs`, `session.rs`, `service.rs`, `cache.rs`, `depth.rs`, `journal.rs`, `files.rs`, `listing.rs`, `fetch.rs`, `create.rs`, `tune.rs`), written from `docs/compatibility/rrdcached.md`, `docs/compatibility/rrdcached-daemon.md`, their vectors, the existing tests and black-box runs of rrdcached 1.11.0, without reading the replaced handlers, queue and journal or the C source; they were deleted by line range with a script that printed only signatures. The usage and help texts are the recorded `HELP` output (vectors `help-*`) and stay under the help-text FLAG. `lib.rs` keeps only the JSON API, whose accept loop still cites a C line.
- Remediation (before the replacement): specify the line protocol, journal format and queue semantics from the `rrdcached` man page (which documents the protocol) and observation; rewrite the named handler ports; decide whether exact `TreeDepth` parity is worth an AVL implementation or whether the value should be specified as "height of a balanced tree over pending keys". Tests: `rrdcached_protocol_parity.rs` (11), `server_roundtrip.rs` (35), `rrdcached_client_paths.rs` (5), fuzz targets `rrdcached_protocol` and `rrdcached_journal`.

**`crates/rondi/src/xml_reader.rs`**

- Models libxml2's `xmlTextReader` and push-parser chunking so that restore error messages report the same line numbers as RRDtool. The modelled code is libxml2 (MIT licensed), not RRDtool, and the module reproduces its internal chunk sizes and state names. Category 4 relative to libxml2; category 1 for the resulting messages.
- Remediation: FLAG. Decide whether libxml2-exact error line numbers are a product requirement. If not, replace with an ordinary XML parser and specify error categories rather than byte-exact libxml2 text. If yes, confirm libxml2 attribution obligations.
- Status: replaced. The new `xml_reader.rs` is a hand-written tokenizer and reader whose delivery and availability rules were derived from the specification and from black-box runs of the pinned RRDtool (every truncation of two seed dumps, plus targeted inputs), without reading this module, RRDtool's C source or libxml2's source. The libxml2 diagnostic texts it prints are observable output.

**`crates/rondi-cli/src/main.rs`, remaining handlers (about 4,735 lines)**

- `rrdcached_mode` option parsing: option letters are public (category 2); messages category 1. Several citations to the C read-options routine. Status: removed with the daemon-layer rewrite; the `rrdcached` personality is now `crates/rondi-cli/src/rrdcached.rs`. Its usage text is the recorded `rrdcached -h` output (opt-001) and stays under the help-text FLAG below.
- Command handlers (`rrdtool_create`, `rrdtool_fetch`, `rrdtool_graph`, `rrdtool_xport`, `rrdtool_update_impl`, `rrdtool_tune`, `rrdtool_restore`, `rrdtool_dump`, `rrdtool_info`, `rrdtool_first`, `rrdtool_last`, `rrdtool_resize`, `rrdtool_list`): each cites the C option table line. Mostly category 1. Reword citations; no rewrite needed beyond the optparse change.
- rrdcached client (`rrdcached_client_path`, `translate_rrdcached_file_command`, `connect_rrdcached`, `split_rrdcached_network_address`): documented as ports of the client C file, which is MIT licensed. Category 1 for path and address rules. Remediation: confirm attribution needs; specify address and path rules from the `rrdcached` and `rrdtool` man pages.
- Xport and graph number formatting (`format_graph_numeric`, `parse_graph_numeric_format`, `format_xport_*`): mostly category 1 and 2 (XML and JSON shapes). Keep.

**`crates/rondi/src/rrd_number.rs` and `crates/rondi-cli/src/cparse.rs`**

- `rrd_strtodbl`, `parse_rrd_decimal` and the `rrd_strtod` wrappers reproduce the C decimal conversion because its operation order is observable in epoch-sized fractional values (category 1). The C source is BSD-licensed (Michael Ringgaard), not GPL. `rrd_diff` reproduces the GPL big-integer subtraction used for COUNTER deltas (category 3 arithmetic, category 1 precision). `c_strtol`, `c_strtoul_errno` follow libc contracts (category 1).
- Remediation: keep, with the BSD attribution question flagged; specify the decimal conversion as an arithmetic recipe.

**`crates/rondi/src/rrdc_proto.rs`**

- Two small codec functions documented as ports: field splitting from the daemon (GPL-2.0 only) and string escaping from the client (MIT). The escaping rules are the wire protocol (category 2).
- Remediation: rewrite from a two-paragraph protocol spec; about 50 lines plus tests.
- Status: removed. Field splitting is `crates/rondi-server/src/rrdcached/protocol.rs`, written from RC-7 to RC-12a; nothing else used the module.

**Tests and documentation**

- 25 test files carry 152 C file mentions and 89 C line citations in comments; several `csrc_*` test files label cases with internal function names and describe the C control flow that produces each result. Test code compares outputs and is category 1. The comments are a provenance record of reference-source inspection. Remediation: reword comments to state the observed behaviour and the pinned-binary version; keep the assertions.
- `docs/rrd-source-notes.md` describes the internal control flow of C functions in prose (update path, restore walk, daemon handlers) and states that `rondi::time` "ports `rrd_parsetime.c` directly". `docs/COMPATIBILITY_DEFECTS.md`, `docs/ROADMAP.md` and `docs/compatibility.md` repeat port claims. Remediation: convert into behavioural specs; keep version pins and upstream URLs for traceability.

### Low

- `crates/rondi/src/storage.rs`, `consolidation.rs`, `data_source.rs`, `format.rs`, `import_export.rs`, `queries.rs`, `lib.rs`, `compatibility.rs`, `safe_fs.rs`: Rondi's own journaled store, snapshot format and filesystem helpers. No RRDtool counterpart in structure or naming. Keep as is.
- `crates/rondi/src/vdef.rs`: whole-series aggregates (average, standard deviation, percentile, least squares) are conventional formulas (category 3). It emulates GCC and Clang floating-point contraction and qsort ordering to match oracle bits on each platform (category 1). Keep; document the contraction behaviour as a compatibility spec.
- `crates/rondi-cli/src/main.rs` PNG renderer (`render_graph_png`, `draw_*`, `set_pixel`, glyph tables): independent bitmap renderer; RRDtool uses Cairo and Pango. Keep.
- `fuzz/src/**`, fuzz targets and benches: harness code. Keep.

## Implementation quirks reproduced without a clear product reason

These are places where Rondi reproduces C implementation accidents rather than specified behaviour. Each should become an explicit, observed specification or be dropped.

| Location | Quirk | Observable? | Recommendation |
|---|---|---|---|
| `rrd_modify.rs` `REALLOC_ZERO_FAILS` | glibc `realloc(ptr, 0)` returning NULL | Error text for a zero-DS edge case | Specify the observed error per platform, or reject the input with Rondi's own message |
| `rrd_restore.rs` empty-database branch | modulo-by-zero result on aarch64; x86_64 traps | Error text on aarch64 | Specify "restore of an RRA with zero rows fails" with one message |
| `rpn.rs` MEDIAN and PERCENT windows | reads above the stack top | Result values in a malformed case | Specify observed result or reject the expression |
| `rpn.rs` `qsort`, `vdef.rs` `rrd_qsort_compatible` | host `qsort` tie order for NaN and signed zero | Sorted output order | Specify the order explicitly from observation |
| `rrd_create.rs` `mkstemp` | C temp-name alphabet | Not observable after rename | Use an ordinary unique temp name |
| `optparse.rs` `ERRMSG_SIZE` | 48-byte error buffer truncation | Error text for long option names | Specify truncation length from observation |
| `rrd_modify.rs` comment near line 945 | uninitialised definition bytes before an error | Not observable after error | Do not reproduce |
| `main.rs` `c_double_to_int` | x86 vs aarch64 out-of-range conversion | Graph geometry for extreme values | Specify per-target result or clamp |
| `rondi-server/src/lib.rs` `cache_tree_*` | AVL height for `STATS TreeDepth` | Yes | Specify as balanced-tree height; implementation may stay |

## Prioritized remediation order

Effort is the non-test line count to rewrite. "Pinned by" lists tests that already compare against the RRDtool 1.11.0 oracle or fixtures. "Spec needed" names the compatibility spec to write before the rewrite, so the new code is written from the spec and tests, not from the old code.

| # | Target | Lines | Pinned by | Spec needed |
|---:|---|---:|---|---|
| 0 | Licensing state, README "ports behavior and code" statement, help text | n/a | n/a | Legal decision (see next section) |
| 1 | `rpn.rs` | ~1,080 | `rpn_differential.rs` (11), `graph_data_differential.rs`, fuzz `rpn_diff`, `rpn_xport` | Operator table from `rrdgraph_rpn` plus observed unknown and edge cases |
| 2 | `rrd_modify.rs` | ~960 | `tune_modify_differential.rs` (5, thin) | Resampling formula and candidate selection from `rrdtune` plus new differential vectors |
| 3 | `rrd_restore.rs` with decision on `xml_reader.rs` | ~840 (+1,860) | `csrc_mutate.rs` (16), fuzz `restore_xml` | XML schema, error table, decision on libxml2 line numbers |
| 4 | `graph.rs`, `graph_layout.rs`, `graph_options.rs` | ~4,100 | `graph_data_differential.rs` (15), `graph_xport_parity.rs` (10), `graph_option_differential.rs` (8), `graph_engine_parity.rs` (5), `vdef_differential.rs` (3) | Element grammar, data preparation, layout measurements, option table |
| 5 | `rrdcached_listen.rs` and ported handlers in `rondi-server/src/lib.rs` | ~850 + ~600 | `rrdcached_listeners.rs` (13), `rrdcached_privileges.rs` (6), `rrdcached_startup.rs` (7), `rrdcached_protocol_parity.rs` (11), `server_roundtrip.rs` (35) | `rrdcached` options, protocol, journal and queue spec (GPL-2.0-only origin) |
| 6 | `rrd_create.rs` and `rrd_resize.rs` | ~970 | `csrc_create.rs` (8), `csrc_mutate.rs` | `.rrd` format spec, create argument grammar, resize row mapping |
| 7 | `optparse.rs` and `main.rs` front end (dispatch, stdin mode, list) | ~300 + ~600 | `rrdtool_optparse.rs` (25), `rrdtool_command_options.rs` (14), `alias_modes.rs` (90), `cli_list_and_aliases.rs` | Option-permutation spec, stdin tokenizer spec, list ordering |
| 8 | `time.rs` | ~850 | `parsetime_compat.rs` (15), fuzz `time_spec` | At-style grammar from `rrdfetch`, message table, DST cases |
| 9 | `main.rs` `print_calc`, `strfduration`, `time_clean` (done) | ~410 | `graph_data_differential.rs`, `number_format_differential.rs` | GPRINT and duration format spec from `rrdgraph_graph` |
| 10 | `rrd_snprintf.rs` (done) | ~600 | `number_format_differential.rs` (2), `csrc_dump.rs` (1), in-module tests | Conversion recipe plus a larger differential corpus |
| 11 | `rrd_binary.rs` update path comments and spec | ~1,000 (spec first; rewrite optional) | `csrc_update.rs` (17), `rrdtool_differential.rs` (7), `rrd_binary_probe.rs` (30) | Consolidation spec |
| 12 | Test comments and `docs/rrd-source-notes.md` | comments only | n/a | Reword to observed behaviour |

The order puts pure-GPL, self-declared ports with existing differential coverage first. `rrd_modify.rs` is second despite thin coverage because it is self-declared and recently added (#91); its tests must grow before the rewrite. `time.rs`, `optparse.rs` and `rrd_snprintf.rs` are later because their C origins carry permissive notices whose effect needs legal input before the rewrite cost is justified.

A clean-room process is the stronger form of each rewrite: one person writes the spec and tests from documentation and black-box observation, and a different person who has not read the C or the current Rust implements from the spec. If that is not practical, the minimum is a fresh implementation written against the spec with the current module removed from view.

## Areas requiring human or legal review

1. License history. The workspace was declared GPL-2.0-or-later with the RRDtool FLOSS exception on 2026-10-07 (`de820e9`) and relicensed to `MIT OR Apache-2.0` on 2026-10-14 (`36346a8`). Most self-described ports (#54, #56, #59, #63, #64, #68, #69, #70, #72, #73, #91) landed between or around those dates. Legal should determine what license covers code written while the project was declared GPL and described as ported.
2. README statement. The README says Rondi "ports behavior and code from RRDtool 1.11.0" and that RRDtool's code "is not relicensed by this project". That statement and the crate `license = "MIT OR Apache-2.0"` metadata conflict for any file that contains RRDtool code. Until resolved, the published crates' license metadata may be inaccurate.
3. Mixed-origin C files. Several reference files are not under RRDtool's plain GPL header: the client library (MIT), the decimal parser (BSD), the time parser (BSD-style with GPL modifications), the snprintf implementation (Powell and Weiss permissive grant with RRDtool header), and the bundled option parser (no notice in the RRDtool copy). The daemon file is GPL-2.0 only. Legal should confirm the applicable terms for each before rewrite priority is final.
4. Verbatim text. `crates/rondi-cli/src/help/*.txt`, `RRDCACHED_HELP`, and `RRDCACHED_COMMAND_TABLE` reproduce RRDtool output text, including a copyright banner. Decide whether byte-exact help output is a requirement worth carrying under a separate notice.
5. libxml2 modelling. `xml_reader.rs` reproduces libxml2 internals; confirm whether that creates attribution obligations under libxml2's MIT license, and whether exact libxml2 error line numbers are needed.
6. FLAG items where independent reconstruction is uncertain: the time parser's months-or-minutes guess and normalization order; RPN undefined-window results; `tune` candidate RRA selection order; restore error ordering; graph layout pixel arithmetic. Each can be specified from observation, but the current knowledge came from reading the C, so a clean-room author would need the observed spec handed over rather than rediscovering it.
7. Documentation and comments as evidence. C line citations and internal function names in source, tests and docs record that development was done by reading the C. They are accurate history. Removing them does not change provenance and should not be done as a substitute for remediation; rewording should happen as each module is rewritten.

## Method and limits

Executed:

- `git worktree add` at `origin/main` (`0fd0cf8`); `git ls-files` (260 tracked files); `wc -l` per file.
- Regex searches over all tracked Rust, Markdown, shell and workflow files for port, translation, mirror, upstream and "like C" wording, named C files, and C line-number citations; counts per file by a Python script.
- `git log --all` over 433 commits, scanned for the same patterns in subjects and bodies; `gh pr list --state all` over 100 pull requests, scanned in titles.
- Extraction of function names defined in the RRDtool 1.11.0 `src/*.c` files and in Rondi's `crates/*/src/*.rs`, and their intersection (91 shared names after excluding generic ones), counted per Rust file.
- A lowercase-identifier overlap count between each Rust module and its C counterpart. This metric is noisy because format field names are shared legitimately, so it was used only to choose what to read.
- Reading of license headers of the reference C files.

Judged by reading:

- Side-by-side reading of selected functions: the Rust number formatter against the C formatter, the Rust time scanner routines against the C time parser, and the module structure of `rrd_modify.rs`, `rrd_restore.rs`, `rrd_create.rs`, `rpn.rs`, `graph.rs`, `optparse.rs`, `rrd_binary.rs` update path, `vdef.rs`, and the server. Every other tier assignment rests on the module's own claims, shared names, and citations rather than line-by-line comparison.
- Section boundaries and line estimates for `main.rs` are approximate, taken from top-level function positions.

Not examined:

- Every function in `rondi-server/src/lib.rs` and `main.rs` line by line.
- Test fixture files and fuzz seeds beyond their names.
- Third-party crate dependencies (covered by `cargo-deny` licence checks).
- Whether any RRDtool C text was pasted and later reformatted outside the patterns searched.

Similarity does not prove copying. Shared names can come from documentation, shared output can come from black-box testing, and shared arithmetic can come from the RRD data model. Conversely, idiomatic Rust can still be a translation. This audit records engineering signals so that maintainers and counsel can decide what to keep, respecify, or rewrite. It is not a legal opinion on infringement or licensing.
