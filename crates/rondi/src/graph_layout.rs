//! Names the legacy `graph` module still stores in its image description.
//!
//! The layout itself lives in [`crate::layout`]; these types only keep the
//! legacy element parser compiling until it is replaced, and nothing reads
//! the values.

/// Legend position as the legacy image description stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegendPosition {
    North,
    West,
    South,
    East,
}

/// Legend direction as the legacy image description stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegendDirection {
    TopDown,
    BottomUp,
    BottomUp2,
}

/// Default font sizes per text kind (default, title, axis, unit, legend,
/// watermark); see [`crate::layout::Sizes`].
pub const TEXT_PROP_SIZES: [f64; 6] = [8.0, 9.0, 7.0, 8.0, 8.0, 5.5];
