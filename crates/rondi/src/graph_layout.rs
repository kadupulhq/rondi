//! Image layout from RRDtool 1.11.0 `rrd_graph.c`: `graph_size_location`,
//! `leg_place`, `data_proc`, `si_unit`, `expand_range` and the legend
//! positions `graph_paint_timestring` reports through `graphv`.
//!
//! RRDtool measures text with Pango. Its default font (DejaVu Sans Mono, or
//! a metric-compatible monospace face such as Andale Mono on macOS) gives
//! every character one hinted advance at 100 dpi, which [`text_width`]
//! models. Characters the font lacks (CJK, emoji) fall back to another face
//! upstream and measure differently here.

use crate::graph::{Gf, GraphImage, TextAlign};
use crate::rrd_binary::rrd_nan;

pub const ALTAUTOSCALE: u32 = 0x02;
pub const ALTAUTOSCALE_MIN: u32 = 0x04;
pub const ALTAUTOSCALE_MAX: u32 = 0x08;
pub const NOLEGEND: u32 = 0x10;
pub const ONLY_GRAPH: u32 = 0x40;
pub const FORCE_RULES_LEGEND: u32 = 0x80;
pub const FULL_SIZE_MODE: u32 = 0x200;
pub const NO_RRDTOOL_TAG: u32 = 0x400;

pub const TEXT_PROP_TITLE: usize = 1;
pub const TEXT_PROP_AXIS: usize = 2;
pub const TEXT_PROP_UNIT: usize = 3;
pub const TEXT_PROP_LEGEND: usize = 4;
pub const TEXT_PROP_WATERMARK: usize = 5;

/// `text_prop[].size` defaults.
pub const TEXT_PROP_SIZES: [f64; 6] = [8.0, 9.0, 7.0, 8.0, 8.0, 5.5];

const MAX_IMAGE_TITLE_LINES: usize = 3;

