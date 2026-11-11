//! Painting and interaction for the price panel and the equity panel.

use eframe::egui::{
    self, pos2, Align2, Color32, ColorImage, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, TextureHandle, TextureOptions,
};

use crate::data::{self, HeatMetric, RasterSpec};
use crate::{Model, Settings};

const AXIS_W: f32 = 72.0;
const AXIS_H: f32 = 22.0;

const BACKGROUND: Color32 = Color32::from_rgb(8, 12, 24);
const GRID: Color32 = Color32::from_rgba_premultiplied(40, 46, 60, 110);
const AXIS_TEXT: Color32 = Color32::from_rgb(150, 158, 175);
const SESSION_LINE: Color32 = Color32::from_rgba_premultiplied(70, 78, 100, 160);
const CROSSHAIR: Color32 = Color32::from_rgba_premultiplied(150, 150, 150, 150);
const GREEN: Color32 = Color32::from_rgb(0x26, 0xa6, 0x9a);
const RED: Color32 = Color32::from_rgb(0xef, 0x53, 0x50);
const BID_LINE: Color32 = Color32::from_rgb(0x4a, 0xe3, 0x8a);
const ASK_LINE: Color32 = Color32::from_rgb(0xff, 0x7a, 0x6b);
const EQUITY_LINE: Color32 = Color32::from_rgb(0x7f, 0xb8, 0xff);
pub(crate) const GAIN: Color32 = Color32::from_rgb(0x5f, 0xd3, 0x8d);
pub(crate) const LOSS: Color32 = Color32::from_rgb(0xff, 0x7b, 0x72);

/// The visible window plus the cached heatmap texture.
pub(crate) struct ChartState {
    pub x0: f64,
    pub x1: f64,
    pub p0: f64,
    pub p1: f64,
    pub auto_y: bool,
    total: f64,
    heat: Option<HeatCache>,
}

#[derive(Clone, PartialEq)]
struct HeatKey {
    spec: RasterSpec,
    metric: HeatMetric,
    contrast: f32,
    smooth: bool,
}

struct HeatCache {
    key: HeatKey,
    texture: TextureHandle,
}

impl ChartState {
    pub fn new(total: f64) -> Self {
        Self {
            x0: 0.0,
            x1: total.max(1.0),
            p0: 0.0,
            p1: 1.0,
            auto_y: true,
            total: total.max(1.0),
            heat: None,
        }
    }

    /// Show the whole data set.
    pub fn fit_all(&mut self) {
        self.x0 = 0.0;
        self.x1 = self.total;
        self.auto_y = true;
    }

    /// Show the most recent `secs` seconds of the data.
    pub fn show_latest(&mut self, secs: f64) {
        self.x1 = self.total;
        self.x0 = self.total - secs;
        self.clamp_x();
    }

    /// The axis length changed (x axis mode switch).
    pub fn set_total(&mut self, total: f64) {
        self.total = total.max(1.0);
        self.heat = None;
    }

    /// Show exactly `[x0, x1]` (clamped to the data).
    pub fn show_range(&mut self, x0: f64, x1: f64) {
        self.x0 = x0;
        self.x1 = x1.max(x0 + 1.0);
        self.clamp_x();
    }

    /// Center on `center`, showing at least `span` (and at least `min_span`) x units.
    pub fn focus(&mut self, center: f64, span: f64, min_span: f64) {
        let span = span.max(min_span);
        self.x0 = center - span * 0.5;
        self.x1 = center + span * 0.5;
        self.auto_y = true;
        self.clamp_x();
    }

    fn clamp_x(&mut self) {
        let span = (self.x1 - self.x0).clamp(5.0, self.total.max(5.0) * 1.1);
        let center = (self.x0 + self.x1) * 0.5;
        self.x0 = center - span * 0.5;
        self.x1 = center + span * 0.5;
        let pad = span * 0.1;
        if self.x0 < -pad {
            self.x1 += -pad - self.x0;
            self.x0 = -pad;
        }
        if self.x1 > self.total + pad {
            self.x0 -= self.x1 - (self.total + pad);
            self.x1 = self.total + pad;
        }
    }
}

/// Screen mapping for one frame.
#[derive(Clone, Copy)]
struct Mapper {
    rect: Rect,
    x0: f64,
    x1: f64,
    p0: f64,
    p1: f64,
}

impl Mapper {
    fn sx(&self, x: f64) -> f32 {
        self.rect.left() + ((x - self.x0) / (self.x1 - self.x0)) as f32 * self.rect.width()
    }
    fn sy(&self, p: f64) -> f32 {
        self.rect.top() + ((self.p1 - p) / (self.p1 - self.p0)) as f32 * self.rect.height()
    }
    fn pos(&self, x: f64, p: f64) -> Pos2 {
        pos2(self.sx(x), self.sy(p))
    }
    fn x_at(&self, px: f32) -> f64 {
        self.x0 + ((px - self.rect.left()) / self.rect.width()) as f64 * (self.x1 - self.x0)
    }
    fn p_at(&self, py: f32) -> f64 {
        self.p1 - ((py - self.rect.top()) / self.rect.height()) as f64 * (self.p1 - self.p0)
    }
}

