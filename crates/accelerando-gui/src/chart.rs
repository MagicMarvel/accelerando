//! Painting and interaction for the price panel and the equity panel.

use eframe::egui::{
    self, pos2, Align2, Color32, ColorImage, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, TextureHandle, TextureOptions,
};

use crate::data::{self, AxisMode, HeatMetric, QuoteColumn, RasterSpec, TradeBubble};
use crate::liquidity_colors::{self, ColorScale};
use crate::market_history::{self, Viewport};
use crate::vp::VpTool;
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
const BID_LINE: Color32 = Color32::from_rgb(0x04, 0xb1, 0xb1);
const ASK_LINE: Color32 = Color32::from_rgb(0xf6, 0x03, 0xc0);
// Bookmap's DEFAULT_COLOR_BUY_TRADES_CIRCLES and DEFAULT_COLOR_SELL_TRADES_CIRCLES.
const BOOKMAP_BUBBLES: [Color32; 2] = [
    Color32::from_rgb(72, 254, 149),
    Color32::from_rgb(255, 87, 70),
];
const CYAN_VIOLET_BUBBLES: [Color32; 2] = [
    Color32::from_rgb(0x42, 0xe9, 0xe4),
    Color32::from_rgb(0xc2, 0x86, 0xff),
];
const QUOTE_WIDTH_PX: f32 = 3.0;
const BUBBLE_HOVER_TOLERANCE_PX: f32 = 5.0;
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
    mode: AxisMode,
    heat: Option<HeatCache>,
}

#[derive(Clone, PartialEq)]
struct HeatKey {
    spec: RasterSpec,
    metric: HeatMetric,
    contrast: f32,
    smooth: bool,
    sigma_rows: f32,
    low_pct: f32,
    high_pct: f32,
    replay_ns: Option<i64>,
}

struct HeatCache {
    key: HeatKey,
    texture: TextureHandle,
}

