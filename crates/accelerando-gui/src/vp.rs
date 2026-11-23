//! Volume profiles on the price panel: strategy `VolumeProfile*` plots and profiles the user
//! draws by dragging over a stretch of bars.
//!
//! Drawn profiles are stored by time and recomputed from the displayed bars' ladders, so they
//! survive axis and bar-type switches. Smoothing, POC / value area and the peak area use
//! [`accelerando_core::volume_profile`], the same code strategies use.

use accelerando_core::volume_profile::{self, ProfileStats};
use accelerando_core::{Footprint, VpLevel};
use eframe::egui::{self, pos2, Align2, Color32, FontId, Painter, Rect, Stroke, StrokeKind};

use crate::chart::{draw_dashes, Mapper};
use crate::{data, Model, NEGATIVE_DELTA_COLOR, POSITIVE_DELTA_COLOR, TOTAL_VOLUME_COLOR};

const POC: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
const VAH: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
const VAL: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);
const MANUAL_EDGE: Color32 = Color32::from_rgb(0xc7, 0x92, 0xea);
const SELECTION: Color32 = Color32::from_rgba_premultiplied(90, 70, 130, 60);

struct ProfileColors {
    positive_delta: Color32,
    negative_delta: Color32,
    poc_volume: Color32,
}

const MANUAL_COLORS: ProfileColors = ProfileColors {
    positive_delta: POSITIVE_DELTA_COLOR,
    negative_delta: NEGATIVE_DELTA_COLOR,
    poc_volume: TOTAL_VOLUME_COLOR,
};

const STRATEGY_COLORS: ProfileColors = ProfileColors {
    positive_delta: Color32::from_rgba_unmultiplied_const(0x26, 0xa6, 0x9a, 120),
    negative_delta: Color32::from_rgba_unmultiplied_const(0xef, 0x53, 0x50, 120),
    poc_volume: Color32::from_rgba_unmultiplied_const(POC.r(), POC.g(), POC.b(), 200),
};

/// A profile the user drew: the bars whose first trade is in `[t0, t1]`.
pub(crate) struct ManualVp {
    pub t0: i64,
    pub t1: i64,
}

/// State and settings of the volume-profile tool.
pub(crate) struct VpTool {
    /// Drag on the chart draws a profile instead of panning.
    pub drawing: bool,
    /// Value-area share of the volume.
    pub va_fraction: f32,
    /// Peak area = contiguous levels with at least this share of the POC volume (0 = off).
    pub peak_fraction: f32,
    /// Extend POC / VAH / VAL of drawn profiles to the right edge of the chart.
    pub extend_lines: bool,
    pub items: Vec<ManualVp>,
    /// x at the start of the current drag.
    drag_from: Option<f64>,
}

impl Default for VpTool {
    fn default() -> Self {
        Self {
            drawing: false,
            va_fraction: 0.70,
            peak_fraction: 0.70,
            extend_lines: true,
            items: Vec::new(),
            drag_from: None,
        }
    }
}

/// Bars whose `[first, last]` x range overlaps `[xa, xb]`.
fn bar_range(model: &Model, xa: f64, xb: f64) -> (usize, usize) {
    let i0 = model.fp_x.partition_point(|f| f.1 < xa);
    let i1 = model.fp_x.partition_point(|f| f.0 <= xb);
    (i0, i1.max(i0))
}

/// Bars whose first trade lies in `[t0, t1]`.
fn bars_in_time(bars: &[Footprint], t0: i64, t1: i64) -> (usize, usize) {
    let i0 = bars.partition_point(|f| f.ts_first_ns < t0);
    let i1 = bars.partition_point(|f| f.ts_first_ns <= t1);
    (i0, i1.max(i0))
}