pub(crate) fn price_panel(
    ui: &mut egui::Ui,
    model: &Model,
    settings: &Settings,
    state: &mut ChartState,
    selected_trade: Option<usize>,
) {
    let outer = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(outer, Sense::click_and_drag());
    let plot = Rect::from_min_max(
        outer.min,
        pos2(outer.right() - AXIS_W, outer.bottom() - AXIS_H),
    );
    if plot.width() < 60.0 || plot.height() < 60.0 {
        return;
    }

    handle_input(ui, &response, plot, state);
    if state.auto_y {
        if let Some((lo, hi)) = visible_price_range(model, settings, state.x0, state.x1) {
            let pad = ((hi - lo) * 0.06).max(model.result.tick_size.abs() * 3.0).max(1e-9);
            state.p0 = lo - pad;
            state.p1 = hi + pad;
        }
    }
    let m = Mapper { rect: plot, x0: state.x0, x1: state.x1, p0: state.p0, p1: state.p1 };

    let axis_painter = ui.painter_at(outer);
    let painter = ui.painter_at(plot);
    painter.rect_filled(plot, 0.0, BACKGROUND);

    if settings.show_heat && model.book_visible() {
        draw_heatmap(ui.ctx(), &painter, model, settings, state, &m);
    }
    draw_axes(&axis_painter, &painter, model, &m);
    if settings.show_overlays {
        draw_boxes(&painter, model, &m);
    }
    if settings.show_bars {
        draw_bars(&painter, model, settings, &m);
    }
    if settings.show_overlays {
        draw_overlays(&painter, model, &m, settings.show_markers);
    }
    if settings.show_bbo && model.book_visible() {
        draw_grid_series(&painter, &model.bid_smooth, &model.bounds, &m, BID_LINE);
        draw_grid_series(&painter, &model.ask_smooth, &model.bounds, &m, ASK_LINE);
        let legend = plot.right_top() + egui::vec2(-8.0, 6.0);
        let rect = painter.text(legend, Align2::RIGHT_TOP, "ask", FontId::monospace(12.0), ASK_LINE);
        painter.text(
            rect.left_top() - egui::vec2(10.0, 0.0),
            Align2::RIGHT_TOP,
            "bid",
            FontId::monospace(12.0),
            BID_LINE,
        );
    }
    if settings.show_trades {
        draw_trades(&painter, model, settings, &m, selected_trade);
    }
    if let Some(pos) = response.hover_pos().filter(|p| plot.contains(*p)) {
        draw_crosshair(&axis_painter, &painter, model, &m, pos);
    }
}

fn handle_input(ui: &egui::Ui, response: &egui::Response, plot: Rect, state: &mut ChartState) {
    let per_px = (state.x1 - state.x0) / plot.width() as f64;
    if response.dragged() {
        let delta = response.drag_delta();
        state.x0 -= delta.x as f64 * per_px;
        state.x1 -= delta.x as f64 * per_px;
        if !state.auto_y {
            let price_per_px = (state.p1 - state.p0) / plot.height() as f64;
            state.p0 += delta.y as f64 * price_per_px;
            state.p1 += delta.y as f64 * price_per_px;
        }
    }
    if response.double_clicked() {
        state.fit_all();
    }
    if let Some(pos) = response.hover_pos() {
        let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta(), i.zoom_delta()));
        if scroll.y != 0.0 {
            // Wheel: zoom the time axis around the cursor.
            let factor = (-scroll.y as f64 * 0.0025).exp();
            let anchor = state.x0 + (pos.x - plot.left()) as f64 * per_px;
            state.x0 = anchor - (anchor - state.x0) * factor;
            state.x1 = anchor + (state.x1 - anchor) * factor;
        }
        if scroll.x != 0.0 {
            // Shift + wheel / horizontal wheel: pan.
            state.x0 -= scroll.x as f64 * per_px;
            state.x1 -= scroll.x as f64 * per_px;
        }
        if zoom != 1.0 && zoom > 0.0 {
            // Ctrl + wheel / pinch: zoom the price axis around the cursor.
            state.auto_y = false;
            let factor = 1.0 / zoom as f64;
            let frac = ((pos.y - plot.top()) / plot.height()) as f64;
            let anchor = state.p1 - frac * (state.p1 - state.p0);
            state.p0 = anchor - (anchor - state.p0) * factor;
            state.p1 = anchor + (state.p1 - anchor) * factor;
        }
    }
    state.clamp_x();
}

/// Low/high of everything price-like that is visible in `[x0, x1]`.
fn visible_price_range(model: &Model, settings: &Settings, x0: f64, x1: f64) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    if settings.show_bars || !model.book_visible() {
        let i0 = model.fp_x.partition_point(|f| f.1 < x0);
        let i1 = model.fp_x.partition_point(|f| f.0 <= x1);
        for fp in &model.bars()[i0..i1.max(i0)] {
            lo = lo.min(fp.low);
            hi = hi.max(fp.high);
        }
    }
    if model.book_visible() && (settings.show_bbo || settings.show_heat) {
        let n = model.bid_smooth.len();
        let ia = (x0.floor().max(0.0) as usize).min(n);
        let ib = (x1.ceil().max(0.0) as usize).min(n);
        let step = ((ib.saturating_sub(ia)) / 4000).max(1);
        for i in (ia..ib).step_by(step) {
            let (bid, ask) = (model.bid_smooth[i] as f64, model.ask_smooth[i] as f64);
            if bid > 0.0 {
                lo = lo.min(bid);
                hi = hi.max(bid);
            }
            if ask > 0.0 {
                lo = lo.min(ask);
                hi = hi.max(ask);
            }
        }
    }
    (lo.is_finite() && hi.is_finite()).then_some((lo, hi))
}