impl ChartState {
    pub fn new(total: f64, mode: AxisMode) -> Self {
        Self {
            x0: 0.0,
            x1: total.max(1.0),
            p0: 0.0,
            p1: 1.0,
            auto_y: true,
            total: total.max(1.0),
            mode,
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
    pub fn set_total(&mut self, total: f64, mode: AxisMode) {
        self.total = total.max(1.0);
        self.mode = mode;
        self.heat = None;
    }

    /// Show exactly `[x0, x1]` (clamped to the data).
    pub fn show_range(&mut self, x0: f64, x1: f64) {
        self.x0 = x0;
        self.x1 = x1.max(x0 + self.min_span());
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

    fn zoom_price(&mut self, fraction: f64, factor: f64, tick: f64) {
        let span = self.p1 - self.p0;
        let min_span = tick.abs().max(self.p1.abs().max(self.p0.abs()) * f64::EPSILON * 8.0).max(1e-9);
        let span_after = (span * factor).max(min_span);
        let anchor = self.p1 - fraction * span;
        let low = anchor - (1.0 - fraction) * span_after;
        let high = anchor + fraction * span_after;
        if low.is_finite() && high.is_finite() && low < high {
            self.p0 = low;
            self.p1 = high;
            self.auto_y = false;
        }
    }

    fn min_span(&self) -> f64 {
        match self.mode {
            AxisMode::Bars => 5.0,
            AxisMode::Time => (self.total * f64::EPSILON * 4.0).max(1e-9),
        }
    }

    fn clamp_x(&mut self) {
        let min_span = self.min_span();
        let span = (self.x1 - self.x0).clamp(min_span, self.total.max(min_span) * 1.1);
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
pub(crate) struct Mapper {
    pub rect: Rect,
    pub x0: f64,
    pub x1: f64,
    pub p0: f64,
    pub p1: f64,
}

impl Mapper {
    pub fn sx(&self, x: f64) -> f32 {
        self.rect.left() + ((x - self.x0) / (self.x1 - self.x0)) as f32 * self.rect.width()
    }
    pub fn sy(&self, p: f64) -> f32 {
        self.rect.top() + ((self.p1 - p) / (self.p1 - self.p0)) as f32 * self.rect.height()
    }
    fn pos(&self, x: f64, p: f64) -> Pos2 {
        pos2(self.sx(x), self.sy(p))
    }
    pub fn x_at(&self, px: f32) -> f64 {
        self.x0 + ((px - self.rect.left()) / self.rect.width()) as f64 * (self.x1 - self.x0)
    }
    fn p_at(&self, py: f32) -> f64 {
        self.p1 - ((py - self.rect.top()) / self.rect.height()) as f64 * (self.p1 - self.p0)
    }
}

pub(crate) fn price_panel(
    ui: &mut egui::Ui,
    model: &Model,
    settings: &mut Settings,
    state: &mut ChartState,
    selected_trade: Option<usize>,
    vp: &mut VpTool,
    pick_bar: bool,
) -> Option<usize> {
    let outer = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(outer, Sense::click_and_drag());
    let plot = Rect::from_min_max(
        outer.min,
        pos2(outer.right() - AXIS_W, outer.bottom() - AXIS_H),
    );
    if plot.width() < 60.0 || plot.height() < 60.0 {
        return None;
    }

    let before = Mapper { rect: plot, x0: state.x0, x1: state.x1, p0: state.p0, p1: state.p1 };
    let drawing_vp = !pick_bar && vp.handle_input(ui, &response, &before, model);
    handle_input(ui, &response, plot, state, !drawing_vp, model.result.tick_size);
    let book_visible = model.book_visible();
    let show_quotes = book_visible && (settings.show_bbo || (model.has_trade_quotes && settings.show_heat));
    let pixels = f64::from(plot.width());
    let seconds_per_pixel = (state.x1 - state.x0) / pixels;
    let event_time = seconds_per_pixel < 1.0 && (model.tape.is_some() || !model.depth.is_empty());
    let columns = if show_quotes || (book_visible && settings.show_heat && state.auto_y) {
        let bubbles = show_quotes && settings.show_trade_bubbles;
        if event_time {
            let view = Viewport {
                start: state.x0, end: state.x1, pixels, replay_ns: model.replay_clock(),
            };
            let mut columns = market_history::fine_columns(
                &model.axis, model.tape.as_ref(), &model.depth, &view, bubbles,
            );
            if !model.has_trade_quotes && model.depth.is_empty() {
                // Old result files have snapshots but no original L2 updates.
                for col in columns.iter_mut().flatten() {
                    let i = col.x.floor() as usize;
                    col.prices = [model.grid.bid[i] as f64, model.grid.ask[i] as f64];
                }
            }
            columns
        } else {
            let (bid, ask) = model.display_quotes();
            let end = model.completed_seconds().min(bid.len());
            let volume = if bubbles { &model.trade_volume[..] } else { &[] };
            data::quote_columns(
                &model.bounds,
                [&bid[..end], &ask[..end]],
                volume,
                state.x0.floor() as usize..state.x1.ceil() as usize,
                (seconds_per_pixel.ceil() as usize).max(1),
            )
        }
    } else {
        Vec::new()
    };
    if state.auto_y {
        if let Some((lo, hi)) = visible_price_range(model, settings, state.x0, state.x1, &columns) {
            let pad = ((hi - lo) * 0.06).max(model.result.tick_size.abs() * 3.0).max(1e-9);
            state.p0 = lo - pad;
            state.p1 = hi + pad;
        }
    }
    let m = Mapper { rect: plot, x0: state.x0, x1: state.x1, p0: state.p0, p1: state.p1 };

    let axis_painter = ui.painter_at(outer);
    let painter = ui.painter_at(plot);
    painter.rect_filled(plot, 0.0, BACKGROUND);

    if settings.show_heat && model.has_heatmap() && book_visible {
        let heat_painter = model.replay_cut_x().map_or_else(|| painter.clone(), |x| {
            painter.with_clip_rect(Rect::from_min_max(
                plot.min, pos2(m.sx(x).clamp(plot.left(), plot.right()), plot.bottom()),
            ))
        });
        draw_heatmap(ui.ctx(), &heat_painter, model, settings, state, &m);
    }
    draw_axes(&axis_painter, &painter, model, &m);
    // During a replay nothing is drawn to the right of the replay position.
    let cut_px = model.replay_cut_x().map(|x| m.sx(x).clamp(plot.left(), plot.right()) + 1.0);
    let full_painter = painter;
    let painter = match cut_px {
        Some(px) => full_painter
            .with_clip_rect(Rect::from_min_max(plot.min, pos2(px, plot.bottom()))),
        None => full_painter.clone(),
    };
    if settings.show_overlays {
        draw_boxes(&painter, model, &m);
        vp.draw_strategy(&painter, model, &m);
    }
    if settings.show_bars {
        draw_bars(&painter, model, settings, &m);
    }
    if settings.show_overlays {
        draw_overlays(&painter, model, &m, settings.show_markers);
    }
    let hover = response.hover_pos().filter(|p| plot.contains(*p));
    let mut hovered_bubble = None;
    if show_quotes {
        draw_quote_columns(&painter, &m, &columns);
        if settings.show_trade_bubbles {
            let bubble_pointer = hover.filter(|_| !pick_bar && !drawing_vp && !response.dragged());
            hovered_bubble = draw_trade_bubbles(&painter, &m, &columns, settings, bubble_pointer);
        }
        let legend = plot.right_top() + egui::vec2(-8.0, 6.0);
        let painter = &full_painter;
        let ask_label = if model.has_trade_quotes { "est. ask" } else { "ask" };
        let bid_label = if model.has_trade_quotes { "est. bid" } else { "bid" };
        let rect = painter.text(legend, Align2::RIGHT_TOP, ask_label, FontId::monospace(12.0), ASK_LINE);
        painter.text(
            rect.left_top() - egui::vec2(10.0, 0.0),
            Align2::RIGHT_TOP,
            bid_label,
            FontId::monospace(12.0),
            BID_LINE,
        );
    }
    if settings.show_trades {
        draw_trades(&painter, model, settings, &m, selected_trade);
    }
    let pointer_x = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos())
        .map(|p| m.x_at(p.x));
    vp.draw_manual(&painter, model, &m, pointer_x);
    let painter = full_painter;
    if let Some(px) = cut_px {
        painter.vline(px, plot.y_range(), Stroke::new(1.0, REPLAY));
    }
    // Choosing the bar a replay starts from: highlight the bar under the pointer.
    let mut picked = None;
    if let (true, Some(pos)) = (pick_bar, hover) {
        let x = m.x_at(pos.x);
        let i = model.fp_x.partition_point(|f| f.1 < x);
        if let Some(&(a, b)) = model.fp_x.get(i) {
            let (left, right) = (m.sx(a), m.sx(b).max(m.sx(a) + 2.0));
            painter.rect_filled(
                Rect::from_min_max(pos2(left, plot.top()), pos2(right, plot.bottom())),
                0.0,
                with_alpha(REPLAY, 50),
            );
            painter.vline(left, plot.y_range(), Stroke::new(1.0, REPLAY));
            painter.text(
                pos2(left + 6.0, plot.top() + 30.0),
                Align2::LEFT_TOP,
                "replay from here",
                FontId::monospace(12.0),
                REPLAY,
            );
            if response.clicked() {
                picked = Some(i);
            }
        }
    }
    if let Some(pos) = hover {
        draw_crosshair(&axis_painter, &painter, model, &m, pos, event_time.then_some(columns.as_slice()));
    }
    indicator_legend(ui, plot, model, settings, vp);
    if let (Some(pos), Some(dot)) = (hover, hovered_bubble) {
        draw_bubble_tooltip(&painter, model, pos, dot, settings.bubble_delta);
    }
    picked
}

/// Replay position line and picker.
pub(crate) const REPLAY: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);

const LEGEND_BG: Color32 = Color32::from_rgba_premultiplied(6, 9, 18, 200);

/// ATAS-style indicator list in the top-left corner, under the crosshair readout: one row per
/// indicator with a visibility checkbox and, where it has any, a ⚙ settings popup.
fn indicator_legend(
    ui: &mut egui::Ui,
    plot: Rect,
    model: &Model,
    settings: &mut Settings,
    vp: &mut VpTool,
) {
    let rect = Rect::from_min_max(plot.left_top() + egui::vec2(6.0, 28.0), plot.right_bottom());
    let builder = egui::UiBuilder::new()
        .max_rect(rect)
        .layout(egui::Layout::top_down(egui::Align::Min));
    ui.scope_builder(builder, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;

        legend_row(ui, "Overlays (zones, lines)", &mut settings.show_overlays, true, false);

        let markers_on = settings.show_overlays;
        legend_row(ui, "Strategy markers (dots)", &mut settings.show_markers, markers_on, false);

        let (n_strategy, n_drawn) = (model.overlays.profiles.len(), vp.items.len());
        let label = if n_drawn > 0 {
            format!("Volume profile ({n_strategy} strategy, {n_drawn} drawn)")
        } else {
            format!("Volume profile ({n_strategy})")
        };
        if let Some(gear) = legend_row(ui, &label, &mut vp.show_strategy, true, true) {
            egui::Popup::from_toggle_button_response(&gear)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_min_width(260.0);
                    ui.strong("Volume profile");
                    ui.weak("Checkbox hides the strategy profiles; drawn ones stay.");
                    ui.add(
                        egui::Slider::new(&mut vp.smooth_ticks, 0.0..=30.0)
                            .text("drawn: smoothing σ (ticks)"),
                    );
                    ui.add(egui::Slider::new(&mut vp.va_fraction, 0.3..=0.95).text("value area"));
                    ui.add(
                        egui::Slider::new(&mut vp.peak_fraction, 0.0..=0.95)
                            .text("peak area (0 = off)"),
                    )
                    .on_hover_text(
                        "Shade the contiguous levels with at least this share of the POC volume.",
                    );
                    ui.checkbox(&mut vp.extend_lines, "Extend POC / VAH / VAL to the right");
                    if ui
                        .add_enabled(n_drawn > 0, egui::Button::new(format!("Clear drawn ({n_drawn})")))
                        .clicked()
                    {
                        vp.items.clear();
                    }
                });
        }
    });
}

