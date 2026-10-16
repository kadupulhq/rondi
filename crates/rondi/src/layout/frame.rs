//! Image size, graph area and legend position (GR-61 to GR-66).

use super::legend::{self, Entry, Laid, Metrics, Order, Placed};
use super::text;

/// Font sizes in points per text kind (`--font`, GR-23).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    pub title: f64,
    pub axis: f64,
    pub unit: f64,
    pub legend: f64,
    pub watermark: f64,
}

impl Default for Sizes {
    fn default() -> Self {
        Self {
            title: 9.0,
            axis: 7.0,
            unit: 8.0,
            legend: 8.0,
            watermark: 5.5,
        }
    }
}

/// Where the legend goes (`--legend-position`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    North,
    #[default]
    South,
    East,
    West,
}

/// Everything about the image that decides its geometry.
#[derive(Clone, Debug)]
pub struct Options {
    pub width: i64,
    pub height: i64,
    pub only_graph: bool,
    pub full_size: bool,
    pub show_legend: bool,
    pub placement: Placement,
    pub order: Order,
    pub title: Option<String>,
    pub vertical_label: bool,
    pub right_axis: bool,
    pub right_axis_label: bool,
    pub watermark: bool,
    /// Characters reserved for the y-axis labels.
    pub units_length: i64,
    /// Whether the x grid and its labels are drawn.
    pub x_labels: bool,
    pub rrdtool_tag: bool,
    pub sizes: Sizes,
    /// `--tabwidth` in points (GR-64).
    pub tab_width: f64,
    /// Whether tab stops include the legend margin; not so when the y-axis
    /// labels are hidden and no units length is given (GR-64).
    pub tab_margin: bool,
    pub markup: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 400,
            height: 100,
            only_graph: false,
            full_size: false,
            show_legend: true,
            placement: Placement::South,
            order: Order::TopDown,
            title: None,
            vertical_label: false,
            right_axis: false,
            right_axis_label: false,
            watermark: false,
            units_length: 6,
            x_labels: true,
            rrdtool_tag: true,
            sizes: Sizes::default(),
            tab_width: 40.0,
            tab_margin: true,
            markup: false,
        }
    }
}

/// The geometry `graphv` reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub graph_left: i64,
    pub graph_top: i64,
    pub graph_width: i64,
    pub graph_height: i64,
    pub image_width: i64,
    pub image_height: i64,
    /// Legend items in element order.
    pub legend: Vec<Placed>,
}

/// Margin around the image and gap before the right edge.
const MARGIN: i64 = 15;
/// Width of the vertical RRDtool tag.
const TAG: i64 = 15;
/// Space under the graph when there is no legend.
const NO_LEGEND_PAD: i64 = 7;
/// Most title lines that take room (GR-63).
const MOST_TITLE_LINES: usize = 3;

/// Title lines: one more than the line breaks that follow some text
/// (GR-63).
//
// Compatibility: a break right after another break, or at the start, adds
// no line; `\n`, `<br>` and `<br/>` break; at most three lines count.
// Provenance: black-box compatibility test (layout-015 to -017 and the
// title probes recorded under GR-63).
fn title_lines(title: &str) -> usize {
    const BREAKS: [&str; 3] = ["\\n", "<br/>", "<br>"];
    let mut lines = 1;
    let mut text_since_break = false;
    let mut rest = title;
    while !rest.is_empty() {
        if let Some(found) = BREAKS.iter().find(|mark| rest.starts_with(**mark)) {
            if text_since_break {
                lines += 1;
            }
            text_since_break = false;
            rest = &rest[found.len()..];
        } else {
            text_since_break = true;
            let ch = rest.chars().next().map_or(1, char::len_utf8);
            rest = &rest[ch..];
        }
    }
    lines.min(MOST_TITLE_LINES)
}

fn floor(value: f64) -> i64 {
    value.floor() as i64
}

