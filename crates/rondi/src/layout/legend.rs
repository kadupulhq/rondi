//! Legend items, lines and their positions (GR-64 to GR-66).

use super::text;

/// How an unterminated legend line is aligned (`TEXTALIGN`, GR-56).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
    #[default]
    Justified,
}

/// Which way legend lines are stacked (GR-65).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    #[default]
    TopDown,
    BottomUp,
    /// Like `BottomUp`, but the comment-only lines before the first and
    /// after the last line with a graph element keep their places.
    BottomUpKeepComments,
}

/// One legend entry as the graph elements give it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The element the entry belongs to; reported back with its position.
    pub element: usize,
    /// The text including any trailing marker such as `\l`.
    pub text: String,
    /// Whether the entry comes from a `COMMENT`.
    pub comment: bool,
    /// The `TEXTALIGN` in force for this entry.
    pub align: Align,
}

/// A placed legend item. `left` and `bottom` are in image pixels; the text
/// box spans `left..left + width` and `bottom - height..bottom`.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub element: usize,
    pub text: String,
    pub left: f64,
    pub bottom: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// `\l` or `\n`.
    Left,
    Right,
    Center,
    Justify,
    /// `\s`: a short line advance.
    Space,
    /// `\u`: move back up.
    Up,
    /// The line was full, or the legend ended.
    Wrap,
}

#[derive(Clone, Debug)]
struct Item {
    entry: usize,
    text: String,
    width: f64,
    /// Whether the usual gap follows the item (no `\g`).
    gap_after: bool,
}

#[derive(Clone, Debug)]
struct Line {
    items: Vec<Item>,
    end: End,
    align: Align,
}

/// The sizes the legend uses, all from the legend font (GR-60, GR-66).
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub size: f64,
    pub tab: f64,
    pub markup: bool,
}

impl Metrics {
    /// Margin at both sides of the legend and the gap between items.
    pub fn gap(&self) -> f64 {
        // Compatibility: whole pixels (16.5 for 8.25 points is 16).
        // Provenance: black-box compatibility test (layout fuzzing against
        // the oracle with fractional legend sizes).
        (2.0 * self.size).floor()
    }

    fn line_advance(&self) -> i64 {
        (1.8 * self.size).floor() as i64
    }

    /// How far `\u` moves back up over a line that is not the first.
    fn up_advance(&self) -> i64 {
        (1.8 * self.size).ceil() as i64
    }

    // Compatibility: `\s` advances a line less the size rounded up.
    // Provenance: black-box compatibility test (the `\s` probes at legend
    // sizes from 3 to 20 points recorded under GR-66).
    fn short_advance(&self) -> i64 {
        self.line_advance() - self.size.ceil() as i64
    }

    /// Space below the last line.
    // Compatibility: 0.6 of the whole-pixel margin.
    // Provenance: black-box compatibility test (image heights at legend
    // sizes from 1 to 40 points in steps of 0.25).
    pub fn trailer(&self) -> i64 {
        (0.6 * self.gap()).floor() as i64
    }

    fn text_width(&self, text: &str) -> f64 {
        if self.markup {
            text::width(&text::markup_text(text), self.size, self.tab, self.gap())
        } else {
            text::width(text, self.size, self.tab, self.gap())
        }
    }
}

/// Splits the marker off `text` (GR-66): only the last two characters are
/// examined. A backslash before any other last character is an error; a
/// lone trailing backslash is text.
fn split_marker(text: &str) -> Result<(&str, Option<char>), String> {
    let mut chars = text.char_indices().rev();
    match (chars.next(), chars.next()) {
        (Some((_, marker)), Some((at, '\\'))) if "lnrcjsug.".contains(marker) => {
            Ok((&text[..at], Some(marker)))
        }
        (Some(_), Some((_, '\\'))) => Err(format!("Unknown control code at the end of '{text}'")),
        _ => Ok((text, None)),
    }
}