/// One legend row; returns the ⚙ button when `gear` is set.
fn legend_row(
    ui: &mut egui::Ui,
    label: &str,
    visible: &mut bool,
    enabled: bool,
    gear: bool,
) -> Option<egui::Response> {
    egui::Frame::new()
        .fill(LEGEND_BG)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let color = if *visible && enabled {
                    Color32::from_gray(225)
                } else {
                    Color32::from_gray(120)
                };
                ui.add_enabled(enabled, egui::Checkbox::without_text(visible))
                    .on_hover_text("Show / hide");
                ui.label(egui::RichText::new(label).monospace().size(12.0).color(color));
                gear.then(|| {
                    ui.add(egui::Button::new("⚙").frame(false).small())
                        .on_hover_text("Settings")
                })
            })
            .inner
        })
        .inner
}

fn handle_input(
    ui: &egui::Ui,
    response: &egui::Response,
    plot: Rect,
    state: &mut ChartState,
    pan: bool,
    tick: f64,
) {
    let hover = response.hover_pos();
    let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta(), i.zoom_delta()));
    let zoom = if zoom.is_finite() && zoom > 0.0 { f64::from(zoom) } else { 1.0 };
    if let Some(pos) = hover.filter(|p| p.x >= plot.right() && plot.y_range().contains(p.y)) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        if scroll.y != 0.0 || zoom != 1.0 {
            let fraction = f64::from((pos.y - plot.top()) / plot.height());
            let factor = (-f64::from(scroll.y) * 0.0025).exp() / zoom;
            state.zoom_price(fraction, factor, tick);
            return;
        }
    }
    let per_px = (state.x1 - state.x0) / plot.width() as f64;
    if pan && response.dragged() {
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
    if let Some(pos) = hover {
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
        if zoom != 1.0 {
            // Ctrl + wheel / pinch: zoom the price axis around the cursor.
            let fraction = f64::from(((pos.y - plot.top()) / plot.height()).clamp(0.0, 1.0));
            state.zoom_price(fraction, 1.0 / zoom, tick);
        }
    }
    state.clamp_x();
}

