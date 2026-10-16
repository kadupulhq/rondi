//! Text measurement with the default monospace face (GR-60).
//!
//! The reference measures text through Pango at 100 dots per inch with a
//! DejaVu Sans Mono face: every character advances by the same whole
//! number of pixels, and a line is as tall as the face's ascent plus its
//! descent, each rounded up. These are the font's published metrics in
//! units of its 2048-unit em square.
//
// Compatibility: widths, heights and tab stops are fitted to `graphv`
// coordinates at legend sizes from 1 to 40 points in steps of 0.25.
// Provenance: black-box compatibility test; see GR-60 in
// docs/compatibility/graph.md.

const EM_UNITS: f64 = 2048.0;
const ADVANCE_UNITS: f64 = 1233.0;
const ASCENT_UNITS: f64 = 1901.0;
const DESCENT_UNITS: f64 = 483.0;
const DOTS_PER_POINT: f64 = 100.0 / 72.0;

fn pixels_per_em(size: f64) -> f64 {
    size * DOTS_PER_POINT
}

/// Pixels one character advances at `size` points.
pub fn advance(size: f64) -> f64 {
    let exact = pixels_per_em(size) * ADVANCE_UNITS / EM_UNITS;
    // Half a pixel rounds up, as glyph positions are hinted to whole pixels.
    (exact + 0.5).floor().max(0.0)
}

/// Height of one line of text at `size` points.
pub fn line_height(size: f64) -> f64 {
    let em = pixels_per_em(size);
    (em * ASCENT_UNITS / EM_UNITS).ceil() + (em * DESCENT_UNITS / EM_UNITS).ceil()
}

/// Width of `text` at `size` points. A tab advances to the next multiple of
/// `tab` pixels from the start of the text that lies beyond `skip` (GR-64).
pub fn width(text: &str, size: f64, tab: f64, skip: f64) -> f64 {
    let step = advance(size);
    let mut pen = 0.0_f64;
    for ch in text.chars() {
        if ch == '\t' && tab > 0.0 {
            pen = ((pen.max(skip) / tab).floor() + 1.0) * tab;
        } else {
            pen += step;
        }
    }
    pen
}

/// The visible text of Pango markup: tags removed and the five predefined
/// and numeric character entities decoded (GR-64). Markup that changes the
/// font size is not modelled.
pub fn markup_text(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(ch) = rest.chars().next() {
        match ch {
            '<' => match rest.find('>') {
                Some(end) => rest = &rest[end + 1..],
                None => {
                    out.push_str(rest);
                    break;
                }
            },
            '&' => {
                let decoded = rest.find(';').and_then(|end| {
                    let name = &rest[1..end];
                    let ch = match name {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => name
                            .strip_prefix("#x")
                            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                            .or_else(|| name.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                            .and_then(char::from_u32),
                    }?;
                    Some((ch, end + 1))
                });
                match decoded {
                    Some((ch, used)) => {
                        out.push(ch);
                        rest = &rest[used..];
                    }
                    None => {
                        out.push('&');
                        rest = &rest[1..];
                    }
                }
            }
            _ => {
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_match_measured_sizes() {
        // (size, advance, height) read from graphv legend coordinates.
        for (size, step, height) in [
            (1.0, 1.0, 3.0),
            (4.0, 3.0, 8.0),
            (4.25, 4.0, 8.0),
            (6.25, 5.0, 12.0),
            (7.0, 6.0, 13.0),
            (8.0, 7.0, 14.0),
            (9.0, 8.0, 15.0),
            (9.25, 8.0, 16.0),
            (10.0, 8.0, 17.0),
            (12.0, 10.0, 20.0),
            (15.0, 13.0, 25.0),
            (20.0, 17.0, 33.0),
            (40.0, 33.0, 66.0),
        ] {
            assert_eq!(advance(size), step, "advance at {size}");
            assert_eq!(line_height(size), height, "height at {size}");
        }
    }

    #[test]
    fn tabs_advance_to_the_next_stop() {
        assert_eq!(width("a\tb", 8.0, 56.0, 16.0), 63.0);
        assert_eq!(
            width(&format!("{}\tb", "a".repeat(9)), 8.0, 56.0, 16.0),
            119.0
        );
        assert_eq!(width("\t", 8.0, 0.0, 16.0), 7.0);
        assert_eq!(width("café", 8.0, 56.0, 16.0), 28.0);
        // Stops at or before the skip are passed over.
        assert_eq!(width("\tb", 8.0, 10.0, 16.0), 27.0);
        assert_eq!(width("aaa\tb", 8.0, 10.0, 16.0), 37.0);
    }

    #[test]
    fn markup_is_reduced_to_its_text() {
        assert_eq!(markup_text("<b>bold</b> x"), "bold x");
        assert_eq!(markup_text("&amp;x&lt;&gt;&quot;&apos;"), "&x<>\"'");
        assert_eq!(markup_text("&#65;&#x42;&bogus;&"), "AB&bogus;&");
        assert_eq!(markup_text("a<b"), "a<b");
    }
}
