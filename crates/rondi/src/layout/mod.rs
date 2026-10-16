//! Graph image layout: text metrics, the image and graph rectangles, the
//! legend, and the y-axis value range.
//!
//! Written from docs/compatibility/graph.md (GR-60 to GR-73) and black-box
//! runs of RRDtool 1.11.0 with `graphv`. The module knows nothing about how
//! the graph elements were parsed or computed: callers hand it legend
//! entries and per-pixel values.

mod frame;
mod legend;
mod range;
mod text;

pub use frame::{Frame, Options, Placement, Sizes, frame};
pub use legend::{Align, Entry, Order, Placed};
pub use range::{RangeOptions, Scale, ValueRange, value_range};
pub use text::{advance, line_height, markup_text, width as text_width};