impl VpTool {
    /// Handle drawing / deleting on the price panel. Returns `true` when the drag was used here
    /// (the chart must not pan).
    pub fn handle_input(&mut self, ui: &egui::Ui, response: &egui::Response, m: &Mapper, model: &Model) -> bool {
        if self.drawing && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.drawing = false;
            self.drag_from = None;
        }
        if response.secondary_clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                self.delete_at(model, m.x_at(pos.x));
            }
        }
        if !self.drawing {
            self.drag_from = None;
            return false;
        }
        if response.drag_started_by(egui::PointerButton::Primary) {
            self.drag_from = response.interact_pointer_pos().map(|p| m.x_at(p.x));
        }
        if response.drag_stopped() {
            if let (Some(from), Some(pos)) = (self.drag_from.take(), response.interact_pointer_pos()) {
                let to = m.x_at(pos.x);
                self.add(model, from.min(to), from.max(to));
            }
        }
        true
    }

    fn add(&mut self, model: &Model, xa: f64, xb: f64) {
        let (i0, i1) = bar_range(model, xa, xb);
        let bars = model.bars();
        if i1 <= i0 {
            return;
        }
        self.items.push(ManualVp { t0: bars[i0].ts_first_ns, t1: bars[i1 - 1].ts_first_ns });
        self.drawing = false;
    }

    /// Remove the most recent drawn profile under `x`.
    fn delete_at(&mut self, model: &Model, x: f64) {
        let bars = model.bars();
        if let Some(k) = self.items.iter().rposition(|vp| {
            let (i0, i1) = bars_in_time(bars, vp.t0, vp.t1);
            i1 > i0 && model.fp_x[i0].0 <= x && x <= model.fp_x[i1 - 1].1
        }) {
            self.items.remove(k);
        }
    }

    pub fn controls(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(260.0);
        ui.strong("Manual VP settings");
        ui.add(egui::Slider::new(&mut self.va_fraction, 0.3..=0.95).text("Value area"));
        ui.add(
            egui::Slider::new(&mut self.peak_fraction, 0.0..=0.95)
                .text("Peak area (0 = off)"),
        )
        .on_hover_text("Shade the contiguous levels with at least this share of the POC volume.");
        ui.checkbox(&mut self.extend_lines, "Extend POC / VAH / VAL to the right");
    }

    /// Drawn profiles, their levels and the current selection (on top of the bars).
    pub fn draw_manual(&self, painter: &Painter, model: &Model, m: &Mapper, hover_x: Option<f64>) {
        let tick = model.result.tick_size.abs().max(1e-9);
        let bars = model.bars();
        let d = model.price_decimals;
        for vp in &self.items {
            let (i0, i1) = bars_in_time(bars, vp.t0, vp.t1);
            if i1 <= i0 {
                continue;
            }
            let (x0, x1) = (model.fp_x[i0].0, model.fp_x[i1 - 1].1);
            if x0 > m.x1 || (x1 < m.x0 && !self.extend_lines) {
                continue;
            }
            let levels = volume_profile::profile_from_footprints(&bars[i0..i1], tick);
            let Some(stats) = volume_profile::value_area(&levels, self.va_fraction as f64) else {
                continue;
            };
            let (sx0, sx1) = (m.sx(x0), m.sx(x1));
            if self.peak_fraction > 0.0 {
                if let Some((_, lo, hi)) = volume_profile::peak_area(&levels, self.peak_fraction as f64) {
                    let rect = Rect::from_min_max(
                        pos2(sx0, m.sy(hi + tick * 0.5)),
                        pos2(sx1, m.sy(lo - tick * 0.5)),
                    );
                    painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(245, 158, 11, 34));
                }
            }
            let label = format!(
                "VP {}-{}",
                data::format_ts(bars[i0].ts_first_ns, model.tz, false, false),
                data::format_ts(bars[i1 - 1].ts_last_ns, model.tz, false, false),
            );
            draw_profile(painter, m, x0, x1, &levels, tick, &MANUAL_COLORS, Some(MANUAL_EDGE), &label);
            draw_levels(painter, m, sx0, sx1, stats, self.extend_lines, d);
        }
        // Selection while dragging.
        if let (Some(from), Some(to)) = (self.drag_from, hover_x) {
            let (xa, xb) = (from.min(to), from.max(to));
            let rect = Rect::from_min_max(pos2(m.sx(xa), m.rect.top()), pos2(m.sx(xb), m.rect.bottom()));
            painter.rect(rect, 0.0, SELECTION, Stroke::new(1.0, MANUAL_EDGE), StrokeKind::Middle);
            let (i0, i1) = bar_range(model, xa, xb);
            painter.text(
                rect.center_top() + egui::vec2(0.0, 22.0),
                Align2::CENTER_TOP,
                format!("{} bars", i1 - i0),
                FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
        if self.drawing {
            painter.text(
                m.rect.center_top() + egui::vec2(0.0, 6.0),
                Align2::CENTER_TOP,
                "Draw VP: drag across bars · right-click a profile to delete · Esc to stop",
                FontId::proportional(12.0),
                MANUAL_EDGE,
            );
        }
    }
}