/// The fixed parts of the image around the graph and the legend metrics
/// (GR-60 to GR-63).
struct Parts {
    metrics: Metrics,
    /// The legend margin and gap.
    border: f64,
    /// Space above the graph: the title, or the margin without one.
    top: i64,
    /// Space left of the graph.
    left: i64,
    /// Height of the x-axis labels below the graph.
    x_labels: i64,
    /// Width of the right axis and its labels.
    right_axis: i64,
    /// Space right of the graph or its right axis.
    right_edge: i64,
    tag: i64,
    watermark: i64,
}

impl Parts {
    fn new(options: &Options) -> Self {
        let sizes = options.sizes;
        let metrics = Metrics {
            size: sizes.legend,
            // Compatibility: a tab advances by the tab width in pixels plus
            // the legend margin, or by the tab width alone when the y-axis
            // labels are hidden and no units length is given; either way a
            // stop lies beyond the margin.
            // Provenance: black-box compatibility test (legend-009, -010 and
            // the tab probes recorded under GR-64; -y none with and without
            // -L at legend sizes 5, 8, 10 and 20 and -T 10, 13 and 40).
            tab: if options.tab_margin {
                (options.tab_width + 2.0 * sizes.legend).floor()
            } else {
                options.tab_width
            },
            markup: options.markup,
        };
        let units = options.units_length * text::advance(sizes.axis) as i64;
        let either = |set: bool, value: i64, otherwise: i64| if set { value } else { otherwise };
        Self {
            border: metrics.gap(),
            metrics,
            top: match options.title.as_deref().filter(|title| !title.is_empty()) {
                Some(title) => floor(sizes.title * 1.6 * (title_lines(title) + 1) as f64),
                None => MARGIN,
            },
            left: MARGIN + either(options.vertical_label, floor(2.0 * sizes.unit), 0) + units,
            x_labels: either(options.x_labels, floor(2.5 * sizes.axis), 0),
            right_axis: either(options.right_axis, MARGIN + units, 0),
            right_edge: either(options.right_axis_label, floor(2.0 * sizes.unit), MARGIN),
            tag: either(options.rrdtool_tag, TAG, 0),
            watermark: either(options.watermark, floor(2.0 * sizes.watermark), 0),
        }
    }

    /// Bottom of the first legend line when the lines start below `y`.
    fn first_line(&self, y: i64) -> i64 {
        floor(y as f64 + 2.0 * self.metrics.size)
    }
}

/// Lays out the image (GR-61 to GR-66).
pub fn frame(options: &Options, entries: &[Entry]) -> Result<Frame, String> {
    if options.only_graph {
        return Ok(Frame {
            graph_left: 0,
            graph_top: 0,
            graph_width: options.width,
            graph_height: options.height,
            image_width: options.width,
            image_height: options.height,
            legend: Vec::new(),
        });
    }
    let parts = Parts::new(options);
    match options.placement {
        Placement::East | Placement::West if options.show_legend => {
            beside(options, entries, &parts)
        }
        Placement::North if options.show_legend => above(options, entries, &parts),
        _ => below(options, entries, &parts),
    }
}

/// A legend beside the graph (GR-65). It is measured first, since its width
/// moves the graph.
fn beside(options: &Options, entries: &[Entry], parts: &Parts) -> Result<Frame, String> {
    let border = parts.border;
    let first_bottom = parts.first_line(parts.top);
    let place = |x: f64| {
        legend::place(
            entries,
            &parts.metrics,
            None,
            x,
            first_bottom,
            options.order,
        )
    };
    let legend_width = place(0.0)?.width;
    let (graph_width, graph_height) = if options.full_size {
        let used = parts.left
            + parts.tag
            + parts.right_axis
            + parts.right_edge
            + legend_width as i64
            + 2 * border as i64;
        (
            (options.width - used).max(1),
            (options.height - parts.top - parts.x_labels - parts.watermark).max(1),
        )
    } else {
        (options.width, options.height)
    };
    let east = options.placement == Placement::East;
    let (graph_left, legend_left) = if east {
        let graph_left = parts.left + parts.tag;
        let legend_left =
            (graph_left + graph_width + parts.right_axis + parts.right_edge) as f64 + border;
        (graph_left, legend_left)
    } else {
        (parts.left + legend_width as i64 + 2 * border as i64, border)
    };
    let image_width = if options.full_size {
        options.width
    } else if east {
        (legend_left + legend_width + border) as i64
    } else {
        graph_left + graph_width + parts.right_axis + parts.right_edge + parts.tag
    };
    let image_height = if options.full_size {
        options.height
    } else {
        parts.top + graph_height + parts.x_labels + parts.watermark
    };
    Ok(Frame {
        graph_left,
        graph_top: parts.top,
        graph_width,
        graph_height,
        image_width,
        image_height,
        legend: place(legend_left)?.placed,
    })
}