/// The width a line takes for alignment: its items plus the gap of each
/// item after the first that is not glued (GR-66).
//
// Compatibility: the fill counts the gap of each later item without `\g`,
// while drawing puts the gap after each item without `\g`.
// Provenance: black-box compatibility test (legend-031 to -044).
fn fill(items: &[Item], gap: f64) -> f64 {
    items.iter().map(|item| item.width).sum::<f64>()
        + gap * items.iter().skip(1).filter(|item| item.gap_after).count() as f64
}

/// Groups entries into lines. `width` is the room for a line, or `None`
/// for a legend beside the graph, whose lines only end at markers.
fn lines(entries: &[Entry], metrics: &Metrics, width: Option<f64>) -> Result<Vec<Line>, String> {
    let gap = metrics.gap();
    let mut done = Vec::new();
    let mut current: Vec<Item> = Vec::new();
    let mut align = Align::default();
    let mut open = false;
    for (index, entry) in entries.iter().enumerate() {
        open = true;
        // Compatibility: the two characters `\t` stand for a tab.
        // Provenance: black-box compatibility test (comment-016, -017).
        let text = entry.text.replace("\\t", "\t");
        let (body, marker) = split_marker(&text)?;
        let glue = marker == Some('g');
        let body = if glue {
            body.trim_end_matches(' ')
        } else {
            body
        };
        align = entry.align;
        if !body.is_empty() {
            let item_width = metrics.text_width(body);
            // Compatibility: the fit test uses the line's fill (GR-66). An
            // entry that ends the line always joins it; any other entry
            // starts a new line when the fill with it would not fit.
            // Provenance: black-box compatibility test (the wrap probes
            // recorded under GR-66; layout-004, -011, -012).
            let ends_line = matches!(marker, Some('l' | 'n' | 'r' | 'c' | 'j' | 's' | 'u'));
            let mut wrapped = false;
            if let Some(room) = width {
                let after = fill(&current, gap) + item_width + if glue { 0.0 } else { gap };
                if !current.is_empty() && !ends_line && after > room {
                    done.push(Line {
                        items: std::mem::take(&mut current),
                        end: End::Wrap,
                        align,
                    });
                    wrapped = true;
                }
            }
            // Compatibility: an entry moved to a new line by the wrap loses
            // its `\g`, so a gap follows it.
            // Provenance: black-box compatibility test (layout fuzzing
            // against the oracle; a wrapped 45-character `\g` comment
            // followed by a short entry).
            current.push(Item {
                entry: index,
                text: body.to_owned(),
                width: item_width,
                gap_after: !glue || wrapped,
            });
            // Compatibility: an entry too wide on its own ends its line at
            // once, aligned as TEXTALIGN says at that point.
            // Provenance: black-box compatibility test (layout fuzzing
            // against the oracle; a 45-character entry in a 50-pixel graph
            // before and after `TEXTALIGN:right`).
            if !ends_line && width.is_some_and(|room| item_width > room) {
                done.push(Line {
                    items: std::mem::take(&mut current),
                    end: End::Wrap,
                    align,
                });
                open = false;
            }
        }
        let end = match marker {
            Some('l' | 'n') => End::Left,
            Some('r') => End::Right,
            Some('c') => End::Center,
            Some('j') => End::Justify,
            Some('s') => End::Space,
            Some('u') => End::Up,
            _ => continue,
        };
        done.push(Line {
            items: std::mem::take(&mut current),
            end,
            align,
        });
        open = false;
    }
    // Compatibility: any element after the last marker, even one without a
    // legend, opens one more line; empty, it takes room only when right
    // aligned or centered (see advance).
    // Provenance: black-box compatibility test (layout fuzzing against the
    // oracle; `TEXTALIGN:right` and `CDEF` after the last `\l`).
    if open {
        done.push(Line {
            items: current,
            end: End::Wrap,
            align,
        });
    }
    Ok(done)
}