/// Profiles supplied by the backtest, drawn behind the bars.
pub(crate) fn draw_strategy_profiles(painter: &Painter, model: &Model, m: &Mapper) {
    let tick = model.result.tick_size.abs().max(1e-9);
    for profile in &model.overlays.profiles {
        if profile.x1 >= m.x0 && profile.x0 <= m.x1 {
            draw_profile(
                painter, m, profile.x0, profile.x1, &profile.levels, tick, &STRATEGY_COLORS, profile.color, &profile.text,
            );
        }
    }
}

/// POC / VAH / VAL of a drawn profile, optionally extended to the right edge of the chart.
fn draw_levels(painter: &Painter, m: &Mapper, sx0: f32, sx1: f32, s: ProfileStats, extend: bool, d: usize) {
    let right = if extend { m.rect.right() } else { sx1 };
    let clip = painter.clip_rect();
    for (price, color, name, dashed) in
        [(s.poc, POC, "POC", false), (s.vah, VAH, "VAH", true), (s.val, VAL, "VAL", true)]
    {
        let y = m.sy(price);
        if y < clip.top() || y > clip.bottom() {
            continue;
        }
        let stroke = Stroke::new(1.3, color);
        if dashed {
            draw_dashes(painter, &[pos2(sx0, y), pos2(right, y)], stroke, 6.0, 4.0);
        } else if sx0.max(clip.left()) <= right.min(clip.right()) {
            painter.line_segment([pos2(sx0.max(clip.left()), y), pos2(right.min(clip.right()), y)], stroke);
        }
        let x = sx1.max(m.rect.left()).min(m.rect.right() - 90.0);
        painter.text(pos2(x + 4.0, y - 1.0), Align2::LEFT_BOTTOM, format!("{name} {price:.d$}"), FontId::proportional(11.0), color);
    }
}