fn draw_heatmap(
    ctx: &egui::Context,
    painter: &Painter,
    model: &Model,
    settings: &Settings,
    state: &mut ChartState,
    m: &Mapper,
) {
    let spec = RasterSpec::new(
        m.x0,
        m.x1,
        m.rect.width(),
        m.p0,
        m.p1,
        m.rect.height(),
        model.result.tick_size,
    );
    let key = HeatKey {
        spec,
        metric: settings.heat_metric,
        contrast: settings.heat_contrast,
        smooth: settings.heat_smooth,
    };
    if state.heat.as_ref().is_none_or(|h| h.key != key) {
        let cells = data::rasterize(
            &spec,
            &model.grid,
            &model.result.liquidity_heatmap.snapshots,
            settings.heat_metric,
        );
        let lut: Vec<Color32> = (0..256).map(|i| heat_color(i as f32 / 255.0)).collect();
        let denom = model.heat_ref.ln_1p().max(1e-6);
        let color = |v: f32| {
            if v > 0.0 {
                let t = (v.ln_1p() / denom).clamp(0.0, 1.0).powf(settings.heat_contrast);
                lut[(t * 255.0) as usize]
            } else {
                Color32::TRANSPARENT
            }
        };
        // Smoothing interpolates along time only. Price levels are discrete, so each raster row
        // is repeated to about one texel per screen pixel; the linear filter then blends
        // neighbouring seconds but keeps the edge between two price levels about a pixel wide.
        let repeat = if settings.heat_smooth {
            let px_per_row = m.rect.height() as f64 * spec.row_p / (m.p1 - m.p0);
            (px_per_row.floor() as usize).clamp(1, (2048 / spec.rows).max(1))
        } else {
            1
        };
        let mut pixels: Vec<Color32> = Vec::with_capacity(spec.cols * spec.rows * repeat);
        for row in cells.chunks_exact(spec.cols) {
            let start = pixels.len();
            pixels.extend(row.iter().map(|&v| color(v)));
            for _ in 1..repeat {
                pixels.extend_from_within(start..start + spec.cols);
            }
        }
        let image = ColorImage::new([spec.cols, spec.rows * repeat], pixels);
        let options = if settings.heat_smooth {
            TextureOptions::LINEAR
        } else {
            TextureOptions::NEAREST
        };
        match state.heat.as_mut() {
            Some(cache) => {
                cache.texture.set(image, options);
                cache.key = key;
            }
            None => {
                let texture = ctx.load_texture("liquidity-heatmap", image, options);
                state.heat = Some(HeatCache { key, texture });
            }
        }
    }
    let Some(cache) = state.heat.as_ref() else { return };
    let (xa, xb) = spec.x_range();
    let (top, bottom) = spec.price_range();
    let rect = Rect::from_min_max(m.pos(xa, top), m.pos(xb, bottom));
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.image(cache.texture.id(), rect, uv, Color32::WHITE);
}

/// Bookmap-like ramp: deep blue → cyan → yellow → orange → white.
fn heat_color(t: f32) -> Color32 {
    const STOPS: [(f32, [f32; 3]); 7] = [
        (0.00, [12.0, 20.0, 48.0]),
        (0.15, [18.0, 44.0, 112.0]),
        (0.40, [22.0, 116.0, 190.0]),
        (0.60, [44.0, 190.0, 172.0]),
        (0.78, [245.0, 216.0, 72.0]),
        (0.92, [250.0, 132.0, 32.0]),
        (1.00, [255.0, 250.0, 240.0]),
    ];
    let t = t.clamp(0.0, 1.0);
    for pair in STOPS.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if t <= t1 {
            let f = (t - t0) / (t1 - t0);
            let ch = |i: usize| (c0[i] + (c1[i] - c0[i]) * f) as u8;
            return Color32::from_rgb(ch(0), ch(1), ch(2));
        }
    }
    Color32::WHITE
}

fn nice_step(raw: f64) -> f64 {
    if !(raw > 0.0) || !raw.is_finite() {
        return 1.0;
    }
    let pow = 10f64.powf(raw.log10().floor());
    let mantissa = raw / pow;
    pow * if mantissa <= 1.0 {
        1.0
    } else if mantissa <= 2.0 {
        2.0
    } else if mantissa <= 5.0 {
        5.0
    } else {
        10.0
    }
}

