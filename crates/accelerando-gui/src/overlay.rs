//! Strategy / indicator overlays (`Footprint::plots` and `BacktestResult::series`) resolved to
//! chart coordinates once at startup.

use std::collections::BTreeMap;

use accelerando_core::result::Series;
use accelerando_core::{Footprint, Plot};
use eframe::egui::Color32;

pub(crate) struct OverlayLine {
    pub color: Color32,
    /// `(x, value)`, ascending in x.
    pub points: Vec<(f64, f64)>,
}

pub(crate) struct OverlayMarker {
    pub x: f64,
    pub price: f64,
    pub shape: String,
    pub color: Color32,
    pub text: String,
}

pub(crate) struct OverlaySegment {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub color: Color32,
    pub dashed: bool,
    pub text: String,
}

pub(crate) struct OverlayBox {
    pub x0: f64,
    pub x1: f64,
    pub low: f64,
    pub high: f64,
    pub color: Color32,
    pub text: String,
}

#[derive(Default)]
pub(crate) struct Overlays {
    pub lines: Vec<OverlayLine>,
    /// Sorted by x.
    pub markers: Vec<OverlayMarker>,
    pub segments: Vec<OverlaySegment>,
    pub boxes: Vec<OverlayBox>,
}

const PALETTE: [Color32; 6] = [
    Color32::from_rgb(0xf2, 0xc1, 0x4e),
    Color32::from_rgb(0x7f, 0xb8, 0xff),
    Color32::from_rgb(0xc7, 0x92, 0xea),
    Color32::from_rgb(0x8b, 0xd4, 0x50),
    Color32::from_rgb(0xff, 0x9e, 0x64),
    Color32::from_rgb(0x5c, 0xe1, 0xe6),
];

impl Overlays {
    /// `footprints` / `series` are the bars the strategy ran on; `bar_x(i)` gives the display x
    /// of bar `i`'s first and last trade (the chart may show different bars).
    pub fn build(
        footprints: &[Footprint],
        series: &[Series],
        bar_x: &dyn Fn(usize) -> (f64, f64),
    ) -> Self {
        let fp_x: Vec<(f64, f64)> = (0..footprints.len()).map(bar_x).collect();
        let mid = |i: usize| (fp_x[i].0 + fp_x[i].1) * 0.5;
        let back = |i: usize, bars: usize| i.saturating_sub(bars);
        let mut out = Self::default();
        let mut lines: BTreeMap<&str, OverlayLine> = BTreeMap::new();
        // The same box id is re-emitted while a box grows; only its last emission is drawn.
        let mut boxes: BTreeMap<&str, OverlayBox> = BTreeMap::new();

        for (i, fp) in footprints.iter().enumerate() {
            for plot in &fp.plots {
                match plot {
                    Plot::Line { id, value, color } => {
                        if value.is_finite() {
                            lines
                                .entry(id.as_str())
                                .or_insert_with(|| OverlayLine {
                                    color: parse_color(color).unwrap_or(PALETTE[0]),
                                    points: Vec::new(),
                                })
                                .points
                                .push((mid(i), *value));
                        }
                    }
                    Plot::LineSegment {
                        from_bars_back,
                        from_price,
                        to_bars_back,
                        to_price,
                        color,
                        dashed,
                        text,
                        ..
                    } => out.segments.push(OverlaySegment {
                        from: (mid(back(i, *from_bars_back)), *from_price),
                        to: (mid(back(i, *to_bars_back)), *to_price),
                        color: parse_color(color).unwrap_or(PALETTE[1]),
                        dashed: *dashed,
                        text: text.clone(),
                    }),
                    Plot::Marker { price, shape, color, text, .. } => {
                        out.markers.push(OverlayMarker {
                            x: mid(i),
                            price: *price,
                            shape: shape.clone(),
                            color: parse_color(color).unwrap_or(PALETTE[0]),
                            text: text.clone(),
                        })
                    }
                    Plot::MarkerAt { bars_back, price, shape, color, text, .. } => {
                        out.markers.push(OverlayMarker {
                            x: mid(back(i, *bars_back)),
                            price: *price,
                            shape: shape.clone(),
                            color: parse_color(color).unwrap_or(PALETTE[0]),
                            text: text.clone(),
                        })
                    }
                    Plot::PriceBox { id, low, high, span, color, text, .. } => {
                        boxes.insert(
                            id.as_str(),
                            OverlayBox {
                                x0: fp_x[back(i, *span)].0,
                                x1: fp_x[i].1,
                                low: *low,
                                high: *high,
                                color: parse_color(color).unwrap_or(PALETTE[2]),
                                text: text.clone(),
                            },
                        );
                    }
                    Plot::PriceBoxAt { bars_back, id, low, high, span, color, text, .. } => {
                        let end = back(i, *bars_back);
                        boxes.insert(
                            id.as_str(),
                            OverlayBox {
                                x0: fp_x[back(end, *span)].0,
                                x1: fp_x[end].1,
                                low: *low,
                                high: *high,
                                color: parse_color(color).unwrap_or(PALETTE[2]),
                                text: text.clone(),
                            },
                        );
                    }
                    // Background bands and volume profiles are not drawn by this viewer.
                    Plot::BackgroundBand { .. }
                    | Plot::VolumeProfile { .. }
                    | Plot::VolumeProfileAt { .. } => {}
                }
            }
        }
        out.lines.extend(lines.into_values());
        out.boxes.extend(boxes.into_values());

        for (n, series) in series.iter().enumerate() {
            let points: Vec<(f64, f64)> = series
                .points
                .iter()
                .filter(|(idx, v)| (*idx as usize) < fp_x.len() && v.is_finite())
                .map(|(idx, v)| (mid(*idx as usize), *v))
                .collect();
            if !points.is_empty() {
                out.lines.push(OverlayLine {
                    color: parse_color(&series.color).unwrap_or(PALETTE[n % PALETTE.len()]),
                    points,
                });
            }
        }
        out.markers.sort_by(|a, b| a.x.total_cmp(&b.x));
        out
    }
}