/// Delta bars grow left from the middle and total volume grows right, framed by a dashed box.
#[allow(clippy::too_many_arguments)]
fn draw_profile(
    painter: &Painter,
    m: &Mapper,
    x0: f64,
    x1: f64,
    levels: &[VpLevel],
    tick: f64,
    colors: &ProfileColors,
    color: Option<Color32>,
    text: &str,
) {
    let (sx0, sx1) = (m.sx(x0), m.sx(x1));
    if !sx0.is_finite() || !sx1.is_finite() || sx1 - sx0 < 4.0 || levels.is_empty() {
        return;
    }
    let (mut max_total, mut max_delta, mut poc) = (0f64, 0f64, 0usize);
    for (j, lv) in levels.iter().enumerate() {
        let total = lv.buy_vol + lv.sell_vol;
        if total > max_total {
            max_total = total;
            poc = j;
        }
        max_delta = max_delta.max((lv.buy_vol - lv.sell_vol).abs());
    }
    if max_total <= 0.0 {
        return;
    }
    let width = sx1 - sx0;
    let (half, mid) = (width * 0.5, sx0 + width * 0.5);
    let clip = painter.clip_rect();
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for (j, lv) in levels.iter().enumerate() {
        let (ya, yb) = (m.sy(lv.price + tick * 0.5), m.sy(lv.price - tick * 0.5));
        if yb < clip.top() || ya > clip.bottom() {
            continue;
        }
        let yb = yb.max(ya + 1.0);
        min_y = min_y.min(ya);
        max_y = max_y.max(yb);
        let total = lv.buy_vol + lv.sell_vol;
        let delta = lv.buy_vol - lv.sell_vol;
        if max_delta > 0.0 {
            let dw = half * (delta.abs() / max_delta) as f32;
            let rect = Rect::from_min_max(pos2(mid - dw, ya), pos2(mid, yb)).intersect(clip);
            if dw >= 0.5 && rect.is_positive() {
                let fill = if delta > 0.0 { colors.positive_delta } else { colors.negative_delta };
                painter.rect_filled(rect, 0.0, fill);
            }
        }
        let vw = half * (total / max_total) as f32;
        let rect = Rect::from_min_max(pos2(mid, ya), pos2(mid + vw, yb)).intersect(clip);
        if vw >= 0.5 && rect.is_positive() {
            let fill = if j == poc { colors.poc_volume } else { TOTAL_VOLUME_COLOR };
            painter.rect_filled(
                rect,
                0.0,
                fill,
            );
        }
    }
    if !min_y.is_finite() {
        return;
    }
    let edge = color.unwrap_or(Color32::from_rgb(0x94, 0xa3, 0xb8));
    let rect = Rect::from_min_max(pos2(sx0, min_y), pos2(sx1, max_y));
    let stroke = Stroke::new(1.2, Color32::from_rgba_unmultiplied(edge.r(), edge.g(), edge.b(), 200));
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
    draw_dashes(painter, &corners, stroke, 4.0, 3.0);
    if !text.is_empty() && width >= 24.0 {
        painter.text(
            pos2(mid, (min_y - 3.0).max(m.rect.top() + 12.0)),
            Align2::CENTER_BOTTOM,
            text,
            FontId::proportional(11.0),
            edge,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Shape;

    #[test]
    fn manual_profile_uses_footprint_colors_for_both_sides_and_poc_volume() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 200.0));
        let mapper = Mapper { rect, x0: 0.0, x1: 1.0, p0: 99.0, p1: 101.0 };
        let levels = [
            VpLevel { price: 100.0, buy_vol: 10.0, sell_vol: 2.0 },
            VpLevel { price: 100.25, buy_vol: 1.0, sell_vol: 6.0 },
        ];
        let input = egui::RawInput { screen_rect: Some(rect), ..Default::default() };
        let mut output = egui::Context::default().run_ui(input, |ui| {
            draw_profile(ui.painter(), &mapper, 0.0, 1.0, &levels, 0.25, &MANUAL_COLORS, None, "");
        });
        output.textures_delta.clear();
        let fills = output.shapes.iter().filter_map(|entry| match &entry.shape {
            Shape::Rect(rect) => Some(rect.fill),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(fills, [POSITIVE_DELTA_COLOR, TOTAL_VOLUME_COLOR, NEGATIVE_DELTA_COLOR, TOTAL_VOLUME_COLOR]);
    }

    #[test]
    fn zoomed_profile_border_only_emits_visible_dashes() {
        let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(700.0, 100.0));
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(clip), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            let painter = ui.painter().with_clip_rect(clip);
            let m = Mapper { rect: clip, x0: 0.0, x1: 1.0, p0: 99.0, p1: 101.0 };
            let levels = [VpLevel { price: 100.0, buy_vol: 10.0, sell_vol: 2.0 }];
            draw_profile(&painter, &m, -10_000.0, 10_001.0, &levels, 0.25, &MANUAL_COLORS, None, "");
        });
        output.textures_delta.clear();
        let dashes: Vec<_> = output.shapes.iter().filter_map(|entry| {
            match entry.shape {
                Shape::LineSegment { points, .. } => Some(points),
                _ => None,
            }
        }).collect();
        assert!((190..=204).contains(&dashes.len()));
        assert!(dashes.into_iter().flatten().all(|point| clip.expand(1.3).contains(point)));
    }
}