fn draw_axes(axis_painter: &Painter, painter: &Painter, model: &Model, m: &Mapper) {
    let plot = m.rect;
    let font = FontId::monospace(11.0);

    // Price axis.
    let step = nice_step((m.p1 - m.p0) / (plot.height() as f64 / 48.0))
        .max(model.result.tick_size.abs());
    let mut price = (m.p0 / step).ceil() * step;
    while price <= m.p1 {
        let y = m.sy(price);
        painter.hline(plot.x_range(), y, Stroke::new(1.0, GRID));
        axis_painter.text(
            pos2(plot.right() + 6.0, y),
            Align2::LEFT_CENTER,
            format!("{price:.*}", model.price_decimals),
            font.clone(),
            AXIS_TEXT,
        );
        price += step;
    }

    // Session starts. Closed-market time (overnight, weekends) is left empty: cut out of the
    // time axis, or an empty spacer on the bar axis.
    let sessions = &model.sessions;
    let s0 = sessions.partition_point(|s| s.1 <= m.x0);
    let s1 = sessions.partition_point(|s| s.1 < m.x1);
    if s1 > s0 && s1 - s0 <= 200 {
        let label = s1 - s0 <= 30;
        for &(_, start_x, start_ts) in &sessions[s0..s1] {
            let x = m.sx(start_x);
            painter.vline(x, plot.y_range(), Stroke::new(1.0, SESSION_LINE));
            if label {
                painter.text(
                    pos2(x + 4.0, plot.bottom() - 4.0),
                    Align2::LEFT_BOTTOM,
                    data::format_ts(start_ts, model.utc_offset_secs, true, false),
                    font.clone(),
                    AXIS_TEXT,
                );
            }
        }
    }

    // Time axis: evenly spaced labels (the compressed axis has no globally round ticks).
    let span = m.x1 - m.x0;
    let count = (plot.width() / 130.0).floor().max(1.0) as usize;
    for k in 0..=count {
        let px = plot.left() + plot.width() * k as f32 / count as f32;
        let x = m.x_at(px);
        if x < 0.0 || x > model.xmap.total() {
            continue;
        }
        let ts = model.xmap.to_ts(x);
        let align = if k == 0 {
            Align2::LEFT_TOP
        } else if k == count {
            Align2::RIGHT_TOP
        } else {
            Align2::CENTER_TOP
        };
        axis_painter.text(
            pos2(px, plot.bottom() + 4.0),
            align,
            data::format_ts(ts, model.utc_offset_secs, span > 6.0 * 3600.0, span < 1800.0),
            font.clone(),
            AXIS_TEXT,
        );
    }
}

fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn draw_bars(painter: &Painter, model: &Model, settings: &Settings, m: &Mapper) {
    let (up, down) = if settings.red_up { (RED, GREEN) } else { (GREEN, RED) };
    let fps = model.bars();
    let i0 = model.fp_x.partition_point(|f| f.1 < m.x0);
    let i1 = model.fp_x.partition_point(|f| f.0 <= m.x1);
    if i1 <= i0 {
        return;
    }
    let over_heat = settings.show_heat && model.book_visible();

    if (i1 - i0) as f32 > m.rect.width() / 3.0 {
        // More bars than pixels: one high-low line per pixel column.
        let mut column: Option<(i64, f64, f64, f64, f64)> = None;
        let flush = |c: (i64, f64, f64, f64, f64)| {
            let (px, open, close, low, high) = c;
            let color = if close >= open { up } else { down };
            let x = px as f32 + 0.5;
            let (top, bottom) = (m.sy(high), m.sy(low).max(m.sy(high) + 1.0));
            painter.vline(x, top..=bottom, Stroke::new(1.0, color));
        };
        for i in i0..i1 {
            let fp = &fps[i];
            let px = m.sx((model.fp_x[i].0 + model.fp_x[i].1) * 0.5).floor() as i64;
            match column.as_mut() {
                Some(c) if c.0 == px => {
                    c.2 = fp.close;
                    c.3 = c.3.min(fp.low);
                    c.4 = c.4.max(fp.high);
                }
                _ => {
                    if let Some(c) = column.take() {
                        flush(c);
                    }
                    column = Some((px, fp.open, fp.close, fp.low, fp.high));
                }
            }
        }
        if let Some(c) = column {
            flush(c);
        }
        return;
    }

    // Every body gets the same width: a fraction of the typical (median) bar spacing on screen,
    // so long-lasting bars (a slow session, a bar spanning a pause) are not drawn wider.
    let center_px = |i: usize| m.sx((model.fp_x[i].0 + model.fp_x[i].1) * 0.5);
    let mut spacing: Vec<f32> = {
        let step = ((i1 - i0) / 400).max(1);
        (i0 + 1..i1).step_by(step).map(|i| center_px(i) - center_px(i - 1)).collect()
    };
    spacing.retain(|d| *d > 0.0);
    spacing.sort_by(f32::total_cmp);
    let slot = spacing.get(spacing.len() / 2).copied().unwrap_or_else(|| {
        // A single visible bar: its own width.
        m.sx(model.fp_x[i0].1) - m.sx(model.fp_x[i0].0)
    });
    if settings.show_footprint
        && slot >= FOOTPRINT_MIN_SLOT
        && draw_footprints(painter, model, settings, m, i0, i1, &center_px, slot)
    {
        return;
    }
    let half = (slot * 0.36).clamp(1.5, 30.0);
    for i in i0..i1 {
        let fp = &fps[i];
        let color = if fp.close >= fp.open { up } else { down };
        let center = center_px(i);
        let (xa, xb) = (center - half, center + half);
        // Over the heatmap a bar body is translucent, and just an outline once it is wide
        // enough that a fill would tint the book levels behind it.
        let fill_alpha = match (over_heat, xb - xa > 40.0) {
            (false, _) => 200,
            (true, false) => 70,
            (true, true) => 0,
        };
        painter.vline(center, m.sy(fp.high)..=m.sy(fp.low), Stroke::new(1.0, color));
        let top = m.sy(fp.open.max(fp.close));
        let bottom = m.sy(fp.open.min(fp.close)).max(top + 1.0);
        painter.rect(
            Rect::from_min_max(pos2(xa, top), pos2(xb, bottom)),
            0.0,
            with_alpha(color, fill_alpha),
            Stroke::new(1.0, color),
            StrokeKind::Middle,
        );
    }
}