/// How a line is aligned: by its marker, or by `TEXTALIGN` without one.
fn line_align(line: &Line) -> Align {
    match line.end {
        End::Left => Align::Left,
        End::Right => Align::Right,
        End::Center => Align::Center,
        End::Justify => Align::Justified,
        // Compatibility: `\s` and `\u` lines are left aligned.
        // Provenance: black-box compatibility test (layout fuzzing
        // against the oracle).
        End::Space | End::Up => Align::Left,
        End::Wrap => line.align,
    }
}

/// Vertical advance after a line (GR-66). A line without items advances
/// only for `\s` and `\u`, or when it is right aligned or centered.
//
// Compatibility: an empty right aligned or centered line takes a whole line.
// Provenance: black-box compatibility test (`COMMENT:\r` and `COMMENT:\c`
// between two lines, and `TEXTALIGN:right` with no legend text).
//
// Compatibility: `\u` moves back up over the line with items it ends or
// follows: by ceil(1.8·size) when that line is not the first, otherwise,
// and after an empty line, by floor(1.8·size). A line with items that ends
// in `\u` first advances as usual, so the next line is 0 or 1 px higher.
// Provenance: black-box compatibility test (legend-005 and the `\u` probes
// at legend sizes 5 to 20 recorded under GR-66).
fn advance(lines: &[Line], index: usize, metrics: &Metrics) -> i64 {
    let line = &lines[index];
    let filled = !line.items.is_empty();
    let up_over = |line: usize| {
        if line > 0 && !lines[line].items.is_empty() {
            metrics.up_advance()
        } else {
            metrics.line_advance()
        }
    };
    match line.end {
        End::Space => metrics.short_advance(),
        End::Up if filled => metrics.line_advance() - up_over(index),
        End::Up => -index.checked_sub(1).map_or(metrics.line_advance(), up_over),
        _ if filled || matches!(line_align(line), Align::Right | Align::Center) => {
            metrics.line_advance()
        }
        _ => 0,
    }
}

/// The legend laid out.
#[derive(Clone, Debug, Default)]
pub struct Laid {
    pub placed: Vec<Placed>,
    /// Total vertical advance of all lines; negative when `\\u` moves
    /// back above the first line.
    pub height: i64,
    /// Width of the widest line (legends beside the graph).
    pub width: f64,
}