/// Parse the CSS-style color strings plots carry: `#rgb`, `#rrggbb`, `#rrggbbaa`,
/// `rgb(r,g,b)`, `rgba(r,g,b,a)` and a few common names.
pub(crate) fn parse_color(s: &str) -> Option<Color32> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        let digit = |i: usize| u8::from_str_radix(hex.get(i..i + 1)?, 16).ok();
        let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
        return match hex.len() {
            3 => Some(Color32::from_rgb(digit(0)? * 17, digit(1)? * 17, digit(2)? * 17)),
            6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        };
    }
    if let Some(body) = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")) {
        let parts: Vec<f32> = body
            .trim_end_matches(')')
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let channel = |v: f32| v.clamp(0.0, 255.0) as u8;
        let alpha = parts.get(3).map_or(255, |a| (a.clamp(0.0, 1.0) * 255.0) as u8);
        return Some(Color32::from_rgba_unmultiplied(
            channel(parts[0]),
            channel(parts[1]),
            channel(parts[2]),
            alpha,
        ));
    }
    Some(match s.as_str() {
        "red" => Color32::from_rgb(0xef, 0x53, 0x50),
        "green" => Color32::from_rgb(0x26, 0xa6, 0x9a),
        "lime" => Color32::from_rgb(0x8b, 0xd4, 0x50),
        "blue" => Color32::from_rgb(0x42, 0x8b, 0xff),
        "yellow" => Color32::from_rgb(0xf2, 0xd3, 0x4e),
        "orange" => Color32::from_rgb(0xff, 0x9e, 0x40),
        "purple" | "magenta" => Color32::from_rgb(0xc7, 0x7d, 0xff),
        "cyan" | "aqua" => Color32::from_rgb(0x5c, 0xe1, 0xe6),
        "white" => Color32::WHITE,
        "black" => Color32::BLACK,
        "gray" | "grey" => Color32::GRAY,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_css_colors() {
        assert_eq!(parse_color("#fff"), Some(Color32::WHITE));
        assert_eq!(parse_color("#FF8000"), Some(Color32::from_rgb(255, 128, 0)));
        assert_eq!(
            parse_color("rgba(10, 20, 30, 0.5)"),
            Some(Color32::from_rgba_unmultiplied(10, 20, 30, 127))
        );
        assert_eq!(parse_color("rgb(1,2,3)"), Some(Color32::from_rgb(1, 2, 3)));
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#12"), None);
    }
}