/// Bars at least this many pixels apart are drawn as footprints (as in the web studio).
pub(crate) const FOOTPRINT_MIN_SLOT: f32 = 32.0;
const FOOTPRINT_TEXT: Color32 = Color32::from_rgb(225, 230, 240);

/// Footprint view: per traded price level a delta profile (left of the bar's middle, green /
/// red by sign) and a volume profile (right), the same shape at every zoom level; delta and
/// volume numbers appear once rows and bars are big enough. A thin candle at the bar's left edge keeps open/close readable, the point
/// of control is outlined and the bar delta is printed under the bar. Returns `false` when the
/// visible bars carry no ladders (the caller then draws candles).
#[allow(clippy::too_many_arguments)]
fn draw_footprints(
    painter: &Painter,
    model: &Model,
    settings: &Settings,
    m: &Mapper,
    i0: usize,
    i1: usize,
    center_px: &dyn Fn(usize) -> f32,
    slot: f32,
) -> bool {
    let fps = &model.bars()[i0..i1];
    let (mut max_side, mut max_total, mut max_delta) = (0f64, 0f64, 0f64);
    for fp in fps {
        for lv in &fp.ladder {
            max_side = max_side.max(lv.buy_vol).max(lv.sell_vol);
            max_total = max_total.max(lv.buy_vol + lv.sell_vol);
            max_delta = max_delta.max((lv.buy_vol - lv.sell_vol).abs());
        }
    }
    if max_side <= 0.0 {
        return false;
    }
    let (up, down) = if settings.red_up { (RED, GREEN) } else { (GREEN, RED) };
    // Aggressor colors stay fixed: buyers green-ish, sellers red-ish, whatever the candle style.
    let (buy_c, sell_c) = (GREEN, RED);
    let tick = model.result.tick_size.abs().max(1e-9);
    let row_px = m.rect.height() * (tick / (m.p1 - m.p0)) as f32;
    let width = slot * 0.88;
    let candle_w = (width * 0.08).clamp(3.0, 6.0);
    let cells_w = width - candle_w - 2.0;
    let text = row_px >= 11.0 && cells_w >= 90.0;
    let font = FontId::monospace((row_px - 1.0).clamp(8.0, 11.0));

    for (k, fp) in fps.iter().enumerate() {
        let x_left = center_px(i0 + k) - width * 0.5;
        let cells_x0 = x_left + candle_w + 2.0;
        let mid = cells_x0 + cells_w * 0.5;
        let dir = if fp.close >= fp.open { up } else { down };

        // Thin candle at the left edge.
        let cx = x_left + candle_w * 0.5;
        painter.vline(cx, m.sy(fp.high)..=m.sy(fp.low), Stroke::new(1.0, dir));
        let top = m.sy(fp.open.max(fp.close));
        let bottom = m.sy(fp.open.min(fp.close)).max(top + 1.0);
        painter.rect_filled(
            Rect::from_min_max(pos2(x_left, top), pos2(x_left + candle_w, bottom)),
            0.0,
            dir,
        );

        let mut poc: Option<(f64, f64)> = None;
        for lv in &fp.ladder {
            let total = lv.buy_vol + lv.sell_vol;
            if poc.is_none_or(|(_, v)| total > v) {
                poc = Some((lv.price, total));
            }
            let (ya, yb) = (m.sy(lv.price + tick * 0.5), m.sy(lv.price - tick * 0.5));
            if yb < m.rect.top() || ya > m.rect.bottom() {
                continue;
            }
            let (ya, yb) = (ya + 0.5, (yb - 0.5).max(ya + 1.0));
            // Delta profile growing left from the middle, volume profile growing right. The
            // shape stays the same at every zoom; numbers are added once rows are tall enough.
            let delta = lv.buy_vol - lv.sell_vol;
            let half_w = cells_w * 0.5;
            let dw = (half_w * (delta.abs() / max_delta.max(1e-9)) as f32).max(1.0);
            painter.rect_filled(
                Rect::from_min_max(pos2(mid - dw, ya), pos2(mid, yb)),
                0.0,
                with_alpha(if delta >= 0.0 { buy_c } else { sell_c }, 170),
            );
            let vw = (half_w * (total / max_total) as f32).max(1.0);
            painter.rect_filled(
                Rect::from_min_max(pos2(mid, ya), pos2(mid + vw, yb)),
                0.0,
                Color32::from_rgba_unmultiplied(140, 152, 175, 120),
            );
            if text {
                let y = (ya + yb) * 0.5;
                painter.text(
                    pos2(mid - 3.0, y),
                    Align2::RIGHT_CENTER,
                    format_size(delta),
                    font.clone(),
                    FOOTPRINT_TEXT,
                );
                painter.text(
                    pos2(mid + 3.0, y),
                    Align2::LEFT_CENTER,
                    format_size(total),
                    font.clone(),
                    FOOTPRINT_TEXT,
                );
            }
        }
        if let Some((price, _)) = poc {
            painter.rect_stroke(
                Rect::from_min_max(
                    pos2(cells_x0, m.sy(price + tick * 0.5)),
                    pos2(cells_x0 + cells_w, m.sy(price - tick * 0.5)),
                ),
                0.0,
                Stroke::new(1.0, Color32::from_rgb(240, 200, 80)),
                StrokeKind::Inside,
            );
        }
        let delta_color = if fp.delta >= 0.0 { buy_c } else { sell_c };
        painter.text(
            pos2(mid, m.sy(fp.low - tick * 0.5) + 3.0),
            Align2::CENTER_TOP,
            format!("Δ{}", format_size(fp.delta)),
            FontId::monospace(10.0),
            delta_color,
        );
    }
    true
}