/// DejaVu Sans Mono units per em, advance width, ascent and descent.
const FONT_UNITS_PER_EM: f64 = 2048.0;
const FONT_ADVANCE: f64 = 1233.0;
const FONT_ASCENT: f64 = 1901.0;
const FONT_DESCENT: f64 = 483.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegendPosition {
    North,
    West,
    South,
    East,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegendDirection {
    TopDown,
    BottomUp,
    BottomUp2,
}

/// The font size in pixels at the 100 dpi `rrd_graph_init` sets.
fn font_pixels(size: f64) -> f64 {
    size * 100.0 / 72.0
}

fn glyph_advance(size: f64) -> f64 {
    (font_pixels(size) * FONT_ADVANCE / FONT_UNITS_PER_EM).round()
}

/// `gfx_get_text_height`: the hinted ascent plus descent of the line.
pub fn text_height(size: f64) -> f64 {
    let pixels = font_pixels(size);
    (pixels * FONT_ASCENT / FONT_UNITS_PER_EM).ceil()
        + (pixels * FONT_DESCENT / FONT_UNITS_PER_EM).ceil()
}

/// `gfx_get_text_width` with the tab stops `gfx_prep_text` installed: an
/// explicit tab advances to the first stop past the pen, and stops past the
/// array repeat the distance between its last two entries, as Pango does.
pub fn text_width(tab_stops: &[f64], size: f64, text: &str) -> f64 {
    let advance = glyph_advance(size);
    let mut pen = 0.0;
    for character in text.chars() {
        if character != '\t' {
            pen += advance;
            continue;
        }
        pen = next_tab_stop(tab_stops, advance, pen);
    }
    pen
}

fn next_tab_stop(tab_stops: &[f64], advance: f64, pen: f64) -> f64 {
    // Without a tab array Pango uses stops every eight spaces and keeps at
    // least one space before the tabbed text.
    if tab_stops.is_empty() {
        let width = 8.0 * advance;
        let mut stop = width;
        while stop < pen + advance {
            stop += width;
        }
        return stop;
    }
    let count = tab_stops.len();
    let distance = if count > 1 {
        tab_stops[count - 1] - tab_stops[count - 2]
    } else {
        8.0 * advance
    };
    let mut index = 0_usize;
    loop {
        let stop = if index < count {
            tab_stops[index]
        } else {
            tab_stops[count - 1] + distance * (index - count + 1) as f64
        };
        if stop >= pen + 1.0 {
            return stop;
        }
        if index >= count && distance <= 0.0 {
            return pen;
        }
        index += 1;
    }
}

/// `graph_title_split(...).count`.
fn graph_title_lines(title: &str) -> usize {
    const DELIMS: [&str; 4] = ["\n", "\\n", "<br>", "<br/>"];
    let mut consumed = title;
    let mut count = 0;
    loop {
        let found = DELIMS
            .iter()
            .filter_map(|delim| consumed.find(delim).map(|pos| (pos, delim.len())))
            .min_by_key(|(pos, _)| *pos);
        match found {
            Some((pos, size)) => {
                if pos != 0 {
                    count += 1;
                }
                consumed = &consumed[pos + size..];
                if count >= MAX_IMAGE_TITLE_LINES {
                    return count;
                }
            }
            None => return count + 1,
        }
    }
}

/// `AlmostEqual2sComplement` on the single-precision values C passes.
fn almost_equal_2s_complement(a: f64, b: f64, max_ulps: i32) -> bool {
    let lexicographic = |value: f64| {
        let bits = (value as f32).to_bits() as i32;
        if bits < 0 {
            (0x8000_0000_i64 - i64::from(bits)) as i32
        } else {
            bits
        }
    };
    lexicographic(a)
        .wrapping_sub(lexicographic(b))
        .wrapping_abs()
        <= max_ulps
}

const SI_SYMBOL: [char; 17] = [
    'y', 'z', 'a', 'f', 'p', 'n', 'u', 'm', ' ', 'k', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y',
];
const SI_SYMBCENTER: f64 = 8.0;

const SENSIBLE_VALUES: [f64; 48] = [
    1000.0, 900.0, 800.0, 750.0, 700.0, 600.0, 500.0, 400.0, 300.0, 250.0, 200.0, 125.0, 100.0,
    90.0, 80.0, 75.0, 70.0, 60.0, 50.0, 40.0, 30.0, 25.0, 20.0, 10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0,
    3.5, 3.0, 2.5, 2.0, 1.8, 1.5, 1.2, 1.0, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0, -1.0,
];

impl GraphImage {
    /// `gfx_get_text_width`. The first call fixes the tab stops for every
    /// later one, as `gfx_prep_text` caches them per tab width.
    fn gfx_get_text_width(&mut self, start: f64, size: f64, text: &str) -> f64 {
        if self.last_tabwidth < 0.0 || self.last_tabwidth != self.tabwidth {
            self.last_tabwidth = self.tabwidth;
            let tab_count = text.len();
            let tab_shift = (start % self.tabwidth) as i64 as f64;
            let border = (self.text_prop[TEXT_PROP_LEGEND] * 2.0) as i32;
            // pango_tab_array_set_tab(tab_array, i, ...) for i in 1..=n
            // leaves index 0 at position 0.
            self.tab_stops = if tab_count == 0 {
                Vec::new()
            } else {
                let mut stops = vec![0.0; tab_count + 1];
                for (i, stop) in stops.iter_mut().enumerate().skip(1) {
                    *stop = self.tabwidth * i as f64 - tab_shift + f64::from(border);
                }
                stops
            };
        }
        text_width(&self.tab_stops, size, text)
    }

    /// The width of `text` in the legend font, measured as the legend
    /// drawing loop does.
    pub fn legend_text_width(&mut self, text: &str) -> f64 {
        self.gfx_get_text_width(0.0, self.text_prop[TEXT_PROP_LEGEND], text)
    }

    /// `leg_place`: positions every legend and sets the legend height, or
    /// with `calc_width` the legend width of a west/east legend.
    pub fn leg_place(&mut self, calc_width: bool) -> Result<(), String> {
        let legend_size = self.text_prop[TEXT_PROP_LEGEND];
        let interleg = (legend_size * 2.0) as i32;
        let border = (legend_size * 2.0) as i32;
        let mut fill = 0_i32;
        let mut leg_c = 0;
        let mut leg_x;
        let mut leg_y = 0_i32;
        let mut glue;
        let mut mark = 0_usize;
        let mut default_txtalign = TextAlign::Justified;
        let mut legendwidth = if calc_width {
            0.0
        } else {
            (self.legendwidth - 2 * i64::from(border)) as f64
        };
        if self.extra_flags & NOLEGEND != 0 || self.extra_flags & ONLY_GRAPH != 0 {
            return Ok(());
        }
        let count = self.gdes.len();
        let mut legspace = vec![0_i32; count];
        let mut i = 0_usize;
        while i < count {
            let saved_legend = calc_width.then(|| self.gdes[i].legend.clone());
            let fill_last = fill;
            if self.gdes[i].gf == Gf::TextAlign {
                default_txtalign = self.gdes[i].txtalign;
            }
            if self.extra_flags & FORCE_RULES_LEGEND == 0 {
                let element = &self.gdes[i];
                if element.gf == Gf::Hrule
                    && (element.yrule < self.minval || element.yrule > self.maxval)
                {
                    self.gdes[i].legend.clear();
                }
                let element = &self.gdes[i];
                if element.gf == Gf::Vrule
                    && (element.xrule < self.start || element.xrule > self.end)
                {
                    self.gdes[i].legend.clear();
                }
            }
            let legend = &mut self.gdes[i].legend;
            while let Some(tab) = legend.find("\\t") {
                legend.replace_range(tab..tab + 2, "\t");
            }
            let mut leg_cc = legend.len();
            let bytes = legend.as_bytes();
            let mut prt_fctn = if leg_cc >= 2 && bytes[leg_cc - 2] == b'\\' {
                let code = bytes[leg_cc - 1];
                leg_cc -= 2;
                legend.truncate(leg_cc);
                code
            } else {
                b'\0'
            };
            if !matches!(
                prt_fctn,
                b'l' | b'n' | b'r' | b'j' | b'c' | b'u' | b'.' | b's' | b'\0' | b'g'
            ) {
                return Err(format!(
                    "Unknown control code at the end of '{}\\{}'",
                    legend,
                    char::from(prt_fctn)
                ));
            }
            if prt_fctn == b'n' {
                prt_fctn = b'l';
            }
            if prt_fctn == b'.' {
                prt_fctn = b'\0';
            }
            while prt_fctn == b'g' && leg_cc > 0 && legend.as_bytes()[leg_cc - 1] == b' ' {
                leg_cc -= 1;
                legend.truncate(leg_cc);
            }
            if leg_cc != 0 {
                legspace[i] = if prt_fctn == b'g' { 0 } else { interleg };
                if fill > 0 {
                    fill += legspace[i];
                }
                let text = self.gdes[i].legend.clone();
                fill = (f64::from(fill)
                    + self.gfx_get_text_width(f64::from(fill + border), legend_size, &text))
                    as i32;
                leg_c += 1;
            } else {
                legspace[i] = 0;
            }
            if prt_fctn == b'g' {
                prt_fctn = b'\0';
            }
            if prt_fctn == b'\0' {
                if calc_width && f64::from(fill) > legendwidth {
                    legendwidth = f64::from(fill);
                }
                if i == count - 1 || f64::from(fill) > legendwidth {
                    prt_fctn = match default_txtalign {
                        TextAlign::Right => b'r',
                        TextAlign::Center => b'c',
                        TextAlign::Justified => b'j',
                        TextAlign::Left => b'l',
                    };
                }
                if f64::from(fill) > legendwidth && leg_c > 1 {
                    i -= 1;
                    fill = fill_last;
                    leg_c -= 1;
                }
                if leg_c == 1 && prt_fctn == b'j' {
                    prt_fctn = b'l';
                }
            }
            if prt_fctn != b'\0' {
                leg_x = f64::from(border);
                glue = if leg_c >= 2 && prt_fctn == b'j' {
                    (legendwidth - f64::from(fill)) / f64::from(leg_c - 1)
                } else {
                    0.0
                };
                if prt_fctn == b'c' {
                    leg_x = f64::from(border) + (legendwidth - f64::from(fill)) / 2.0;
                }
                if prt_fctn == b'r' {
                    leg_x = legendwidth - f64::from(fill) + f64::from(border);
                }
                let mut ii = mark;
                while ii <= i {
                    if !self.gdes[ii].legend.is_empty() {
                        self.gdes[ii].leg_x = leg_x;
                        self.gdes[ii].leg_y = f64::from(leg_y + border);
                        let text = self.gdes[ii].legend.clone();
                        leg_x += self.gfx_get_text_width(leg_x, legend_size, &text)
                            + f64::from(legspace[ii])
                            + glue;
                    }
                    ii += 1;
                }
                if leg_x > f64::from(border) || prt_fctn == b's' {
                    leg_y = (f64::from(leg_y) + legend_size * 1.8) as i32;
                }
                if prt_fctn == b's' {
                    leg_y = (f64::from(leg_y) - legend_size) as i32;
                }
                if prt_fctn == b'u' {
                    leg_y = (f64::from(leg_y) - legend_size * 1.8) as i32;
                }
                if calc_width && f64::from(fill) > legendwidth {
                    legendwidth = f64::from(fill);
                }
                fill = 0;
                leg_c = 0;
                mark = ii;
            }
            if let Some(saved_legend) = saved_legend {
                self.gdes[i].legend = saved_legend;
            }
            i += 1;
        }
        if calc_width {
            self.legendwidth = (legendwidth + f64::from(2 * border)) as i64;
        } else {
            self.legendheight = (f64::from(leg_y) + f64::from(border) * 0.6) as i64;
        }
        Ok(())
    }

    /// `graph_size_location`: the image size, the graph origin and the
    /// legend origins.
    pub fn graph_size_location(&mut self, elements: bool) -> Result<(), String> {
        let mut xvertical = 0_i32;
        let mut xylabel = 0_i32;
        let mut xmain = 0_i32;
        let mut ymain = 0_i32;
        let mut yxlabel = 0_i32;
        let xspacing = 15_i32;
        let yspacing = 15_i32;
        let mut ywatermark = 4_i32;
        let nolegend = self.extra_flags & NOLEGEND != 0;
        let side_legend = matches!(
            self.legendposition,
            LegendPosition::West | LegendPosition::East
        );
        if self.extra_flags & ONLY_GRAPH != 0 {
            self.xorigin = 0;
            self.ximg = self.xsize;
            self.yimg = self.ysize;
            self.yorigin = self.ysize;
            return Ok(());
        }
        let watermark = self
            .watermark
            .as_deref()
            .is_some_and(|text| !text.is_empty());
        if watermark {
            ywatermark = (self.text_prop[TEXT_PROP_WATERMARK] * 2.0) as i32;
        }
        if self.ylegend.as_deref().is_some_and(|text| !text.is_empty()) {
            xvertical = (self.text_prop[TEXT_PROP_UNIT] * 2.0) as i32;
        }
        let xvertical2 = if self
            .second_axis_legend
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            (self.text_prop[TEXT_PROP_UNIT] * 2.0) as i32
        } else {
            xspacing
        };
        let ytitle = match self.title.as_deref().filter(|text| !text.is_empty()) {
            Some(title) => {
                let lines = graph_title_lines(title);
                (self.text_prop[TEXT_PROP_TITLE] * (lines + 1) as f64 * 1.6) as i32
            }
            None => yspacing,
        };
        if elements {
            if self.draw_x_grid {
                yxlabel = (self.text_prop[TEXT_PROP_AXIS] * 2.5) as i32;
            }
            if self.draw_y_grid || self.forceleftspace {
                xylabel = (self.gfx_get_text_width(0.0, self.text_prop[TEXT_PROP_AXIS], "0")
                    * f64::from(self.unitslength)) as i32;
            }
        }
        xylabel += xspacing;
        self.legendheight = 0;
        self.legendwidth = 0;
        if !nolegend && side_legend {
            self.leg_place(true)?;
        }
        if self.extra_flags & FULL_SIZE_MODE != 0 {
            self.ximg = self.xsize;
            self.yimg = self.ysize;
            xmain = self.ximg as i32;
            ymain = self.yimg as i32;
            xmain -= xylabel;
            if side_legend && !nolegend {
                xmain -= self.legendwidth as i32;
            }
            if self.second_axis_scale != 0.0 {
                xmain -= xylabel;
            }
            if self.extra_flags & NO_RRDTOOL_TAG == 0 {
                xmain -= xspacing;
            }
            xmain -= xvertical + xvertical2;
            if xmain < 1 {
                xmain = 1;
            }
            self.xsize = i64::from(xmain);
            if !nolegend && !side_legend {
                self.legendwidth = self.ximg;
                self.leg_place(false)?;
            }
            if !side_legend && !nolegend {
                ymain -= yxlabel + self.legendheight as i32;
            } else {
                ymain -= yxlabel;
            }
            ymain -= ytitle;
            if nolegend {
                ymain = (f64::from(ymain) - 0.5 * f64::from(yspacing)) as i32;
            }
            if watermark {
                ymain -= ywatermark;
            }
            if ymain < 1 {
                ymain = 1;
            }
            self.ysize = i64::from(ymain);
        } else {
            if elements {
                xmain = self.xsize as i32;
                ymain = self.ysize as i32;
            }
            self.ximg = i64::from(xmain + xylabel);
            if self.extra_flags & NO_RRDTOOL_TAG == 0 {
                self.ximg += i64::from(xspacing);
            }
            if side_legend && !nolegend {
                self.ximg += self.legendwidth;
            }
            if self.second_axis_scale != 0.0 {
                self.ximg += i64::from(xylabel);
            }
            self.ximg += i64::from(xvertical + xvertical2);
            if !nolegend && !side_legend {
                self.legendwidth = self.ximg;
                self.leg_place(false)?;
            }
            self.yimg = i64::from(ymain + yxlabel);
            if !side_legend && !nolegend {
                self.yimg += self.legendheight;
            }
            if ytitle != 0 {
                self.yimg += i64::from(ytitle);
            } else {
                self.yimg = (self.yimg as f64 + 1.5 * f64::from(yspacing)) as i64;
            }
            if nolegend {
                self.yimg = (self.yimg as f64 + 0.5 * f64::from(yspacing)) as i64;
            }
            if watermark {
                self.yimg += i64::from(ywatermark);
            }
        }
        if !nolegend && side_legend {
            self.leg_place(false)?;
        }
        let (xvertical, xylabel, xmain, ymain, yxlabel, ytitle) = (
            i64::from(xvertical),
            i64::from(xylabel),
            i64::from(xmain),
            i64::from(ymain),
            i64::from(yxlabel),
            i64::from(ytitle),
        );
        let second_axis = if self.second_axis_scale != 0.0 {
            xylabel
        } else {
            0
        };
        match self.legendposition {
            LegendPosition::North => {
                self.x_origin_title = self.ximg / 2;
                self.y_origin_title = 0;
                self.x_origin_legend = 0;
                self.y_origin_legend = ytitle;
                self.x_origin_legend_y = 0;
                self.y_origin_legend_y = ytitle + self.legendheight + ymain / 2 + yxlabel;
                self.xorigin = xvertical + xylabel;
                self.yorigin = ytitle + self.legendheight + ymain;
                self.x_origin_legend_y2 = xvertical + xylabel + xmain + second_axis;
                self.y_origin_legend_y2 = ytitle + self.legendheight + ymain / 2 + yxlabel;
            }
            LegendPosition::West => {
                self.x_origin_title = self.legendwidth + self.xsize / 2;
                self.y_origin_title = 0;
                self.x_origin_legend = 0;
                self.y_origin_legend = ytitle;
                self.x_origin_legend_y = self.legendwidth;
                self.y_origin_legend_y = ytitle + ymain / 2;
                self.xorigin = self.legendwidth + xvertical + xylabel;
                self.yorigin = ytitle + ymain;
                self.x_origin_legend_y2 =
                    self.legendwidth + xvertical + xylabel + xmain + second_axis;
                self.y_origin_legend_y2 = ytitle + ymain / 2;
            }
            LegendPosition::South => {
                self.x_origin_title = self.ximg / 2;
                self.y_origin_title = 0;
                self.x_origin_legend = 0;
                self.y_origin_legend = ytitle + ymain + yxlabel;
                self.x_origin_legend_y = 0;
                self.y_origin_legend_y = ytitle + ymain / 2;
                self.xorigin = xvertical + xylabel;
                self.yorigin = ytitle + ymain;
                self.x_origin_legend_y2 = xvertical + xylabel + xmain + second_axis;
                self.y_origin_legend_y2 = ytitle + ymain / 2;
            }
            LegendPosition::East => {
                self.x_origin_title = self.xsize / 2;
                self.y_origin_title = 0;
                self.x_origin_legend =
                    xvertical + xylabel + xmain + i64::from(xvertical2) + second_axis;
                self.y_origin_legend = ytitle;
                self.x_origin_legend_y = 0;
                self.y_origin_legend_y = ytitle + ymain / 2;
                self.xorigin = xvertical + xylabel;
                self.yorigin = ytitle + ymain;
                self.x_origin_legend_y2 = xvertical + xylabel + xmain + second_axis;
                self.y_origin_legend_y2 = ytitle + ymain / 2;
                if self.extra_flags & NO_RRDTOOL_TAG == 0 {
                    let xspacing = i64::from(xspacing);
                    self.x_origin_title += xspacing;
                    self.x_origin_legend += xspacing;
                    self.x_origin_legend_y += xspacing;
                    self.xorigin += xspacing;
                    self.x_origin_legend_y2 += xspacing;
                }
            }
        }
        Ok(())
    }

    /// `data_proc`: one value per pixel column for every LINE, AREA and
    /// TICK, stacked, and the resulting value range.
    pub fn data_proc(&mut self) -> Result<(), String> {
        let pixstep = (self.end - self.start) as f64 / self.xsize as f64;
        let mut minval = rrd_nan();
        let mut maxval = rrd_nan();
        let xsize = usize::try_from(self.xsize).unwrap_or(0);
        for element in &mut self.gdes {
            if matches!(element.gf, Gf::Line | Gf::Area | Gf::Tick) {
                element.p_data = vec![rrd_nan(); xsize + 1];
            }
        }
        for i in 0..xsize {
            let gr_time = (self.start as f64 + pixstep * i as f64) as u64;
            let mut paintval = 0.0;
            for ii in 0..self.gdes.len() {
                match self.gdes[ii].gf {
                    Gf::Line | Gf::Area | Gf::Tick => {
                        if !self.gdes[ii].stack {
                            paintval = 0.0;
                        }
                        let mut value = self.gdes[ii].yrule;
                        if value.is_nan() || self.gdes[ii].gf == Gf::Tick {
                            value = match self.gdes[ii].vidx {
                                Some(vidx) if self.gdes[vidx].gf == Gf::Vdef => {
                                    self.gdes[vidx].vf.val
                                }
                                Some(vidx) => {
                                    let source = &self.gdes[vidx];
                                    if gr_time as i64 >= source.start
                                        && (gr_time as i64) < source.end
                                        && source.step != 0
                                    {
                                        let index = ((gr_time.wrapping_sub(source.start as u64))
                                            as f64
                                            / source.step as f64)
                                            .floor();
                                        source
                                            .data
                                            .get(index as usize)
                                            .copied()
                                            .unwrap_or_else(rrd_nan)
                                    } else {
                                        rrd_nan()
                                    }
                                }
                                None => rrd_nan(),
                            };
                        }
                        let element = &mut self.gdes[ii];
                        if !value.is_nan() {
                            paintval += value;
                            element.p_data[i] = paintval;
                            if paintval.is_finite() && element.gf != Gf::Tick && !element.skipscale
                            {
                                if (minval.is_nan() || paintval < minval)
                                    && !(self.logarithmic && paintval <= 0.0)
                                {
                                    minval = paintval;
                                }
                                if maxval.is_nan() || paintval > maxval {
                                    maxval = paintval;
                                }
                            }
                        } else {
                            element.p_data[i] = rrd_nan();
                        }
                    }
                    Gf::Stack => {
                        return Err(String::from(
                            "STACK should already be turned into LINE or AREA here",
                        ));
                    }
                    _ => {}
                }
            }
        }
        if self.logarithmic {
            if minval.is_nan() || maxval.is_nan() || maxval <= 0.0 {
                minval = 0.0;
                maxval = 5.1;
            }
            if minval <= 0.0 {
                minval = maxval / 10e8;
            }
        } else if minval.is_nan() || maxval.is_nan() {
            minval = 0.0;
            maxval = 1.0;
        }
        if self.minval.is_nan() || (!self.rigid && self.minval > minval) {
            self.minval = if self.logarithmic {
                minval / 2.0
            } else {
                minval
            };
        }
        if self.maxval.is_nan() || (!self.rigid && self.maxval < maxval) {
            self.maxval = if self.logarithmic {
                maxval * 2.0
            } else {
                maxval
            };
        }
        if !self.minval.is_nan() && self.rigid && self.allow_shrink && self.minval < minval {
            self.minval = if self.logarithmic {
                minval / 2.0
            } else {
                minval
            };
        }
        if !self.maxval.is_nan() && self.rigid && self.allow_shrink && self.maxval > maxval {
            self.maxval = if self.logarithmic {
                maxval * 2.0
            } else {
                maxval
            };
        }
        if self.minval > self.maxval {
            if self.minval > 0.0 {
                self.minval = 0.99 * self.maxval;
            } else {
                self.minval = 1.01 * self.maxval;
            }
        }
        if almost_equal_2s_complement(self.minval, self.maxval, 4) {
            if self.maxval > 0.0 {
                self.maxval *= 1.01;
            } else {
                self.maxval *= 0.99;
            }
            if almost_equal_2s_complement(self.maxval, 0.0, 4) {
                self.maxval = 1.0;
            }
        }
        Ok(())
    }

    /// `si_unit`.
    pub fn si_unit(&mut self) {
        let base = self.base as f64;
        let digits = (self.minval.abs().max(self.maxval.abs()).ln() / base.ln()).floor();
        let viewdigits = if self.unitsexponent != 9999 {
            f64::from(self.unitsexponent / 3).floor()
        } else {
            digits
        };
        self.magfact = base.powf(digits);
        self.viewfactor = (self.magfact / base.powf(viewdigits)) as f32;
        self.symbol = if viewdigits + SI_SYMBCENTER < SI_SYMBOL.len() as f64
            && viewdigits + SI_SYMBCENTER >= 0.0
        {
            SI_SYMBOL[(viewdigits as i32 + SI_SYMBCENTER as i32) as usize]
        } else {
            '?'
        };
    }

    /// `expand_range`.
    pub fn expand_range(&mut self) {
        if !self.ygridstep.is_nan() {
            let unit = f64::from(self.ylabfact) * self.ygridstep;
            self.minval = unit * (self.minval / unit).floor();
            self.maxval = unit * (self.maxval / unit).ceil();
            return;
        }
        if self.extra_flags & ALTAUTOSCALE != 0 {
            let delt = self.maxval - self.minval;
            let mut adj = delt * 0.1;
            let fact = 2.0
                * 10.0_f64.powf(
                    (self.minval.abs().max(self.maxval.abs()) / self.magfact)
                        .log10()
                        .floor()
                        - 2.0,
                );
            if delt < fact {
                adj = (fact - delt) * 0.55;
            }
            self.minval -= adj;
            self.maxval += adj;
        } else if self.extra_flags & ALTAUTOSCALE_MIN != 0 {
            let adj = (self.maxval - self.minval) * 0.1;
            self.minval -= adj;
        } else if self.extra_flags & ALTAUTOSCALE_MAX != 0 {
            let adj = (self.maxval - self.minval) * 0.1;
            self.maxval += adj;
        } else {
            let scaled_min = self.minval / self.magfact;
            let scaled_max = self.maxval / self.magfact;
            let mut i = 1;
            while SENSIBLE_VALUES[i] > 0.0 {
                let (previous, current) = (SENSIBLE_VALUES[i - 1], SENSIBLE_VALUES[i]);
                if previous >= scaled_min && current <= scaled_min {
                    self.minval = current * self.magfact;
                }
                if -previous <= scaled_min && -current >= scaled_min {
                    self.minval = -previous * self.magfact;
                }
                if previous >= scaled_max && current <= scaled_max {
                    self.maxval = previous * self.magfact;
                }
                if -previous <= scaled_max && -current >= scaled_max {
                    self.maxval = -current * self.magfact;
                }
                i += 1;
            }
        }
    }

    /// The legend drawing loop of `graph_paint_timestring`: the index and
    /// bottom-left pen position of every element with a legend.
    pub fn legend_origins(&self) -> Vec<(usize, f64, f64)> {
        if self.extra_flags & NOLEGEND != 0 || self.extra_flags & ONLY_GRAPH != 0 {
            return Vec::new();
        }
        let count = self.gdes.len();
        let mut first_noncomment = count;
        let mut last_noncomment = 0;
        let mut min = 0.0;
        let mut max = 0.0;
        let mut gotcha = false;
        for (i, element) in self.gdes.iter().enumerate() {
            if element.legend.is_empty() {
                continue;
            }
            if !gotcha {
                min = element.leg_y;
                gotcha = true;
            }
            if element.gf != Gf::Comment {
                if self.legenddirection == LegendDirection::BottomUp2 {
                    min = element.leg_y;
                }
                first_noncomment = i;
                break;
            }
        }
        gotcha = false;
        for (i, element) in self.gdes.iter().enumerate().rev() {
            if element.legend.is_empty() {
                continue;
            }
            if !gotcha {
                max = element.leg_y;
                gotcha = true;
            }
            if element.gf != Gf::Comment {
                if self.legenddirection == LegendDirection::BottomUp2 {
                    max = element.leg_y;
                }
                last_noncomment = i;
                break;
            }
        }
        self.gdes
            .iter()
            .enumerate()
            .filter(|(_, element)| !element.legend.is_empty())
            .map(|(i, element)| {
                let x0 = self.x_origin_legend as f64 + element.leg_x;
                let reverse = match self.legenddirection {
                    LegendDirection::TopDown => false,
                    LegendDirection::BottomUp => true,
                    LegendDirection::BottomUp2 => i >= first_noncomment && i <= last_noncomment,
                };
                let y0 = if reverse {
                    self.y_origin_legend as f64 + max + min - element.leg_y
                } else {
                    self.y_origin_legend as f64 + element.leg_y
                };
                (i, x0, y0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_metrics_match_rrdtool() {
        let stops = [0.0, 56.0];
        assert_eq!(text_width(&stops, 7.0, "0"), 6.0);
        assert_eq!(text_width(&stops, 8.0, "  foo"), 35.0);
        assert_eq!(text_width(&stops, 8.0, "  a\tb\tc"), 119.0);
        assert_eq!(text_width(&stops, 8.0, "  abcdef\tb"), 119.0);
        assert_eq!(text_height(8.0), 14.0);
    }

    /// `data_proc` and `expand_range` over constant LINE/AREA values on a
    /// 400 pixel wide graph.
    fn value_range(elements: &[&str], setup: impl FnOnce(&mut GraphImage)) -> (f64, f64) {
        let mut im = GraphImage::new(1_000_000_000, 1_000_000_400, 1);
        let script: Vec<String> = elements.iter().map(|item| item.to_string()).collect();
        im.graph_script(&script, &|_, _, start, end| Ok((start, end)))
            .unwrap();
        setup(&mut im);
        im.data_proc().unwrap();
        im.si_unit();
        if !im.rigid || im.allow_shrink {
            im.expand_range();
        }
        (im.minval, im.maxval)
    }

    const TWO_TO_EIGHT: [&str; 2] = ["LINE1:2#ff0000", "LINE1:8#00ff00"];

    #[test]
    fn automatic_range_snaps_to_sensible_values() {
        assert_eq!(value_range(&TWO_TO_EIGHT, |_| {}), (1.8, 8.0));
        assert_eq!(
            value_range(&["LINE1:-7.5#ff0000", "LINE1:15.5#ff0000"], |_| {}),
            (-8.0, 20.0)
        );
    }

    #[test]
    fn skipscale_ticks_and_rules_stay_out_of_the_range() {
        let elements = [
            "LINE1:2#ff0000",
            "LINE1:4#ff0000",
            "LINE1:1000#ff0000::skipscale",
            "HRULE:-1000#ff0000",
        ];
        assert_eq!(value_range(&elements, |_| {}), (1.8, 4.0));
    }

    #[test]
    fn stacked_values_extend_the_range() {
        let elements = ["AREA:3#ff0000", "AREA:2#00ff00::STACK"];
        assert_eq!(value_range(&elements, |_| {}), (2.5, 5.0));
    }

    #[test]
    fn rigid_limits_hold_until_allow_shrink_is_selected() {
        let limits = |im: &mut GraphImage| {
            im.minval = 0.0;
            im.maxval = 10.0;
            im.rigid = true;
        };
        assert_eq!(value_range(&TWO_TO_EIGHT, limits), (0.0, 10.0));
        let shrunk = |im: &mut GraphImage| {
            limits(im);
            im.allow_shrink = true;
        };
        assert_eq!(value_range(&TWO_TO_EIGHT, shrunk), (1.8, 8.0));
    }

    #[test]
    fn flexible_limits_expand_to_include_outlying_data() {
        let limits = |im: &mut GraphImage| {
            im.minval = 3.0;
            im.maxval = 7.0;
        };
        assert_eq!(value_range(&TWO_TO_EIGHT, limits), (1.8, 8.0));
    }

    #[test]
    fn alternate_autoscale_modes_expand_the_requested_side() {
        let flag = |flag| move |im: &mut GraphImage| im.extra_flags |= flag;
        let (min, max) = value_range(&TWO_TO_EIGHT, flag(ALTAUTOSCALE));
        assert!((min - 1.4).abs() < 1e-12 && (max - 8.6).abs() < 1e-12);
        let (min, max) = value_range(&TWO_TO_EIGHT, flag(ALTAUTOSCALE_MIN));
        assert!((min - 1.4).abs() < 1e-12 && max == 8.0);
        let (min, max) = value_range(&TWO_TO_EIGHT, flag(ALTAUTOSCALE_MAX));
        assert!(min == 2.0 && (max - 8.6).abs() < 1e-12);
    }

    #[test]
    fn inverted_and_collapsed_limits_are_moved_apart() {
        let inverted = |im: &mut GraphImage| {
            im.minval = 9.0;
            im.maxval = 4.0;
            im.rigid = true;
        };
        assert_eq!(value_range(&TWO_TO_EIGHT, inverted), (0.99 * 4.0, 4.0));
        let collapsed = |im: &mut GraphImage| {
            im.minval = 4.0;
            im.maxval = 4.0;
            im.rigid = true;
        };
        assert_eq!(value_range(&TWO_TO_EIGHT, collapsed), (4.0, 4.0 * 1.01));
    }

    #[test]
    fn title_lines_follow_graph_title_split() {
        assert_eq!(graph_title_lines("a"), 1);
        assert_eq!(graph_title_lines("a\\nb"), 2);
        assert_eq!(graph_title_lines("\\na"), 1);
        assert_eq!(graph_title_lines("a\n"), 2);
        assert_eq!(graph_title_lines("a<br>b<br/>c\nd"), 3);
    }
}
