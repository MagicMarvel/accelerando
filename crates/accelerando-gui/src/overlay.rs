//! Strategy and indicator overlays resolved to chart coordinates as bars become visible.

use std::collections::{BTreeMap, BTreeSet};

use accelerando_core::result::Series;
use accelerando_core::{Footprint, Plot, VpLevel};
use eframe::egui::Color32;

pub(crate) struct OverlayLine {
    pub color: Color32,
    /// `(x, value)`, ascending in x.
    pub points: Vec<(f64, f64)>,
}

pub(crate) struct OverlayMarker {
    pub group: Option<String>,
    pub x: f64,
    pub price: f64,
    pub shape: String,
    pub color: Color32,
    pub text: String,
}

pub(crate) struct OverlaySegment {
    pub group: Option<String>,
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub color: Color32,
    pub dashed: bool,
    pub text: String,
}

pub(crate) struct OverlayBox {
    pub group: Option<String>,
    pub x0: f64,
    pub x1: f64,
    pub low: f64,
    pub high: f64,
    pub color: Color32,
    pub text: String,
}

/// A volume profile drawn over the bars `[x0, x1]` (strategy `VolumeProfile*` plots).
pub(crate) struct OverlayProfile {
    id: String,
    pub x0: f64,
    pub x1: f64,
    pub levels: Vec<VpLevel>,
    pub color: Option<Color32>,
    pub text: String,
}