fn draw_boxes(painter: &Painter, model: &Model, m: &Mapper) {
    for b in &model.overlays.boxes {
        if b.x1 < m.x0 || b.x0 > m.x1 {
            continue;
        }
        let rect = Rect::from_min_max(m.pos(b.x0, b.high), m.pos(b.x1, b.low));
        painter.rect(
            rect,
            0.0,
            with_alpha(b.color, 28),
            Stroke::new(1.0, with_alpha(b.color, 160)),
            StrokeKind::Middle,
        );
        if !b.text.is_empty() {
            painter.text(
                rect.left_top() + egui::vec2(3.0, 2.0),
                Align2::LEFT_TOP,
                &b.text,
                FontId::proportional(11.0),
                b.color,
            );
        }
    }
}

fn draw_overlays(painter: &Painter, model: &Model, m: &Mapper, show_markers: bool) {
    let overlays = &model.overlays;
    let max_points = (m.rect.width() * 4.0) as usize;
    for line in &overlays.lines {
        let i0 = line.points.partition_point(|p| p.0 < m.x0).saturating_sub(1);
        let i1 = (line.points.partition_point(|p| p.0 <= m.x1) + 1).min(line.points.len());
        if i1 <= i0 + 1 {
            continue;
        }
        let step = ((i1 - i0) / max_points.max(1)).max(1);
        let stroke = Stroke::new(1.4, line.color);
        let mut points: Vec<Pos2> = Vec::new();
        let mut last_x = f64::NEG_INFINITY;
        for &(x, v) in line.points[i0..i1].iter().step_by(step) {
            if data::crosses_session(&model.session_x, last_x, x) && points.len() > 1 {
                painter.line(std::mem::take(&mut points), stroke);
            } else if data::crosses_session(&model.session_x, last_x, x) {
                points.clear();
            }
            points.push(m.pos(x, v));
            last_x = x;
        }
        if points.len() > 1 {
            painter.line(points, stroke);
        }
    }
    for seg in &overlays.segments {
        if seg.from.0.max(seg.to.0) < m.x0 || seg.from.0.min(seg.to.0) > m.x1 {
            continue;
        }
        let (a, b) = (m.pos(seg.from.0, seg.from.1), m.pos(seg.to.0, seg.to.1));
        let stroke = Stroke::new(1.4, seg.color);
        if seg.dashed {
            painter.extend(Shape::dashed_line(&[a, b], stroke, 6.0, 4.0));
        } else {
            painter.line_segment([a, b], stroke);
        }
        if !seg.text.is_empty() {
            painter.text(b, Align2::LEFT_BOTTOM, &seg.text, FontId::proportional(11.0), seg.color);
        }
    }
    if !show_markers {
        return;
    }
    let i0 = overlays.markers.partition_point(|mk| mk.x < m.x0);
    let i1 = overlays.markers.partition_point(|mk| mk.x <= m.x1);
    let visible = i1.saturating_sub(i0);
    let step = (visible / 4000).max(1);
    // Shrink markers as they crowd, so a zoomed-out view is not buried under them.
    let radius = match visible {
        0..=200 => 5.0,
        201..=1000 => 3.0,
        _ => 1.5,
    };
    for marker in overlays.markers[i0..i1.max(i0)].iter().step_by(step) {
        let pos = m.pos(marker.x, marker.price);
        draw_marker(painter, pos, &marker.shape, marker.color, radius);
        if visible <= 150 && !marker.text.is_empty() {
            painter.text(
                pos + egui::vec2(7.0, 0.0),
                Align2::LEFT_CENTER,
                &marker.text,
                FontId::proportional(11.0),
                marker.color,
            );
        }
    }
}

fn draw_marker(painter: &Painter, pos: Pos2, shape: &str, color: Color32, r: f32) {
    let polygon = |points: Vec<Pos2>| {
        painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
    };
    match shape {
        "triangle" | "triangle_up" | "up" | "arrow_up" => polygon(vec![
            pos2(pos.x, pos.y - r),
            pos2(pos.x + r, pos.y + r),
            pos2(pos.x - r, pos.y + r),
        ]),
        "triangle_down" | "down" | "arrow_down" => polygon(vec![
            pos2(pos.x, pos.y + r),
            pos2(pos.x - r, pos.y - r),
            pos2(pos.x + r, pos.y - r),
        ]),
        "diamond" => polygon(vec![
            pos2(pos.x, pos.y - r),
            pos2(pos.x + r, pos.y),
            pos2(pos.x, pos.y + r),
            pos2(pos.x - r, pos.y),
        ]),
        "square" => {
            painter.rect_filled(Rect::from_center_size(pos, egui::vec2(r * 1.6, r * 1.6)), 0.0, color);
        }
        "cross" | "x" => {
            let stroke = Stroke::new(1.6, color);
            painter.line_segment([pos + egui::vec2(-r, -r), pos + egui::vec2(r, r)], stroke);
            painter.line_segment([pos + egui::vec2(-r, r), pos + egui::vec2(r, -r)], stroke);
        }
        _ => {
            painter.circle_filled(pos, r * 0.8, color);
        }
    }
}