/// The image width and the graph width for a legend above or below.
fn widths(options: &Options, parts: &Parts) -> (i64, i64) {
    let fixed = parts.left + parts.right_axis + parts.right_edge + parts.tag;
    if options.full_size {
        (options.width, (options.width - fixed).max(1))
    } else {
        (fixed + options.width, options.width)
    }
}

/// The span legend lines fill above or below the graph: the image width
/// less the legend margin at each side.
fn area(image_width: i64, parts: &Parts) -> Option<(f64, f64)> {
    Some((parts.border, image_width as f64 - parts.border))
}

/// A legend above the graph (GR-65).
fn above(options: &Options, entries: &[Entry], parts: &Parts) -> Result<Frame, String> {
    let (image_width, graph_width) = widths(options, parts);
    let laid = legend::place(
        entries,
        &parts.metrics,
        area(image_width, parts),
        parts.border,
        parts.first_line(parts.top),
        options.order,
    )?;
    let graph_top = parts.top + laid.height + parts.metrics.trailer();
    let graph_height = if options.full_size {
        (options.height - graph_top - parts.x_labels - parts.watermark).max(1)
    } else {
        options.height
    };
    Ok(Frame {
        graph_left: parts.left,
        graph_top,
        graph_width,
        graph_height,
        image_width,
        image_height: if options.full_size {
            options.height
        } else {
            graph_top + graph_height + parts.x_labels + parts.watermark
        },
        legend: laid.placed,
    })
}