#[derive(Default)]
pub(crate) struct Overlays {
    bars: usize,
    pub groups: BTreeSet<String>,
    /// Volume profiles, one per id (the last emission wins), sorted by `x0`.
    pub profiles: Vec<OverlayProfile>,
    plot_lines: BTreeMap<String, OverlayLine>,
    series_lines: Vec<OverlayLine>,
    /// Sorted by x.
    pub markers: Vec<OverlayMarker>,
    pub segments: Vec<OverlaySegment>,
    pub boxes: BTreeMap<(Option<String>, String), OverlayBox>,
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
    pub fn sync(
        &mut self,
        footprints: &[Footprint],
        series: &[Series],
        bar_x: &dyn Fn(usize) -> (f64, f64),
    ) {
        let end = footprints.len();
        if self.bars > end {
            *self = Self::default();
        }
        let start = self.bars;
        if start == end {
            return;
        }
        let mid = |i: usize| {
            let (left, right) = bar_x(i);
            (left + right) * 0.5
        };
        let back = |i: usize, bars: usize| i.saturating_sub(bars);
        let groups = &mut self.groups;
        let mut group_name = |group: &Option<String>| {
            let name = group.as_deref()?.trim();
            if name.is_empty() {
                return None;
            }
            if !groups.contains(name) {
                groups.insert(name.to_owned());
            }
            Some(name.to_owned())
        };
        // Profiles may also grow under one id; anonymous ones get a unique key.
        let mut profiles: BTreeMap<String, OverlayProfile> = BTreeMap::new();
        let markers_start = self.markers.len();

        for (i, fp) in footprints.iter().enumerate().skip(start) {
            for plot in &fp.plots {
                match plot {
                    Plot::Line { id, value, color } => {
                        if value.is_finite() {
                            self.plot_lines
                                .entry(id.clone())
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
                        group,
                    } => self.segments.push(OverlaySegment {
                        group: group_name(group),
                        from: (mid(back(i, *from_bars_back)), *from_price),
                        to: (mid(back(i, *to_bars_back)), *to_price),
                        color: parse_color(color).unwrap_or(PALETTE[1]),
                        dashed: *dashed,
                        text: text.clone(),
                    }),
                    Plot::Marker {
                        price,
                        shape,
                        color,
                        text,
                        group,
                        ..
                    } => {
                        if group.as_deref() == Some("big_trades") {
                            continue;
                        }
                        self.markers.push(OverlayMarker {
                            group: group_name(group),
                            x: mid(i),
                            price: *price,
                            shape: shape.clone(),
                            color: parse_color(color).unwrap_or(PALETTE[0]),
                            text: text.clone(),
                        })
                    }
                    Plot::MarkerAt {
                        bars_back,
                        price,
                        shape,
                        color,
                        text,
                        group,
                        ..
                    } => self.markers.push(OverlayMarker {
                        group: group_name(group),
                        x: mid(back(i, *bars_back)),
                        price: *price,
                        shape: shape.clone(),
                        color: parse_color(color).unwrap_or(PALETTE[0]),
                        text: text.clone(),
                    }),
                    Plot::PriceBox {
                        id,
                        low,
                        high,
                        span,
                        color,
                        text,
                        group,
                    } => {
                        self.boxes.insert(
                            (group.clone(), id.clone()),
                            OverlayBox {
                                group: group_name(group),
                                x0: bar_x(back(i, *span)).0,
                                x1: bar_x(i).1,
                                low: *low,
                                high: *high,
                                color: parse_color(color).unwrap_or(PALETTE[2]),
                                text: text.clone(),
                            },
                        );
                    }
                    Plot::PriceBoxAt {
                        bars_back,
                        id,
                        low,
                        high,
                        span,
                        color,
                        text,
                        group,
                    } => {
                        let end = back(i, *bars_back);
                        self.boxes.insert(
                            (group.clone(), id.clone()),
                            OverlayBox {
                                group: group_name(group),
                                x0: bar_x(back(end, *span)).0,
                                x1: bar_x(end).1,
                                low: *low,
                                high: *high,
                                color: parse_color(color).unwrap_or(PALETTE[2]),
                                text: text.clone(),
                            },
                        );
                    }
                    Plot::VolumeProfile { id, levels, span } => {
                        let key = if id.is_empty() {
                            format!("__vp_{i}")
                        } else {
                            id.clone()
                        };
                        let start = back(i, span.saturating_sub(1));
                        profiles.insert(
                            key.clone(),
                            OverlayProfile {
                                id: key,
                                x0: bar_x(start).0,
                                x1: bar_x(i).1,
                                levels: levels.clone(),
                                color: None,
                                text: String::new(),
                            },
                        );
                    }
                    Plot::VolumeProfileAt {
                        bars_back,
                        id,
                        levels,
                        span,
                        text,
                        color,
                    } => {
                        let key = if id.is_empty() {
                            format!("__vp_{i}")
                        } else {
                            id.clone()
                        };
                        let end = back(i, *bars_back);
                        let start = back(end, span.saturating_sub(1));
                        profiles.insert(
                            key.clone(),
                            OverlayProfile {
                                id: key,
                                x0: bar_x(start).0,
                                x1: bar_x(end).1,
                                levels: levels.clone(),
                                color: color.as_deref().and_then(parse_color),
                                text: text.clone(),
                            },
                        );
                    }
                    // Background bands are not drawn by this viewer.
                    Plot::BackgroundBand { .. } => {}
                }
            }
        }
        if !profiles.is_empty() {
            self.profiles
                .retain(|profile| !profiles.contains_key(&profile.id));
            self.profiles.extend(
                profiles
                    .into_values()
                    .filter(|profile| !profile.levels.is_empty()),
            );
            self.profiles
                .sort_by(|a, b| a.x0.total_cmp(&b.x0).then_with(|| a.id.cmp(&b.id)));
        }

        if start == 0 {
            self.series_lines = series
                .iter()
                .enumerate()
                .map(|(n, line)| OverlayLine {
                    color: parse_color(&line.color).unwrap_or(PALETTE[n % PALETTE.len()]),
                    points: Vec::new(),
                })
                .collect();
        }
        for (line, series) in self.series_lines.iter_mut().zip(series) {
            let first = series
                .points
                .partition_point(|&(idx, _)| (idx as usize) < start);
            let last =
                first + series.points[first..].partition_point(|&(idx, _)| (idx as usize) < end);
            line.points.extend(
                series.points[first..last]
                    .iter()
                    .filter(|(_, value)| value.is_finite())
                    .map(|&(idx, value)| (mid(idx as usize), value)),
            );
        }
        self.markers[markers_start..].sort_by(|a, b| a.x.total_cmp(&b.x));
        if markers_start > 0
            && self.markers.get(markers_start).is_some_and(|first| {
                self.markers[markers_start - 1]
                    .x
                    .total_cmp(&first.x)
                    .is_gt()
            })
        {
            // A newly confirmed marker can be anchored to an earlier bar.
            self.markers.sort_by(|a, b| a.x.total_cmp(&b.x));
        }
        self.bars = end;
    }

    pub fn lines(&self) -> impl Iterator<Item = &OverlayLine> {
        self.plot_lines.values().chain(
            self.series_lines
                .iter()
                .filter(|line| !line.points.is_empty()),
        )
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
            3 => Some(Color32::from_rgb(
                digit(0)? * 17,
                digit(1)? * 17,
                digit(2)? * 17,
            )),
            6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(Color32::from_rgba_unmultiplied(
                byte(0)?,
                byte(2)?,
                byte(4)?,
                byte(6)?,
            )),
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
        let alpha = parts
            .get(3)
            .map_or(255, |a| (a.clamp(0.0, 1.0) * 255.0) as u8);
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
    fn mixed_plot_types_share_one_named_legend_entry() {
        let mut bar = Footprint::seed(0, 100.0);
        let group = Some("POC 回踩".to_owned());
        bar.plots.push(Plot::PriceBox {
            id: "peak".into(),
            low: 99.0,
            high: 101.0,
            span: 0,
            color: "#fff".into(),
            text: String::new(),
            group: group.clone(),
        });
        bar.plots.push(Plot::MarkerAt {
            bars_back: 0,
            price: 100.0,
            shape: "circle".into(),
            color: "#fff".into(),
            text: String::new(),
            text_dx: None,
            text_dy: None,
            group: group.clone(),
        });
        bar.plots.push(Plot::LineSegment {
            from_bars_back: 0,
            from_price: 99.0,
            to_bars_back: 0,
            to_price: 101.0,
            color: "#fff".into(),
            dashed: false,
            text: String::new(),
            group: group.clone(),
        });
        let mut overlays = Overlays::default();
        overlays.sync(&[bar], &[], &|_| (0.0, 1.0));
        assert_eq!(overlays.groups, BTreeSet::from(["POC 回踩".to_owned()]));
        assert_eq!(overlays.boxes.values().next().unwrap().group, group);
        assert_eq!(overlays.markers[0].group, group);
        assert_eq!(overlays.segments[0].group, group);
    }

    #[test]
    fn incremental_replay_updates_only_closed_bars_and_rewinds_replacements() {
        let mut bars = [
            Footprint::seed(0, 100.0),
            Footprint::seed(1, 101.0),
            Footprint::seed(2, 102.0),
        ];
        for (i, bar) in bars.iter_mut().enumerate() {
            bar.plots.push(Plot::PriceBox {
                id: "growing".into(),
                low: 99.0,
                high: 100.0 + i as f64,
                span: i,
                color: "#fff".into(),
                text: String::new(),
                group: Some(" group ".into()),
            });
            bar.plots.push(Plot::MarkerAt {
                bars_back: if i == 2 { 2 } else { 0 },
                price: 100.0 + i as f64,
                shape: "circle".into(),
                color: "#fff".into(),
                text: String::new(),
                text_dx: None,
                text_dy: None,
                group: None,
            });
        }
        bars[0].plots.push(Plot::VolumeProfile {
            id: "growing".into(),
            levels: vec![VpLevel {
                price: 100.0,
                buy_vol: 1.0,
                sell_vol: 0.0,
            }],
            span: 1,
        });
        bars[2].plots.push(Plot::VolumeProfile {
            id: "growing".into(),
            levels: Vec::new(),
            span: 1,
        });
        let series = [Series {
            id: "line".into(),
            color: "#fff".into(),
            points: vec![(0, 100.0), (1, f64::NAN), (2, 102.0)],
        }];
        let mut overlays = Overlays::default();
        let bar_x = |i| (i as f64, i as f64 + 1.0);
        overlays.sync(&bars[..2], &series, &bar_x);
        assert_eq!(overlays.boxes.values().next().unwrap().x1, 2.0);
        assert_eq!(overlays.lines().next().unwrap().points, [(0.5, 100.0)]);
        assert_eq!(overlays.profiles.len(), 1);

        overlays.sync(&bars, &series, &bar_x);
        assert_eq!(overlays.boxes.values().next().unwrap().x1, 3.0);
        assert_eq!(
            overlays.lines().next().unwrap().points,
            [(0.5, 100.0), (2.5, 102.0)]
        );
        assert!(overlays.profiles.is_empty());
        let prices: Vec<_> = overlays.markers.iter().map(|marker| marker.price).collect();
        assert_eq!(prices, [100.0, 102.0, 101.0]);

        overlays.sync(&bars[..1], &series, &bar_x);
        assert_eq!(overlays.boxes.values().next().unwrap().x1, 1.0);
        assert_eq!(overlays.lines().next().unwrap().points, [(0.5, 100.0)]);
        assert_eq!(overlays.profiles.len(), 1);
        assert_eq!(overlays.markers.len(), 1);
        overlays.sync(&[], &series, &bar_x);
        assert!(overlays.groups.is_empty());
        assert!(overlays.lines().next().is_none());
    }

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