/// Draw a per-second series (the bid or ask line), averaging cells when several share a pixel
/// and breaking the line at missing data and at session boundaries.
fn draw_grid_series(
    painter: &Painter,
    values: &[f32],
    bounds: &[(usize, usize)],
    m: &Mapper,
    color: Color32,
) {
    let n = values.len();
    let ia = (m.x0.floor().max(0.0) as usize).min(n);
    let ib = ((m.x1.ceil().max(0.0) as usize) + 1).min(n);
    if ib <= ia {
        return;
    }
    let step = (((ib - ia) as f32 / (m.rect.width() * 1.5)) as usize).max(1);
    let stroke = Stroke::new(1.6, color);
    let flush = |points: &mut Vec<Pos2>| {
        match points.len() {
            0 => {}
            1 => {
                painter.circle_filled(points[0], 1.2, color);
            }
            _ => {
                painter.line(std::mem::take(points), stroke);
            }
        }
        points.clear();
    };
    let first = bounds.partition_point(|b| b.1 <= ia);
    let mut points: Vec<Pos2> = Vec::new();
    for &(start, end) in &bounds[first..] {
        if start >= ib {
            break;
        }
        let (a, b) = (start.max(ia), end.min(ib));
        let mut i = a;
        while i < b {
            let j = (i + step).min(b);
            let (mut sum, mut count) = (0f64, 0u32);
            for &v in &values[i..j] {
                if v.is_finite() {
                    sum += v as f64;
                    count += 1;
                }
            }
            if count == 0 {
                flush(&mut points);
            } else {
                points.push(m.pos((i + j) as f64 * 0.5, sum / count as f64));
            }
            i = j;
        }
        flush(&mut points);
    }
}

fn draw_trades(
    painter: &Painter,
    model: &Model,
    settings: &Settings,
    m: &Mapper,
    selected: Option<usize>,
) {
    let (up, down) = if settings.red_up { (RED, GREEN) } else { (GREEN, RED) };
    for (i, trade) in model.result.trades.iter().enumerate() {
        let (entry_x, exit_x) = model.trade_x[i];
        if exit_x < m.x0 || entry_x > m.x1 {
            continue;
        }
        let entry = m.pos(entry_x, trade.entry_px);
        let exit = m.pos(exit_x, trade.exit_px);
        let color = if trade.pnl >= 0.0 { GAIN } else { LOSS };
        let width = if selected == Some(i) { 3.0 } else { 1.5 };
        painter.extend(Shape::dashed_line(&[entry, exit], Stroke::new(width, color), 5.0, 3.0));
        // Buying (long entry, short exit) is an up arrow below the fill price, selling a down
        // arrow above it.
        let scale = if selected == Some(i) { 1.4 } else { 1.0 };
        let long = trade.dir > 0;
        draw_arrow(painter, entry, long, if long { up } else { down }, scale);
        draw_arrow(painter, exit, !long, if long { down } else { up }, scale);
    }
}

/// A filled arrow whose tip touches `tip`: pointing up from below for a buy, down from above for
/// a sell.
fn draw_arrow(painter: &Painter, tip: Pos2, up: bool, color: Color32, scale: f32) {
    let s = if up { 1.0 } else { -1.0 };
    let (head_w, head_h, shaft_w, shaft_h) = (6.0 * scale, 7.0 * scale, 2.0 * scale, 8.0 * scale);
    let y = |dy: f32| tip.y + s * dy;
    let tip = pos2(tip.x, y(2.0));
    painter.add(Shape::convex_polygon(
        vec![tip, pos2(tip.x + head_w, y(2.0 + head_h)), pos2(tip.x - head_w, y(2.0 + head_h))],
        color,
        Stroke::NONE,
    ));
    let (ya, yb) = (y(2.0 + head_h - 0.5), y(2.0 + head_h + shaft_h));
    painter.rect_filled(
        Rect::from_min_max(pos2(tip.x - shaft_w, ya.min(yb)), pos2(tip.x + shaft_w, ya.max(yb))),
        0.0,
        color,
    );
}