/// A legend below the graph, or none (GR-61, GR-65).
fn below(options: &Options, entries: &[Entry], parts: &Parts) -> Result<Frame, String> {
    let (image_width, graph_width) = widths(options, parts);
    // The lines' positions depend on the graph height, which in full-size
    // mode depends on the legend's height: measure first.
    let measure = |graph_height: i64| {
        legend::place(
            entries,
            &parts.metrics,
            area(image_width, parts),
            parts.border,
            parts.first_line(parts.top + graph_height + parts.x_labels),
            options.order,
        )
    };
    let block = |laid: &Laid| {
        if options.show_legend {
            laid.height + parts.metrics.trailer()
        } else if options.full_size {
            // Compatibility: full-size mode leaves one more pixel.
            // Provenance: black-box compatibility test
            // (graph_data_differential -D -g and the probes recorded under
            // GR-62).
            NO_LEGEND_PAD + 1
        } else {
            NO_LEGEND_PAD
        }
    };
    let fixed = parts.top + parts.x_labels + parts.watermark;
    let graph_height = if options.full_size {
        (options.height - fixed - block(&measure(0)?)).max(1)
    } else {
        options.height
    };
    let laid = measure(graph_height)?;
    Ok(Frame {
        // Compatibility: without a legend, an east placement still moves
        // the graph right by the tag's width, as it does with a legend.
        // Provenance: black-box compatibility test (layout fuzzing against
        // the oracle; -g with --legend-position east, with and without the
        // tag).
        graph_left: if options.placement == Placement::East {
            parts.left + parts.tag
        } else {
            parts.left
        },
        graph_top: parts.top,
        graph_width,
        graph_height,
        image_width,
        image_height: if options.full_size {
            options.height
        } else {
            fixed + graph_height + block(&laid)
        },
        legend: if options.show_legend {
            laid.placed
        } else {
            Vec::new()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::legend::Align;

    fn entry(element: usize, text: &str) -> Entry {
        Entry {
            element,
            text: text.to_owned(),
            comment: false,
            align: Align::Justified,
        }
    }

    fn sample() -> Vec<Entry> {
        vec![entry(0, "  alpha"), entry(1, " 10.43"), entry(2, "end\\n")]
    }

    fn geometry(frame: &Frame) -> [i64; 6] {
        [
            frame.graph_left,
            frame.graph_top,
            frame.graph_width,
            frame.graph_height,
            frame.image_width,
            frame.image_height,
        ]
    }

    #[test]
    fn tab_stops_follow_the_y_axis_labels() {
        let width = |tab_margin: bool| {
            let options = Options {
                tab_width: 10.0,
                tab_margin,
                ..Options::default()
            };
            frame(&options, &[entry(0, "\\tz\\l")]).unwrap().legend[0].width
        };
        // Stops at multiples of 26, or of 10 beyond the 16 px margin.
        assert_eq!(width(true), 33.0);
        assert_eq!(width(false), 27.0);
    }

    #[test]
    fn default_south_legend() {
        let frame = frame(&Options::default(), &sample()).unwrap();
        assert_eq!(geometry(&frame), [51, 15, 400, 100, 481, 155]);
        let first = &frame.legend[0];
        assert_eq!((first.left, first.bottom, first.width), (16.0, 148.0, 49.0));
        assert_eq!(frame.legend[1].left, 81.0);
        assert_eq!(frame.legend[2].left, 139.0);
    }

    #[test]
    fn titles_labels_and_axes() {
        let with = |change: &dyn Fn(&mut Options)| {
            let mut options = Options::default();
            change(&mut options);
            geometry(&frame(&options, &sample()).unwrap())
        };
        assert_eq!(
            with(&|o| o.title = Some("One\\nTwo".into())),
            [51, 43, 400, 100, 481, 183]
        );
        assert_eq!(
            with(&|o| o.vertical_label = true),
            [67, 15, 400, 100, 497, 155]
        );
        assert_eq!(with(&|o| o.right_axis = true), [51, 15, 400, 100, 532, 155]);
        assert_eq!(
            with(&|o| o.right_axis_label = true),
            [51, 15, 400, 100, 482, 155]
        );
        assert_eq!(with(&|o| o.watermark = true), [51, 15, 400, 100, 481, 166]);
        assert_eq!(with(&|o| o.x_labels = false), [51, 15, 400, 100, 481, 138]);
        assert_eq!(
            with(&|o| o.show_legend = false),
            [51, 15, 400, 100, 481, 139]
        );
        assert_eq!(
            with(&|o| o.rrdtool_tag = false),
            [51, 15, 400, 100, 466, 155]
        );
        assert_eq!(with(&|o| o.only_graph = true), [0, 0, 400, 100, 400, 100]);
        assert_eq!(
            with(&|o| {
                o.show_legend = false;
                o.placement = Placement::East;
            }),
            [66, 15, 400, 100, 481, 139]
        );
        assert_eq!(
            with(&|o| {
                o.full_size = true;
                o.height = 200;
            }),
            [51, 15, 319, 145, 400, 200]
        );
    }

    #[test]
    fn legends_beside_and_above_the_graph() {
        let with = |placement| {
            let options = Options {
                placement,
                ..Options::default()
            };
            frame(&options, &sample()).unwrap()
        };
        let north = with(Placement::North);
        assert_eq!(geometry(&north), [51, 38, 400, 100, 481, 155]);
        assert_eq!(north.legend[0].bottom, 31.0);
        let east = with(Placement::East);
        assert_eq!(geometry(&east), [66, 15, 400, 100, 657, 132]);
        assert_eq!(east.legend[0].left, 497.0);
        let west = with(Placement::West);
        assert_eq!(geometry(&west), [227, 15, 400, 100, 657, 132]);
        assert_eq!(west.legend[0].left, 16.0);
    }

    #[test]
    fn title_line_counting() {
        for (title, lines) in [
            ("T", 1),
            ("a\\n", 2),
            ("\\na", 1),
            ("a\\n\\nb", 2),
            ("a<br>b", 2),
            ("a<br/>b<br/>c<br/>d", 3),
            ("a<BR>b", 1),
        ] {
            assert_eq!(title_lines(title), lines, "{title}");
        }
    }
}