/// Low/high of everything price-like that is visible in `[x0, x1]`.
fn visible_price_range(
    model: &Model,
    settings: &Settings,
    x0: f64,
    x1: f64,
    columns: &[Vec<QuoteColumn>],
) -> Option<(f64, f64)> {
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
    for column in columns.iter().flatten() {
        for price in column.prices.into_iter().filter(|price| price.is_finite()) {
            lo = lo.min(price);
            hi = hi.max(price);
        }
        if let Some(bubble) = column.bubble {
            lo = lo.min(bubble.price);
            hi = hi.max(bubble.price);
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
    let mut spec = RasterSpec::new(
        m.x0,
        m.x1,
        m.rect.width(),
        m.p0,
        m.p1,
        m.rect.height(),
        model.result.tick_size,
    );
    if !model.depth.is_empty() {
        spec = spec.with_event_time(m.x0, m.x1, m.rect.width());
    }
    let pixels_per_price = f64::from(m.rect.height()) / (m.p1 - m.p0);
    let sigma_rows = settings.price_filter.sigma(
        model.result.tick_size * pixels_per_price,
        spec.row_p * pixels_per_price,
        settings.price_filter_px,
    );
    let key = HeatKey {
        spec,
        metric: settings.heat_metric,
        contrast: settings.heat_contrast,
        smooth: settings.heat_smooth,
        sigma_rows,
        low_pct: settings.heat_low_pct,
        high_pct: settings.heat_high_pct,
        replay_ns: model.replay_clock(),
    };
    if state.heat.as_ref().is_none_or(|h| h.key != key) {
        let mut cells = if model.depth.is_empty() {
            data::rasterize(&spec, &model.grid, &model.result.liquidity_heatmap.snapshots, settings.heat_metric)
        } else {
            market_history::rasterize_depth(&spec, &model.axis, &model.depth, settings.heat_metric, key.replay_ns)
        };
        let groups = settings.heat_smooth.then(|| {
            liquidity_colors::column_groups(&spec, &model.axis, model.replay_cut_x())
        });
        liquidity_colors::filter_cells(&mut cells, spec.cols, groups.as_deref(), sigma_rows);
        let scale = ColorScale::from_cells(&cells, key.low_pct, key.high_pct, key.contrast);
        let lut: Vec<_> = (0..256).map(|i| {
            let [r, g, b] = liquidity_colors::palette(i as f32 / 255.0);
            Color32::from_rgb(r, g, b)
        }).collect();
        let color = |v: f32| {
            if v > 0.0 { lut[(scale.intensity(v) * 255.0) as usize] } else { Color32::TRANSPARENT }
        };
        // With only the time filter enabled, duplicate each price row so interpolation
        // stays horizontal except for the edge between neighboring price levels.
        let repeat = if key.smooth && sigma_rows < 0.01 {
            ((spec.row_p * pixels_per_price).floor() as usize).clamp(1, (2048 / spec.rows).max(1))
        } else {
            1
        };
        let mut pixels = Vec::with_capacity(spec.cols * spec.rows * repeat);
        for row in cells.chunks_exact(spec.cols) {
            let start = pixels.len();
            pixels.extend(row.iter().map(|&size| color(size)));
            for _ in 1..repeat {
                pixels.extend_from_within(start..start + spec.cols);
            }
        }
        let image = ColorImage::new([spec.cols, spec.rows * repeat], pixels);
        let options = if key.smooth || sigma_rows >= 0.01 {
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
                    data::format_ts(start_ts, model.tz, true, false),
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
        let mut time = data::format_ts(ts, model.tz, span > 6.0 * 3600.0, span < 1800.0);
        if model.xmap.mode() == AxisMode::Time && span < 10.0 {
            time.push_str(&data::fractional_seconds(ts, span));
        }
        axis_painter.text(
            pos2(px, plot.bottom() + 4.0),
            align,
            time,
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
    let over_heat = settings.show_heat && model.has_heatmap() && model.book_visible();

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

fn draw_quote_columns(painter: &Painter, m: &Mapper, segments: &[Vec<QuoteColumn>]) {
    let width = QUOTE_WIDTH_PX / painter.ctx().pixels_per_point();
    for (side, color) in [BID_LINE, ASK_LINE].into_iter().enumerate() {
        let stroke = Stroke::new(width, color);
        for segment in segments {
            for run in segment.split(|column| !column.prices[side].is_finite()) {
                if let [column] = run {
                    painter.circle_filled(m.pos(column.x, column.prices[side]), width * 0.5, color);
                } else if !run.is_empty() {
                    painter.line(
                        run.iter().map(|column| m.pos(column.x, column.prices[side])).collect(),
                        stroke,
                    );
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct PlottedBubble {
    trade: TradeBubble,
    center: Pos2,
    radius: f32,
}

impl PlottedBubble {
    fn hit_distance(self, pointer: Pos2, tolerance: f32) -> Option<f32> {
        let distance = self.center.distance_sq(pointer);
        (distance <= (self.radius + tolerance).powi(2)).then_some(distance)
    }
}

fn draw_trade_bubbles(
    painter: &Painter,
    m: &Mapper,
    segments: &[Vec<QuoteColumn>],
    settings: &Settings,
    pointer: Option<Pos2>,
) -> Option<PlottedBubble> {
    let pixels_per_unit = [
        f64::from(m.rect.width()) / (m.x1 - m.x0),
        f64::from(m.rect.height()) / (m.p1 - m.p0),
    ];
    let options = data::BubbleClustering {
        strength: settings.bubble_aggregation,
        target: data::bubble_size_target(settings.bubble_size),
        spherical: settings.bubble_3d,
        delta: settings.bubble_delta,
    };
    let colors = if settings.bubble_colorblind { CYAN_VIOLET_BUBBLES } else { BOOKMAP_BUBBLES };
    let tolerance = BUBBLE_HOVER_TOLERANCE_PX / painter.ctx().pixels_per_point();
    let pointer = pointer.filter(|&pos| painter.clip_rect().contains(pos));
    let mut hovered: Option<(PlottedBubble, f32)> = None;
    let mut mesh = egui::Mesh::default();
    for segment in segments {
        let bubbles = data::cluster_bubbles(
            segment.iter().filter_map(|column| column.bubble),
            pixels_per_unit,
            m.x0,
            options,
        );
        let largest = bubbles.iter().map(|bubble| bubble.clustering_volume(options.delta)).fold(0.0, f64::max);
        for bubble in bubbles {
            // In Delta mode Bookmap cancels matched buy/sell size before sending the dot
            // to the renderer, while the cluster center still uses total executed volume.
            let volume = bubble.clustering_volume(options.delta);
            if volume < 1.0 {
                continue;
            }
            let radius = data::bubble_radius(volume, largest, options.target, options.spherical);
            let alpha = data::bubble_alpha(radius, settings.bubble_transparency);
            let radius = radius as f32;
            let center = m.pos(bubble.x, bubble.price);
            if alpha == 0 || !painter.clip_rect().expand(radius).contains(center) {
                continue;
            }
            let dot = PlottedBubble { trade: bubble, center, radius };
            if let Some(distance) = pointer.and_then(|pos| dot.hit_distance(pos, tolerance)) {
                if hovered.is_none_or(|(_, best)| distance <= best) {
                    hovered = Some((dot, distance));
                }
            }
            let buy_share = if options.delta {
                u8::from(bubble.buy > bubble.sell) as f32
            } else {
                (bubble.buy / volume) as f32
            };
            if settings.bubble_3d {
                add_sphere(&mut mesh, center, radius, buy_share, alpha, colors);
            } else {
                let color = bubble_color(buy_share, colors);
                let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
                painter.circle_filled(center, radius, fill);
            }
        }
    }
    if !mesh.indices.is_empty() {
        painter.add(Shape::mesh(mesh));
    }
    hovered.map(|(dot, _)| {
        painter.circle_stroke(dot.center, dot.radius, Stroke::new(1.0, Color32::WHITE));
        dot
    })
}

fn draw_bubble_tooltip(painter: &Painter, model: &Model, pointer: Pos2, dot: PlottedBubble, delta: bool) {
    let trade = dot.trade;
    let size = |v: f64| format!("{v:.8}").trim_end_matches('0').trim_end_matches('.').to_owned();
    let mut info = format!(
        "Trade volume  {}\nBuy {}   Sell {}\nPrice {:.digits$}",
        size(trade.total()), size(trade.buy), size(trade.sell), trade.price,
        digits = model.price_decimals,
    );
    if delta {
        info.push_str(&format!("   Δ {}", size(trade.buy - trade.sell)));
    }
    let font = FontId::monospace(12.0);
    let bounds = painter.clip_rect().shrink(4.0);
    let galley = painter.layout(info, font, Color32::WHITE, (bounds.width() - 16.0).max(1.0));
    let padding = egui::vec2(8.0, 6.0);
    let size = galley.size() + padding * 2.0;
    let mut origin = pointer + egui::vec2(14.0, 16.0);
    if origin.x + size.x > bounds.right() {
        origin.x = pointer.x - size.x - 14.0;
    }
    if origin.y + size.y > bounds.bottom() {
        origin.y = pointer.y - size.y - 12.0;
    }
    origin.x = origin.x.clamp(bounds.left(), (bounds.right() - size.x).max(bounds.left()));
    origin.y = origin.y.clamp(bounds.top(), (bounds.bottom() - size.y).max(bounds.top()));
    painter.rect_filled(Rect::from_min_size(origin, size), 4.0, Color32::from_rgb(17, 23, 33));
    painter.galley(origin + padding, galley, Color32::WHITE);
}

fn bubble_color(buy_share: f32, [buy, sell]: [Color32; 2]) -> Color32 {
    let mix = |sell: u8, buy: u8| (f32::from(sell) + (f32::from(buy) - f32::from(sell)) * buy_share).round() as u8;
    Color32::from_rgb(
        mix(sell.r(), buy.r()),
        mix(sell.g(), buy.g()),
        mix(sell.b(), buy.b()),
    )
}

/// Shade each side of a mixed sphere separately using Bookmap's lighting constants.
fn add_sphere(
    mesh: &mut egui::Mesh, center: Pos2, radius: f32, buy_share: f32, opacity: u8,
    [buy, sell]: [Color32; 2],
) {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};

    let buy_arc = TAU * buy_share;
    let buy_start = -FRAC_PI_2 - PI * buy_share;
    for (color, start, arc) in [
        (buy, buy_start, buy_arc),
        (sell, buy_start + buy_arc, TAU - buy_arc),
    ] {
        if arc <= 0.0 {
            continue;
        }
        let sides = ((arc / TAU * 36.0).ceil() as u32).max(1);
        let stride = sides + 1;
        let base = mesh.vertices.len() as u32;
        let shade = |x: f32, y: f32, alpha: u8| {
            let z = (1.0 - x * x - y * y).max(0.0).sqrt();
            let light = (0.6 * x + 0.8 * z).max(0.0).powf(0.7);
            let intensity = (0.02 + 0.98 * light * light).sqrt();
            let channel = |c: u8| (f32::from(c) * intensity).round() as u8;
            Color32::from_rgba_unmultiplied(channel(color.r()), channel(color.g()), channel(color.b()), alpha)
        };
        mesh.colored_vertex(center, shade(0.0, 0.0, opacity));
        for (ring, r) in [0.35, 0.65, 0.86, 1.0, 1.0 + 0.75 / radius].into_iter().enumerate() {
            let ring_start = base + 1 + ring as u32 * stride;
            let alpha = if ring < 4 { opacity } else { 0 };
            for i in 0..=sides {
                let angle = start + arc * i as f32 / sides as f32;
                let (y, x) = angle.sin_cos();
                mesh.colored_vertex(center + egui::vec2(x, y) * (r * radius), shade(x * r, y * r, alpha));
                if i == sides {
                    break;
                }
                if ring == 0 {
                    mesh.add_triangle(base, ring_start + i, ring_start + i + 1);
                } else {
                    let previous = ring_start - stride;
                    mesh.add_triangle(previous + i, ring_start + i, ring_start + i + 1);
                    mesh.add_triangle(previous + i, ring_start + i + 1, previous + i + 1);
                }
            }
        }
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
    // During a replay a trade shows up at its entry and its outcome only at its exit.
    let now = model.replay_clock().unwrap_or(i64::MAX);
    for (i, trade) in model.result.trades.iter().enumerate() {
        let (entry_x, exit_x) = model.trade_x[i];
        if exit_x < m.x0 || entry_x > m.x1 || trade.entry_ts_ns > now {
            continue;
        }
        let entry = m.pos(entry_x, trade.entry_px);
        let scale = if selected == Some(i) { 1.4 } else { 1.0 };
        let long = trade.dir > 0;
        if trade.exit_ts_ns > now {
            draw_arrow(painter, entry, long, if long { up } else { down }, scale);
            continue;
        }
        let exit = m.pos(exit_x, trade.exit_px);
        let color = if trade.pnl >= 0.0 { GAIN } else { LOSS };
        let width = if selected == Some(i) { 3.0 } else { 1.5 };
        painter.extend(Shape::dashed_line(&[entry, exit], Stroke::new(width, color), 5.0, 3.0));
        // Buying (long entry, short exit) is an up arrow below the fill price, selling a down
        // arrow above it.
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

fn draw_crosshair(
    axis_painter: &Painter, painter: &Painter, model: &Model, m: &Mapper, pos: Pos2,
    fine: Option<&[Vec<QuoteColumn>]>,
) {
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
    let mut time = data::format_ts(ts, model.tz, true, true);
    if model.xmap.mode() == AxisMode::Time && m.x1 - m.x0 < 10.0 {
        time.push_str(&data::fractional_seconds(ts, m.x1 - m.x0));
    }
    label(axis_painter, pos2(pos.x, plot.bottom() + 4.0), Align2::CENTER_TOP, time);

    let mut info = data::format_ts_full(ts, model.tz);
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
                "   @{:.d$}  sell {}  buy {}  Δ {}",
                lv.price,
                format_size(lv.sell_vol),
                format_size(lv.buy_vol),
                format_size(lv.buy_vol - lv.sell_vol),
            ));
        }
    }
    if let Some(columns) = fine {
        if model.replay_clock().is_none_or(|clock| ts <= clock) {
            if let Some(column) = columns.iter().flatten()
                .min_by(|a, b| (a.x - x).abs().total_cmp(&(b.x - x).abs())) {
                let [bid, ask] = if model.depth.is_empty() { column.prices } else { market_history::book_quote(&model.depth, ts) };
                let prefix = if model.has_trade_quotes { "est. " } else { "" };
                if bid.is_finite() { info.push_str(&format!("   {prefix}bid {bid:.d$}")); }
                if ask.is_finite() { info.push_str(&format!("   {prefix}ask {ask:.d$}")); }
                if let Some(bubble) = column.bubble {
                    info.push_str(&format!("   buy {}  sell {}", format_size(bubble.buy), format_size(bubble.sell)));
                }
            }
        }
    } else if x >= 0.0 && model.book_visible() {
        let sec = x as usize;
        let quote_end = if model.has_trade_quotes { model.completed_seconds() } else { model.grid.bid.len() };
        let (bids, asks) = (&model.grid.bid[..quote_end], &model.grid.ask[..quote_end]);
        if let (Some(&bid), Some(&ask)) = (bids.get(sec), asks.get(sec)) {
            let prefix = if model.has_trade_quotes { "est. " } else { "" };
            if bid.is_finite() {
                info.push_str(&format!("   {prefix}bid {bid:.d$}"));
            }
            if ask.is_finite() {
                info.push_str(&format!("   {prefix}ask {ask:.d$}"));
            }
            if !model.has_trade_quotes && bid.is_finite() && ask.is_finite() {
                info.push_str(&format!("   spread {:.d$}", ask - bid));
            }
        }
        if sec < model.completed_seconds() {
            if let Some(vol) = model.trade_volume.get(sec) {
                info.push_str(&format!("   1s buy {}  sell {}", format_size(vol.buy as f64), format_size(vol.sell as f64)));
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
    let mut i1 = (xs.partition_point(|x| *x <= state.x1) + 1).min(xs.len());
    if let Some(now) = model.replay_clock() {
        // During a replay the curve stops at the replay position.
        i1 = i1.min(equity.partition_point(|e| e.ts_ns <= now));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_axis_wheel_zooms_price_at_pointer_and_plot_wheel_zooms_time() {
        let ctx = egui::Context::default();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(700.0, 500.0));
        let plot = Rect::from_min_max(outer.min, pos2(600.0, 400.0));
        let mut state = ChartState::new(300.0, AxisMode::Time);
        state.p0 = 90.0;
        state.p1 = 110.0;
        let mut frame = |events| {
            let input = egui::RawInput { screen_rect: Some(outer), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                let response = ui.allocate_rect(outer, Sense::click_and_drag());
                handle_input(ui, &response, plot, &mut state, true, 0.25);
            });
            output.textures_delta.clear();
            (state.x0, state.x1, state.p0, state.p1, state.auto_y)
        };
        let wheel = || egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 120.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(pos2(630.0, 100.0))]);
        let (x0, x1, low, high, auto_y) = frame(vec![wheel()]);
        assert_eq!((x0, x1), (0.0, 300.0));
        assert!(!auto_y);
        assert!(high - low < 20.0);
        assert!((high - 0.25 * (high - low) - 105.0).abs() < 1e-9);

        frame(vec![egui::Event::PointerMoved(pos2(300.0, 100.0))]);
        let (left, right, new_low, new_high, _) = frame(vec![wheel()]);
        assert!(right - left < x1 - x0);
        assert_eq!((new_low, new_high), (low, high));
    }

    #[test]
    fn hover_uses_the_rendered_bubble_radius() {
        let dot = PlottedBubble {
            trade: TradeBubble { x: 1.0, price: 7600.25, buy: 200.0, sell: 20.0 },
            center: pos2(40.0, 40.0),
            radius: 10.0,
        };
        assert!(dot.hit_distance(pos2(52.0, 40.0), 5.0).is_some());
        assert!(dot.hit_distance(pos2(54.0, 54.0), 5.0).is_none());
    }

    #[test]
    fn sphere_bubbles_have_valid_shaded_geometry() {
        assert_eq!(bubble_color(0.0, BOOKMAP_BUBBLES), BOOKMAP_BUBBLES[1]);
        assert_eq!(bubble_color(1.0, BOOKMAP_BUBBLES), BOOKMAP_BUBBLES[0]);

        let mut mesh = egui::Mesh::default();
        add_sphere(&mut mesh, pos2(10.0, 20.0), 8.0, 0.5, 191, BOOKMAP_BUBBLES);
        add_sphere(&mut mesh, pos2(40.0, 60.0), 4.0, 1.0, 191, CYAN_VIOLET_BUBBLES);
        assert!(mesh.is_valid());
        assert!(mesh.vertices.iter().any(|vertex| vertex.color.a() == 0));
        let center = mesh.vertices[0].color;
        assert!(mesh.vertices.iter().any(|vertex| vertex.color.a() == center.a() && vertex.color != center));
    }
}