fn format_size(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

fn draw_crosshair(axis_painter: &Painter, painter: &Painter, model: &Model, m: &Mapper, pos: Pos2) {
    let plot = m.rect;
    let stroke = Stroke::new(1.0, CROSSHAIR);
    painter.vline(pos.x, plot.y_range(), stroke);
    painter.hline(plot.x_range(), pos.y, stroke);

    let d = model.price_decimals;
    let x = m.x_at(pos.x);
    let price = m.p_at(pos.y);
    let ts = model.xmap.to_ts(x);
    let font = FontId::monospace(11.0);
    let tag_bg = Color32::from_rgb(52, 60, 82);

    let label = |painter: &Painter, at: Pos2, align: Align2, text: String| {
        let galley = painter.layout_no_wrap(text, font.clone(), Color32::WHITE);
        let rect = align.anchor_size(at, galley.size());
        painter.rect_filled(rect.expand(2.0), 2.0, tag_bg);
        painter.galley(rect.min, galley, Color32::WHITE);
    };
    label(
        axis_painter,
        pos2(plot.right() + 4.0, pos.y),
        Align2::LEFT_CENTER,
        format!("{price:.d$}"),
    );
    label(
        axis_painter,
        pos2(pos.x, plot.bottom() + 4.0),
        Align2::CENTER_TOP,
        data::format_ts(ts, model.utc_offset_secs, true, true),
    );

    let mut info = data::format_ts_full(ts, model.utc_offset_secs);
    let i = model.fp_x.partition_point(|f| f.1 < x);
    if let Some(fp) = model.bars().get(i).filter(|_| model.fp_x[i].0 <= x) {
        info.push_str(&format!(
            "   O {:.d$}  H {:.d$}  L {:.d$}  C {:.d$}  V {}  Δ {}",
            fp.open,
            fp.high,
            fp.low,
            fp.close,
            format_size(fp.volume),
            format_size(fp.delta),
        ));
        let tick = model.result.tick_size.abs().max(1e-9);
        if let Some(lv) = fp.ladder.iter().find(|lv| (lv.price - price).abs() <= tick * 0.5) {
            info.push_str(&format!(
                "\n{:.d$}  sell {}  buy {}  Δ {}",
                lv.price,
                format_size(lv.sell_vol),
                format_size(lv.buy_vol),
                format_size(lv.buy_vol - lv.sell_vol),
            ));
        }
    }
    if x >= 0.0 && model.book_visible() {
        let sec = x as usize;
        if let (Some(&bid), Some(&ask)) = (model.grid.bid.get(sec), model.grid.ask.get(sec)) {
            if bid.is_finite() && ask.is_finite() {
                info.push_str(&format!(
                    "\nbid {bid:.d$}  ask {ask:.d$}  spread {:.d$}",
                    ask - bid
                ));
            }
        }
    }
    let galley = painter.layout_no_wrap(info, FontId::monospace(12.0), Color32::from_gray(230));
    let at = plot.left_top() + egui::vec2(8.0, 6.0);
    painter.rect_filled(
        Rect::from_min_size(at, galley.size()).expand(4.0),
        3.0,
        Color32::from_rgba_unmultiplied(8, 12, 24, 215),
    );
    painter.galley(at, galley, Color32::from_gray(230));
}

/// Equity curve on the same x axis as the price panel, broken across closed-market time.
pub(crate) fn equity_panel(ui: &mut egui::Ui, model: &Model, state: &ChartState) {
    let outer = ui.available_rect_before_wrap();
    ui.allocate_rect(outer, Sense::hover());
    let plot = Rect::from_min_max(
        pos2(outer.left(), outer.top() + 2.0),
        pos2(outer.right() - AXIS_W, outer.bottom() - 2.0),
    );
    if plot.width() < 60.0 || plot.height() < 30.0 {
        return;
    }
    let axis_painter = ui.painter_at(outer);
    let painter = ui.painter_at(plot);
    painter.rect_filled(plot, 0.0, BACKGROUND);

    let equity = &model.result.equity;
    let xs = &model.equity_x;
    let i0 = xs.partition_point(|x| *x < state.x0).saturating_sub(1);
    let i1 = (xs.partition_point(|x| *x <= state.x1) + 1).min(xs.len());
    if i1 <= i0 {
        return;
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for point in &equity[i0..i1] {
        lo = lo.min(point.equity);
        hi = hi.max(point.equity);
    }
    let pad = ((hi - lo) * 0.1).max(hi.abs() * 1e-6).max(1e-9);
    let m = Mapper { rect: plot, x0: state.x0, x1: state.x1, p0: lo - pad, p1: hi + pad };
    let step = ((i1 - i0) / (plot.width() as usize * 2).max(1)).max(1);
    let stroke = Stroke::new(1.5, EQUITY_LINE);
    let mut points: Vec<Pos2> = Vec::new();
    let mut last_x = f64::NEG_INFINITY;
    for i in (i0..i1).step_by(step) {
        if data::crosses_session(&model.session_x, last_x, xs[i]) {
            if points.len() > 1 {
                painter.line(std::mem::take(&mut points), stroke);
            }
            points.clear();
        }
        // Equity changes at a point in time and stays flat until the next: draw steps.
        let p = m.pos(xs[i], equity[i].equity);
        if let Some(prev) = points.last().copied() {
            points.push(pos2(p.x, prev.y));
        }
        points.push(p);
        last_x = xs[i];
    }
    if points.len() > 1 {
        painter.line(points, stroke);
    }
    let font = FontId::monospace(11.0);
    painter.text(
        plot.left_top() + egui::vec2(8.0, 4.0),
        Align2::LEFT_TOP,
        "Equity",
        font.clone(),
        AXIS_TEXT,
    );
    let mut labels = vec![(hi, m.sy(hi), Align2::LEFT_TOP)];
    if m.sy(lo) - m.sy(hi) > 14.0 {
        labels.push((lo, m.sy(lo), Align2::LEFT_BOTTOM));
    }
    for (value, y, align) in labels {
        painter.hline(plot.x_range(), y, Stroke::new(1.0, GRID));
        axis_painter.text(
            pos2(plot.right() + 6.0, y),
            align,
            format!("{value:.0}"),
            font.clone(),
            AXIS_TEXT,
        );
    }
}
