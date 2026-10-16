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
    let sizes = options.sizes;
    let metrics = Metrics {
        size: sizes.legend,
        // Compatibility: a tab advances by the tab width in pixels plus the
        // legend margin, or by the tab width alone when the y-axis labels
        // are hidden and no units length is given; either way a stop lies
        // beyond the margin.
        // Provenance: black-box compatibility test (legend-009, -010 and
        // the tab probes recorded under GR-64; -y none with and without -L
        // at legend sizes 5, 8, 10 and 20 and -T 10, 13 and 40).
        tab: if options.tab_margin {
            (options.tab_width + 2.0 * sizes.legend).floor()
        } else {
            options.tab_width
        },
        markup: options.markup,
    };
    let border = metrics.gap();
    let top = match options.title.as_deref().filter(|title| !title.is_empty()) {
        Some(title) => floor(sizes.title * 1.6 * (title_lines(title) + 1) as f64),
        None => MARGIN,
    };
    let units = options.units_length * text::advance(sizes.axis) as i64;
    let left = MARGIN
        + if options.vertical_label {
            floor(2.0 * sizes.unit)
        } else {
            0
        }
        + units;
    let x_labels = if options.x_labels {
        floor(2.5 * sizes.axis)
    } else {
        0
    };
    let right_axis = if options.right_axis {
        MARGIN + units
    } else {
        0
    };
    let right_edge = if options.right_axis_label {
        floor(2.0 * sizes.unit)
    } else {
        MARGIN
    };
    let tag = if options.rrdtool_tag { TAG } else { 0 };
    let watermark = if options.watermark {
        floor(2.0 * sizes.watermark)
    } else {
        0
    };
    let placement = if options.show_legend {
        options.placement
    } else {
        Placement::South
    };
    let side = matches!(placement, Placement::East | Placement::West) && options.show_legend;

    // A legend beside the graph is measured first, since its width moves
    // the graph; above or below the graph it needs the image width.
    let beside = |first_bottom: i64, x: f64| {
        legend::place(entries, &metrics, None, x, first_bottom, options.order)
    };
    let mut frame = Frame::default();
    if side {
        let first_bottom = floor(top as f64 + 2.0 * sizes.legend);
        let probe = beside(first_bottom, 0.0)?;
        let legend_width = probe.width;
        let fixed_height = top + x_labels + watermark;
        let (graph_width, graph_height) = if options.full_size {
            let used =
                left + tag + right_axis + right_edge + legend_width as i64 + 2 * border as i64;
            (
                (options.width - used).max(1),
                (options.height - fixed_height).max(1),
            )
        } else {
            (options.width, options.height)
        };
        let (graph_left, legend_left) = if placement == Placement::East {
            let graph_left = left + tag;
            let legend_left = (graph_left + graph_width + right_axis + right_edge) as f64 + border;
            (graph_left, legend_left)
        } else {
            (left + legend_width as i64 + 2 * border as i64, border)
        };
        let laid = beside(first_bottom, legend_left)?;
        frame.graph_left = graph_left;
        frame.graph_top = top;
        frame.graph_width = graph_width;
        frame.graph_height = graph_height;
        frame.image_width = if options.full_size {
            options.width
        } else if placement == Placement::East {
            (legend_left + legend_width + border) as i64
        } else {
            graph_left + graph_width + right_axis + right_edge + tag
        };
        frame.image_height = if options.full_size {
            options.height
        } else {
            top + graph_height + x_labels + watermark
        };
        frame.legend = laid.placed;
        return Ok(frame);
    }

    let image_width = if options.full_size {
        options.width
    } else {
        left + options.width + right_axis + right_edge + tag
    };
    let graph_width = if options.full_size {
        (options.width - (left + right_axis + right_edge + tag)).max(1)
    } else {
        options.width
    };
    let area = Some((border, image_width as f64 - border));
    let block = |laid: &Laid| laid.height + metrics.trailer();
    match placement {
        Placement::North if options.show_legend => {
            let laid = legend::place(
                entries,
                &metrics,
                area,
                border,
                floor(top as f64 + 2.0 * sizes.legend),
                options.order,
            )?;
            let graph_top = top + block(&laid);
            let graph_height = if options.full_size {
                (options.height - graph_top - x_labels - watermark).max(1)
            } else {
                options.height
            };
            frame.graph_top = graph_top;
            frame.graph_height = graph_height;
            frame.image_height = if options.full_size {
                options.height
            } else {
                graph_top + graph_height + x_labels + watermark
            };
            frame.legend = laid.placed;
        }
        _ => {
            // The lines' positions depend on the graph height, which in
            // full-size mode depends on the legend's height: measure first.
            let measure = |graph_height: i64| {
                legend::place(
                    entries,
                    &metrics,
                    area,
                    border,
                    floor((top + graph_height + x_labels) as f64 + 2.0 * sizes.legend),
                    options.order,
                )
            };
            let legend_block = |laid: &Laid| {
                if options.show_legend {
                    block(laid)
                } else if options.full_size {
                    // Compatibility: full-size mode leaves one more pixel.
                    // Provenance: black-box compatibility test
                    // (graph_data_differential -D -g and the probes
                    // recorded under GR-62).
                    NO_LEGEND_PAD + 1
                } else {
                    NO_LEGEND_PAD
                }
            };
            let mut graph_height = options.height;
            if options.full_size {
                let probe = measure(0)?;
                graph_height =
                    (options.height - top - x_labels - legend_block(&probe) - watermark).max(1);
            }
            let laid = measure(graph_height)?;
            frame.graph_top = top;
            frame.graph_height = graph_height;
            frame.image_height = if options.full_size {
                options.height
            } else {
                top + graph_height + x_labels + legend_block(&laid) + watermark
            };
            if options.show_legend {
                frame.legend = laid.placed;
            }
        }
    }
    // Compatibility: without a legend, an east placement still moves the
    // graph right by the tag's width, as it does with a legend.
    // Provenance: black-box compatibility test (layout fuzzing against the
    // oracle; -g with --legend-position east, with and without the tag).
    frame.graph_left = if options.placement == Placement::East {
        left + tag
    } else {
        left
    };
    frame.graph_width = graph_width;
    frame.image_width = image_width;
    Ok(frame)
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