/// Places `entries`. For a legend above or below the graph, `area` is the
/// horizontal span lines may fill; beside the graph it is `None` and lines
/// start at `left`. `first_bottom` is the bottom of the first line.
pub fn place(
    entries: &[Entry],
    metrics: &Metrics,
    area: Option<(f64, f64)>,
    left: f64,
    first_bottom: i64,
    order: Order,
) -> Result<Laid, String> {
    let gap = metrics.gap();
    let lines = lines(entries, metrics, area.map(|(from, to)| to - from))?;
    let height_of_text = text::line_height(metrics.size);
    // Compatibility: a bottom-up order mirrors the positions of items within
    // the lines they occupy, so the space between two lines stays with the
    // pair.
    // Provenance: black-box compatibility test (`bottomup` with `\s` lines,
    // and `bottomup` and `bottomup2` with `TEXTALIGN:right` after the last
    // line).
    //
    let items: Vec<(usize, &Item)> = lines
        .iter()
        .enumerate()
        .flat_map(|(index, line)| line.items.iter().map(move |item| (index, item)))
        .collect();
    let flipped = match order {
        Order::TopDown => 0..0,
        Order::BottomUp => 0..items.len(),
        // Compatibility: comments before the first and after the last
        // graph element keep their places, even within a line that also
        // holds mirrored items.
        // Provenance: black-box compatibility test (`bottomup2` with two
        // leading and two trailing comment lines, and with comments sharing
        // a line with graph elements).
        Order::BottomUpKeepComments => {
            let graph = |(_, item): &&(usize, &Item)| !entries[item.entry].comment;
            match (
                items.iter().position(|item| graph(&item)),
                items.iter().rposition(|item| graph(&item)),
            ) {
                (Some(first), Some(last)) => first..last + 1,
                _ => 0..0,
            }
        }
    };
    let width = lines
        .iter()
        .map(|line| fill(&line.items, gap))
        .fold(0.0, f64::max);
    // Beside the graph the lines align within the widest one.
    let (from, to) = area.unwrap_or((left, left + width));
    let room = to - from;
    let mut cursor = first_bottom;
    let mut bottoms = Vec::with_capacity(lines.len());
    let mut starts = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let fill = fill(&line.items, gap);
        let (x, spread) = match line_align(line) {
            Align::Left => (from, 0.0),
            Align::Right => (to - fill, 0.0),
            Align::Center => (from + (room - fill) / 2.0, 0.0),
            Align::Justified if line.items.len() > 1 => {
                (from, (room - fill) / (line.items.len() - 1) as f64)
            }
            Align::Justified => (from, 0.0),
        };
        bottoms.push(cursor);
        starts.push((x, spread));
        // Compatibility: a right aligned line takes no room when the image
        // ends at or before the legend margin, and a justified line takes
        // none when its items overlap by more than the room and a gap.
        // Provenance: black-box compatibility test (layout fuzzing against
        // the oracle; -D with -w 15, 16 and 20 and `\r` lines; justified
        // lines of two and three items at legend sizes 8 and 20 around the
        // threshold).
        let takes_no_room = match line_align(line) {
            Align::Right => to <= 0.0,
            Align::Justified => line.items.len() > 1 && spread < -(room + gap),
            _ => false,
        };
        if !takes_no_room {
            cursor += advance(&lines, index, metrics);
        }
    }
    // Mirrored within the lines of the flipped items, so the space between
    // two lines stays with the pair.
    let mirror = items[flipped.clone()]
        .first()
        .zip(items[flipped.clone()].last())
        .map_or(0, |(first, last)| bottoms[first.0] + bottoms[last.0]);
    let mut placed: Vec<Placed> = Vec::new();
    let mut number = 0;
    for ((line, bottom), (mut x, spread)) in lines.iter().zip(&bottoms).zip(starts) {
        for item in &line.items {
            let bottom = if flipped.contains(&number) {
                mirror - bottom
            } else {
                *bottom
            };
            number += 1;
            placed.push(Placed {
                element: entries[item.entry].element,
                text: item.text.clone(),
                left: x,
                bottom: bottom as f64,
                width: item.width,
                height: height_of_text,
            });
            x += item.width + if item.gap_after { gap } else { 0.0 } + spread;
        }
    }
    placed.sort_by_key(|item| item.element);
    Ok(Laid {
        placed,
        height: cursor - first_bottom,
        width,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: Metrics = Metrics {
        size: 8.0,
        tab: 56.0,
        markup: false,
    };

    fn entries(texts: &[&str]) -> Vec<Entry> {
        texts
            .iter()
            .enumerate()
            .map(|(element, text)| Entry {
                element,
                text: (*text).to_owned(),
                comment: true,
                align: Align::Justified,
            })
            .collect()
    }

    fn lefts(texts: &[&str]) -> Vec<(f64, f64)> {
        place(
            &entries(texts),
            &METRICS,
            Some((16.0, 465.0)),
            16.0,
            148,
            Order::TopDown,
        )
        .unwrap()
        .placed
        .iter()
        .map(|item| (item.left, item.bottom))
        .collect()
    }

    #[test]
    fn justified_lines_share_the_free_space() {
        assert_eq!(
            lefts(&["a", "b", "c"]),
            vec![(16.0, 148.0), (237.0, 148.0), (458.0, 148.0)]
        );
        assert_eq!(
            lefts(&["a\\g", "b", "c"]),
            vec![(16.0, 148.0), (221.0, 148.0), (442.0, 148.0)]
        );
        assert_eq!(
            lefts(&["a", "b\\g", "c"]),
            vec![(16.0, 148.0), (245.0, 148.0), (458.0, 148.0)]
        );
        assert_eq!(lefts(&["a  \\g", "b"]), vec![(16.0, 148.0), (442.0, 148.0)]);
    }

    #[test]
    fn markers_end_and_align_lines() {
        assert_eq!(
            lefts(&["a\\r", "b\\l", "c\\c", "d\\j"]),
            vec![(458.0, 148.0), (16.0, 162.0), (237.0, 176.0), (16.0, 190.0)]
        );
        assert_eq!(
            lefts(&["a\\g", "b\\c"]),
            vec![(225.5, 148.0), (232.5, 148.0)]
        );
        assert_eq!(lefts(&["b\\.\\l"])[0], (16.0, 148.0));
    }

    #[test]
    fn short_and_up_markers_move_the_cursor() {
        assert_eq!(lefts(&["a\\l", "\\s", "b\\l"])[1], (16.0, 168.0));
        assert_eq!(lefts(&["left\\l", "\\u", "right\\r"])[1], (430.0, 148.0));
        assert_eq!(lefts(&["a\\l", "b\\u", "c\\l"])[2], (16.0, 161.0));
        assert_eq!(lefts(&["\\l", "\\l", "x"])[0], (16.0, 148.0));
        // An empty `\\u` after a line that is not the first moves up by
        // ceil(1.8·size); after the first line by floor(1.8·size).
        assert_eq!(lefts(&["a\\l", "b\\l", "\\u", "c\\l"])[2], (16.0, 161.0));
        assert_eq!(lefts(&["a\\l", "\\u", "b\\l"])[1], (16.0, 148.0));
        // When 1.8·size is whole, both are the same.
        let size_10 = Metrics {
            size: 10.0,
            ..METRICS
        };
        let laid = place(
            &entries(&["a\\l", "b\\u", "c\\l"]),
            &size_10,
            Some((16.0, 465.0)),
            16.0,
            148,
            Order::TopDown,
        )
        .unwrap();
        assert_eq!(laid.placed[2].bottom, 166.0);
        // The height may be negative.
        let up = place(
            &entries(&["\\u"]),
            &METRICS,
            Some((16.0, 465.0)),
            16.0,
            148,
            Order::TopDown,
        );
        assert_eq!(up.unwrap().height, -14);
    }

    #[test]
    fn control_codes_and_tabs() {
        let place_one = |text: &str| {
            place(
                &entries(&[text]),
                &METRICS,
                Some((16.0, 465.0)),
                16.0,
                148,
                Order::TopDown,
            )
        };
        assert_eq!(
            place_one("x\\q").unwrap_err(),
            "Unknown control code at the end of 'x\\q'"
        );
        assert_eq!(place_one("x\\").unwrap().placed[0].text, "x\\");
        let tab = place_one("a\\tb").unwrap();
        assert_eq!(
            (tab.placed[0].text.as_str(), tab.placed[0].width),
            ("a\tb", 63.0)
        );
        let long = "x".repeat(60);
        let joined = lefts(&[&long, "b\\l"]);
        assert_eq!(joined[1].1, 148.0);
    }

    #[test]
    fn full_lines_wrap() {
        let long = "x".repeat(32);
        let got = lefts(&[&long, &long, "a"]);
        assert_eq!(got, vec![(16.0, 148.0), (16.0, 162.0), (458.0, 162.0)]);
        let laid = place(
            &entries(&[&long, &long]),
            &METRICS,
            None,
            497.0,
            31,
            Order::TopDown,
        )
        .unwrap();
        assert_eq!(laid.placed[1].left, 497.0 + 224.0 + 16.0);
        assert_eq!(laid.width, 464.0);
    }

    #[test]
    fn bottom_up_orders_reverse_the_lines() {
        let texts = ["head\\l", "a\\l", "b\\l", "foot\\l"];
        let mut items = entries(&texts);
        items[1].comment = false;
        items[2].comment = false;
        let bottoms = |order| {
            place(&items, &METRICS, Some((16.0, 465.0)), 16.0, 148, order)
                .unwrap()
                .placed
                .iter()
                .map(|item| item.bottom)
                .collect::<Vec<_>>()
        };
        assert_eq!(bottoms(Order::BottomUp), vec![190.0, 176.0, 162.0, 148.0]);
        assert_eq!(
            bottoms(Order::BottomUpKeepComments),
            vec![148.0, 176.0, 162.0, 190.0]
        );
    }

    fn laid(items: &[Entry], area: (f64, f64), order: Order) -> Laid {
        place(items, &METRICS, Some(area), area.0, 148, order).unwrap()
    }

    #[test]
    fn bottom_up_mirrors_items_within_their_lines() {
        // The `\s` space stays between the first two lines.
        let bottoms = |items: &[Entry], order| {
            laid(items, (16.0, 465.0), order)
                .placed
                .iter()
                .map(|item| item.bottom)
                .collect::<Vec<_>>()
        };
        let spaced = entries(&["a\\s", "b\\l", "c\\l"]);
        assert_eq!(bottoms(&spaced, Order::BottomUp), vec![168.0, 162.0, 148.0]);
        // A leading comment keeps its place on a line it shares with
        // mirrored items.
        let wide = format!("{}\\r", "y".repeat(45));
        let mut shared = entries(&["bb", "ccc", &wide, "sp\\j"]);
        shared[1].comment = false;
        shared[3].comment = false;
        assert_eq!(
            bottoms(&shared, Order::BottomUpKeepComments),
            vec![148.0, 162.0, 162.0, 148.0]
        );
        // Without a graph element nothing moves.
        assert_eq!(
            bottoms(&entries(&["a\\l", "b\\l"]), Order::BottomUpKeepComments),
            vec![148.0, 162.0]
        );
    }

    #[test]
    fn empty_lines_take_room_when_right_or_centered() {
        assert_eq!(lefts(&["a\\l", "\\r", "b\\l"])[1], (16.0, 176.0));
        assert_eq!(lefts(&["a\\l", "\\c", "b\\l"])[1], (16.0, 176.0));
        assert_eq!(lefts(&["a\\l", "\\l", "b\\l"])[1], (16.0, 162.0));
        // An element after the last marker opens a line that takes the
        // alignment in force.
        let mut items = entries(&["a\\l", ""]);
        assert_eq!(laid(&items, (16.0, 465.0), Order::TopDown).height, 14);
        items[1].align = Align::Right;
        assert_eq!(laid(&items, (16.0, 465.0), Order::TopDown).height, 28);
        items.pop();
        assert_eq!(laid(&items, (16.0, 465.0), Order::TopDown).height, 14);
    }

    #[test]
    fn wide_entries_end_their_line() {
        let wide = "y".repeat(45);
        // Too wide on its own: the line ends before TEXTALIGN changes.
        let mut items = entries(&[&wide, "", "a"]);
        items[1].align = Align::Right;
        items[2].align = Align::Right;
        let placed = laid(&items, (16.0, 115.0), Order::TopDown).placed;
        assert_eq!((placed[0].left, placed[1].left), (16.0, 108.0));
        // A wrapped entry loses its `\g`.
        let x = format!("{}\\g", "x".repeat(20));
        let y = format!("{wide}\\g");
        assert_eq!(lefts(&[&x, &y, "c"])[2], (458.0, 162.0));
    }

    #[test]
    fn some_lines_take_no_room() {
        // A justified line whose items overlap by more than the room and a
        // gap.
        let fits = format!("{}\\j", "y".repeat(124));
        let overlaps = format!("{}\\j", "y".repeat(126));
        let height =
            |text: &str| laid(&entries(&["ccc", text]), (16.0, 465.0), Order::TopDown).height;
        assert_eq!(height(&fits), 14);
        assert_eq!(height(&overlaps), 0);
        // A right aligned line in an image that ends before the margin.
        let narrow = laid(&entries(&["m\\r", "n\\r"]), (16.0, 0.0), Order::TopDown);
        assert_eq!(narrow.height, 0);
        assert_eq!(narrow.placed[1].bottom, 148.0);
    }
}
