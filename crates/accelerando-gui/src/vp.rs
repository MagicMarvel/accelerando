//! Volume profiles on the price panel: strategy `VolumeProfile*` plots and profiles the user
//! draws by dragging over a stretch of bars.
//!
//! Drawn profiles are stored by time and recomputed from the displayed bars' ladders, so they
//! survive axis and bar-type switches. Smoothing, POC / value area and the peak area use
//! [`accelerando_core::volume_profile`], the same code strategies use.

use accelerando_core::volume_profile::{self, ProfileStats};
use accelerando_core::{Footprint, VpLevel};
use eframe::egui::{self, pos2, Align2, Color32, FontId, Painter, Rect, Shape, Stroke, StrokeKind};

use crate::chart::Mapper;
use crate::{data, Model};

const BUY: Color32 = Color32::from_rgb(0x26, 0xa6, 0x9a);
const SELL: Color32 = Color32::from_rgb(0xef, 0x53, 0x50);
const VOLUME: Color32 = Color32::from_rgb(0x64, 0x74, 0x8b);
const POC: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
const VAH: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
const VAL: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);
const MANUAL_EDGE: Color32 = Color32::from_rgb(0xc7, 0x92, 0xea);
const SELECTION: Color32 = Color32::from_rgba_premultiplied(90, 70, 130, 60);

/// A profile the user drew: the bars whose first trade is in `[t0, t1]`.
pub(crate) struct ManualVp {
    pub t0: i64,
    pub t1: i64,
}

/// State and settings of the volume-profile tool.
pub(crate) struct VpTool {
    /// Drag on the chart draws a profile instead of panning.
    pub drawing: bool,
    /// Gaussian smoothing of drawn profiles, in ticks (0 = raw).
    pub smooth_ticks: f32,
    /// Value-area share of the volume.
    pub va_fraction: f32,
    /// Peak area = contiguous levels with at least this share of the POC volume (0 = off).
    pub peak_fraction: f32,
    /// Extend POC / VAH / VAL of drawn profiles to the right edge of the chart.
    pub extend_lines: bool,
    /// Draw the strategy's own volume profiles.
    pub show_strategy: bool,
    pub items: Vec<ManualVp>,
    /// x at the start of the current drag.
    drag_from: Option<f64>,
}

impl Default for VpTool {
    fn default() -> Self {
        Self {
            drawing: false,
            smooth_ticks: 4.0,
            va_fraction: 0.70,
            peak_fraction: 0.70,
            extend_lines: true,
            show_strategy: true,
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

    /// Profiles drawn by the strategy (behind the bars).
    pub fn draw_strategy(&self, painter: &Painter, model: &Model, m: &Mapper) {
        if !self.show_strategy {
            return;
        }
        let tick = model.result.tick_size.abs().max(1e-9);
        for p in &model.overlays.profiles {
            if p.x1 < m.x0 || p.x0 > m.x1 {
                continue;
            }
            draw_profile(painter, m, p.x0, p.x1, &p.levels, tick, p.color, &p.text);
        }
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
            let raw = volume_profile::profile_from_footprints(&bars[i0..i1], tick);
            let levels = volume_profile::smooth_profile(&raw, tick, self.smooth_ticks as f64);
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
                "VP {}-{}  σ{:.0}",
                data::format_ts(bars[i0].ts_first_ns, model.tz, false, false),
                data::format_ts(bars[i1 - 1].ts_last_ns, model.tz, false, false),
                self.smooth_ticks,
            );
            draw_profile(painter, m, x0, x1, &levels, tick, Some(MANUAL_EDGE), &label);
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

/// POC / VAH / VAL of a drawn profile, optionally extended to the right edge of the chart.
fn draw_levels(painter: &Painter, m: &Mapper, sx0: f32, sx1: f32, s: ProfileStats, extend: bool, d: usize) {
    let right = if extend { m.rect.right() } else { sx1 };
    for (price, color, name, dashed) in
        [(s.poc, POC, "POC", false), (s.vah, VAH, "VAH", true), (s.val, VAL, "VAL", true)]
    {
        let y = m.sy(price);
        let stroke = Stroke::new(1.3, color);
        if dashed {
            painter.extend(Shape::dashed_line(&[pos2(sx0, y), pos2(right, y)], stroke, 6.0, 4.0));
        } else {
            painter.line_segment([pos2(sx0, y), pos2(right, y)], stroke);
        }
        let x = sx1.max(m.rect.left()).min(m.rect.right() - 90.0);
        painter.text(pos2(x + 4.0, y - 1.0), Align2::LEFT_BOTTOM, format!("{name} {price:.d$}"), FontId::proportional(11.0), color);
    }
}

/// Web-studio style profile over `[x0, x1]`: delta bars grow left from the middle, total
/// volume right, the POC level is orange, and a dashed box with a label frames it.
#[allow(clippy::too_many_arguments)]
fn draw_profile(
    painter: &Painter,
    m: &Mapper,
    x0: f64,
    x1: f64,
    levels: &[VpLevel],
    tick: f64,
    color: Option<Color32>,
    text: &str,
) {
    let (sx0, sx1) = (m.sx(x0), m.sx(x1));
    if sx1 - sx0 < 4.0 || levels.is_empty() {
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
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for (j, lv) in levels.iter().enumerate() {
        let (ya, yb) = (m.sy(lv.price + tick * 0.5), m.sy(lv.price - tick * 0.5));
        if yb < m.rect.top() || ya > m.rect.bottom() {
            continue;
        }
        let yb = yb.max(ya + 1.0);
        min_y = min_y.min(ya);
        max_y = max_y.max(yb);
        let total = lv.buy_vol + lv.sell_vol;
        let delta = lv.buy_vol - lv.sell_vol;
        if max_delta > 0.0 {
            let dw = half * (delta.abs() / max_delta) as f32;
            if dw >= 0.5 {
                let c = if delta >= 0.0 { BUY } else { SELL };
                painter.rect_filled(
                    Rect::from_min_max(pos2(mid - dw, ya), pos2(mid, yb)),
                    0.0,
                    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 120),
                );
            }
        }
        let vw = half * (total / max_total) as f32;
        if vw >= 0.5 {
            let c = if j == poc { POC } else { VOLUME };
            painter.rect_filled(
                Rect::from_min_max(pos2(mid, ya), pos2(mid + vw, yb)),
                0.0,
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), if j == poc { 200 } else { 130 }),
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
    painter.extend(Shape::dashed_line(&corners, stroke, 4.0, 3.0));
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
