//! Painting and interaction for the price panel and the equity panel.

use std::sync::{mpsc, Arc};

use accelerando_core::{volume_profile, DepthStore, DepthWindow, Footprint, LiquidityLevel, VpLevel};
use eframe::egui::{
    self, pos2, Align2, Color32, ColorImage, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, TextureHandle, TextureOptions,
};

use crate::data::{self, AxisMode, HeatMetric, QuoteColumn, RasterSpec, TradeBubble};
use crate::big_trades::{BigTradeSettings, Marker, MarkerShape};
use crate::heatmap_studies::{self, FlowMarker, FlowQuery, Studies, Study};
use crate::liquidity_colors::{self, ColorScale};
use crate::market_history::{self, Viewport};
use crate::replay_bubbles::{BubbleGroup, BubbleView, ReplayBubbles};
use crate::vp::VpTool;
use crate::{ChartMode, Model, Settings};

const AXIS_W: f32 = 72.0;
const AXIS_H: f32 = 22.0;
const REPLAY_DEPTH_W: f32 = 132.0;
const REPLAY_FRACTION: f64 = 0.85;

const BACKGROUND: Color32 = Color32::from_rgb(8, 12, 24);
const GRID: Color32 = Color32::from_rgba_premultiplied(40, 46, 60, 110);
const AXIS_TEXT: Color32 = Color32::from_rgb(150, 158, 175);
const SESSION_LINE: Color32 = Color32::from_rgba_premultiplied(70, 78, 100, 160);
const CROSSHAIR: Color32 = Color32::from_rgba_premultiplied(150, 150, 150, 150);
const GREEN: Color32 = Color32::from_rgb(0x26, 0xa6, 0x9a);
const RED: Color32 = Color32::from_rgb(0xef, 0x53, 0x50);
const BID_LINE: Color32 = Color32::from_rgb(0x2e, 0xcc, 0x71);
const ASK_LINE: Color32 = Color32::from_rgb(0xe7, 0x4c, 0x3c);
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
// Match the falloff measured across flat quote lines in Bookmap screenshots.
const QUOTE_FEATHER_PX: f32 = 1.6;
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
    pub follow_replay: bool,
    total: f64,
    mode: AxisMode,
    heat: Option<HeatCache>,
    heat_worker: Option<HeatWorker>,
    heat_preview: Option<HeatPreview>,
    flow_heat: Option<FlowHeatCache>,
    flow_worker: Option<FlowHeatWorker>,
    replay_bubbles: ReplayBubbles,
}

#[derive(Clone, PartialEq)]
struct HeatKey {
    spec: RasterSpec,
    metric: HeatMetric,
    studies: Studies,
    contrast: f32,
    smooth: bool,
    sigma_rows: f32,
    low_pct: f32,
    high_pct: f32,
    replay_ns: Option<i64>,
    /// Exact depth window drawn, if any (`None` = overview or in-memory history).
    depth_window: Option<(i64, i64)>,
}

struct HeatCache {
    key: HeatKey,
    texture: TextureHandle,
    depth_cells: Option<DepthRaster>,
    scale: ColorScale,
    replay_texture: Option<ReplayHeatTexture>,
}

struct HeatPreview {
    key: HeatKey,
    detailed_scale: Option<ColorScale>,
    repeat: usize,
    texture: TextureHandle,
}

struct HeatJob {
    key: HeatKey,
    axis: data::TimeAxis,
    replay_x: Option<f64>,
    store: DepthStore,
    window: Option<Arc<DepthWindow>>,
    repeat: usize,
    color_rows: std::ops::Range<usize>,
}

struct HeatWorker {
    jobs: mpsc::Sender<HeatJob>,
    frames: mpsc::Receiver<(HeatKey, HeatImage)>,
    pending: Option<HeatKey>,
}

impl HeatWorker {
    fn spawn(ctx: &egui::Context) -> Self {
        let (jobs, receive) = mpsc::channel::<HeatJob>();
        let (send, frames) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for job in receive {
                let depth = job.window.as_ref().map_or_else(|| job.store.overview(), |window| &window.history);
                let raster = DepthRaster::build(&job.key, &job.axis, depth, None);
                let groups = job.key.smooth.then(|| {
                    liquidity_colors::column_groups(&job.key.spec, &job.axis, job.replay_x)
                });
                let image = HeatImage::build(
                    &job.key, Some(raster), Vec::new(), groups, job.repeat, job.color_rows,
                );
                if send.send((job.key, image)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self { jobs, frames, pending: None }
    }
}

struct HeatImage {
    image: ColorImage,
    depth_cells: Option<DepthRaster>,
    scale: ColorScale,
    replay_texture: Option<ReplayHeatTexture>,
}

impl HeatImage {
    fn build(
        key: &HeatKey,
        depth_cells: Option<DepthRaster>,
        snapshot_cells: Vec<f32>,
        groups: Option<Vec<Option<usize>>>,
        repeat: usize,
        color_rows: std::ops::Range<usize>,
    ) -> Self {
        let spec = &key.spec;
        let mut cells = depth_cells.as_ref().map_or(snapshot_cells, |depth| depth.cells.clone());
        if let Some(groups) = &groups {
            liquidity_colors::smooth_time(&mut cells, spec.cols, groups);
        }
        let (pixels, remaining_repeat) = liquidity_colors::price_pixels(&cells, spec.cols, repeat, key.sigma_rows);
        let stride = spec.cols * (repeat / remaining_repeat);
        let color_cells = color_rows.start * stride..color_rows.end * stride;
        let scale = ColorScale::from_cells(&pixels[color_cells], key.low_pct, key.high_pct, key.contrast);
        let replay_texture = (key.replay_ns.is_some() && depth_cells.is_some()).then(|| ReplayHeatTexture {
            first_column: spec.c_lo - 1,
            // Two guard columns plus space for the viewport to gain a column as it pans.
            width: spec.cols + 3,
            repeat,
            has_colors: cells.iter().any(|&size| size.is_finite() && size > 0.0),
            groups,
        });
        let image = heat_image(&pixels, spec.cols, remaining_repeat, &scale, replay_texture.as_ref().map(|ring| ring.width));
        Self { image, depth_cells, scale, replay_texture }
    }

    fn upload(self, ctx: &egui::Context, key: HeatKey, old: Option<HeatCache>) -> HeatCache {
        let options = key.texture_options(self.replay_texture.is_some());
        let texture = if let Some(mut cache) = old {
            cache.texture.set(self.image, options);
            cache.texture
        } else {
            ctx.load_texture("liquidity-heatmap", self.image, options)
        };
        HeatCache { key, texture, depth_cells: self.depth_cells, scale: self.scale, replay_texture: self.replay_texture }
    }
}

// Texture slots are tied to absolute raster columns, so following the clock moves
// the display rectangle without moving the historical pixels in GPU memory.
struct ReplayHeatTexture {
    first_column: i64,
    width: usize,
    repeat: usize,
    has_colors: bool,
    groups: Option<Vec<Option<usize>>>,
}

impl ReplayHeatTexture {
    fn slot(&self, column: i64) -> usize {
        (column - self.first_column).rem_euclid(self.width as i64) as usize
    }
}

fn same_heat_column(
    col: i64,
    key: &HeatKey,
    times: &[Option<(i64, i64)>],
    groups: Option<&[Option<usize>]>,
    old: &HeatCache,
) -> bool {
    let old_col = key.spec.c_lo + col - old.key.spec.c_lo;
    if !(-1..=old.key.spec.cols as i64).contains(&old_col) {
        return false;
    }
    // Duplicate the edge columns in two guard slots for linear sampling.
    let col = col.clamp(0, key.spec.cols as i64 - 1) as usize;
    let old_col = old_col.clamp(0, old.key.spec.cols as i64 - 1) as usize;
    if key.spec.c_lo + col as i64 != old.key.spec.c_lo + old_col as i64 {
        return false;
    }
    let old_times = &old.depth_cells.as_ref().unwrap().times;
    let old_groups = old.replay_texture.as_ref().unwrap().groups.as_deref();
    let signature = |times: &[Option<(i64, i64)>], groups: Option<&[Option<usize>]>, col| {
        let neighbors = groups.map_or([None; 2], |groups| liquidity_colors::time_neighbors(groups, col));
        (times[col], neighbors.map(|i| i.map(|i| times[i])))
    };
    signature(times, groups, col) == signature(old_times, old_groups, old_col)
}

fn heat_image(cells: &[f32], cols: usize, repeat: usize, scale: &ColorScale, ring_width: Option<usize>) -> ColorImage {
    static PALETTE: std::sync::OnceLock<[Color32; 256]> = std::sync::OnceLock::new();
    let lut = PALETTE.get_or_init(|| {
        std::array::from_fn(|i| {
            let [r, g, b] = liquidity_colors::palette(i as f32 / 255.0);
            Color32::from_rgb(r, g, b)
        })
    });
    let bins = (cells.len() >= 32_768).then(|| scale.palette_bins());
    let color = |v: f32| {
        if !v.is_finite() || v <= 0.0 {
            return Color32::TRANSPARENT;
        }
        let index = bins.as_ref().map_or_else(
            || (scale.intensity(v) * 255.0) as usize,
            |bins| bins.index(v),
        );
        lut[index]
    };
    let width = ring_width.unwrap_or(cols);
    let mut pixels = Vec::with_capacity(width * (cells.len() / cols) * repeat);
    for row in cells.chunks_exact(cols) {
        let start = pixels.len();
        if ring_width.is_some() {
            pixels.push(color(row[0]));
        }
        pixels.extend(row.iter().copied().map(color));
        if ring_width.is_some() {
            pixels.push(color(row[cols - 1]));
            pixels.resize(start + width, Color32::TRANSPARENT);
        }
        for _ in 1..repeat {
            pixels.extend_from_within(start..start + width);
        }
    }
    ColorImage::new([width, cells.len() / cols * repeat], pixels)
}

fn update_replay_heat(
    cache: &mut HeatCache,
    key: &HeatKey,
    depth: &DepthRaster,
    groups: Option<&[Option<usize>]>,
    options: TextureOptions,
) {
    let ring = cache.replay_texture.as_ref().unwrap();
    let spec = &key.spec;
    let dirty: Vec<_> = (-1..=spec.cols as i64)
        .map(|col| !same_heat_column(col, key, &depth.times, groups, cache))
        .collect();
    let mut next = 0;
    while let Some(offset) = dirty[next..].iter().position(|&dirty| dirty) {
        let start = next + offset;
        let column = spec.c_lo + start as i64 - 1;
        let slot = ring.slot(column);
        let limit = dirty.len().min(start + ring.width - slot);
        let count = dirty[start..limit].iter().take_while(|&&dirty| dirty).count();
        let columns: Vec<_> = (start..start + count).map(|i| {
            let col = i.saturating_sub(1).min(spec.cols - 1);
            let neighbors = groups.map_or([None; 2], |groups| liquidity_colors::time_neighbors(groups, col));
            (col, neighbors)
        }).collect();
        let mut cells = Vec::with_capacity(count * spec.rows);
        for row in depth.cells.chunks_exact(spec.cols) {
            cells.extend(columns.iter().map(|&(col, neighbors)| {
                liquidity_colors::time_filtered_cell(row, col, neighbors)
            }));
        }
        let (pixels, repeat) = liquidity_colors::price_pixels(&cells, count, ring.repeat, key.sigma_rows);
        let image = heat_image(&pixels, count, repeat, &cache.scale, None);
        cache.texture.set_partial([slot, 0], image, options);
        next = start + count;
    }
}

struct DepthRaster {
    times: Vec<Option<(i64, i64)>>,
    cells: Vec<f32>,
}

impl HeatKey {
    fn same_appearance(&self, other: &Self) -> bool {
        self.metric == other.metric && self.studies == other.studies && self.smooth == other.smooth
            && self.contrast == other.contrast && self.low_pct == other.low_pct && self.high_pct == other.high_pct
    }

    fn can_preview(&self, previous: &Self) -> bool {
        self.same_appearance(previous) && match (self.replay_ns, previous.replay_ns) {
            (Some(now), Some(then)) => now >= then,
            (None, None) => true,
            _ => false,
        }
    }

    fn overlap_columns(&self, other: &Self) -> usize {
        let (a, b) = (&self.spec, &other.spec);
        let first = a.c_lo.max(b.c_lo);
        let end = (a.c_lo + a.cols as i64).min(b.c_lo + b.cols as i64);
        (end - first).max(0) as usize
    }

    fn accepts(&self, ready: &Self) -> bool {
        self == ready || (self.replay_ns.zip(ready.replay_ns).is_some_and(|(now, then)| now > then)
            && self.can_preview(ready)
            && self.same_depth_grid(ready) && self.sigma_rows == ready.sigma_rows
            && self.overlap_columns(ready) > 0)
    }

    fn texture_options(&self, replaying_depth: bool) -> TextureOptions {
        // Price smoothing has already been applied to the cell values.
        match (replaying_depth, self.smooth) {
            (true, true) => TextureOptions::LINEAR_REPEAT,
            (true, false) => TextureOptions::NEAREST_REPEAT,
            (false, true) => TextureOptions::LINEAR,
            (false, false) => TextureOptions::NEAREST,
        }
    }

    fn row_repeat(&self, pixels_per_price: f64) -> usize {
        if self.smooth || self.sigma_rows > 0.0 {
            price_row_repeat(&self.spec, pixels_per_price)
        } else {
            1
        }
    }

    fn same_depth_grid(&self, other: &Self) -> bool {
        self.spec.col_w == other.spec.col_w
            && self.spec.row_p == other.spec.row_p
            && self.spec.k_hi == other.spec.k_hi
            && self.spec.rows == other.spec.rows
            && self.metric == other.metric
            && self.studies == other.studies
            && self.depth_window == other.depth_window
    }
}

impl DepthRaster {
    fn build(
        key: &HeatKey,
        axis: &data::TimeAxis,
        depth: &accelerando_core::DepthHistory,
        previous: Option<(&HeatKey, &Self)>,
    ) -> Self {
        let spec = &key.spec;
        let times = market_history::raster_times(spec, axis, key.replay_ns);
        let mut missing = times.clone();
        let mut runs = Vec::<(usize, usize, usize)>::new();
        let previous = previous.filter(|(old_key, _)| key.same_depth_grid(old_key));
        if let Some((old_key, old)) = previous {
            let shift = spec.c_lo - old_key.spec.c_lo;
            // A column at the replay edge can occupy the same slot with a new
            // end time. Match both endpoints to keep that column causal.
            for (col, time) in missing.iter_mut().enumerate() {
                let old_col = col as i64 + shift;
                if time.is_none() || old.times.get(old_col as usize).copied() != Some(*time) {
                    continue;
                }
                *time = None;
                if let Some(run) = runs.last_mut().filter(|run| run.0 + run.2 == col) {
                    run.2 += 1;
                } else {
                    runs.push((col, old_col as usize, 1));
                }
            }
        }
        let mut cells = if missing.iter().all(Option::is_none) {
            vec![0.0; spec.cols * spec.rows]
        } else if key.studies.active() {
            heatmap_studies::rasterize_resting(spec, &missing, depth, key.metric, key.studies)
        } else {
            market_history::rasterize_depth(spec, &missing, depth, key.metric)
        };
        if let Some((old_key, old)) = previous {
            for (row, old_row) in cells
                .chunks_exact_mut(spec.cols)
                .zip(old.cells.chunks_exact(old_key.spec.cols))
            {
                for &(col, old_col, len) in &runs {
                    row[col..col + len].copy_from_slice(&old_row[old_col..old_col + len]);
                }
            }
        }
        Self { times, cells }
    }
}

struct FlowHeatCache {
    key: FlowHeatKey,
    markers: Vec<FlowMarker>,
    net: Vec<f32>,
    texture: Option<TextureHandle>,
}

#[derive(Clone, PartialEq)]
struct FlowHeatKey {
    heat: HeatKey,
    visible: (i64, i64),
    prices: (f64, f64),
    markers: [Option<u32>; 3],
    marker_limit: usize,
    show_net: bool,
}

impl FlowHeatKey {
    fn can_preview(&self, old: &Self) -> bool {
        self.heat.can_preview(&old.heat)
            && self.markers == old.markers && self.marker_limit == old.marker_limit
            && self.show_net == old.show_net
            && self.visible.0 < old.visible.1 && old.visible.0 < self.visible.1
            && self.prices.0 <= old.prices.1 && old.prices.0 <= self.prices.1
    }
}

struct FlowHeatJob {
    key: FlowHeatKey,
    query: FlowQuery,
    trades_from: i64,
    axis: data::TimeAxis,
    store: DepthStore,
    window: Option<Arc<DepthWindow>>,
    tape: Option<market_history::TradeTape>,
}

struct FlowHeatImage {
    key: FlowHeatKey,
    layers: heatmap_studies::FlowLayers,
    image: Option<ColorImage>,
}

struct FlowHeatWorker {
    jobs: mpsc::Sender<FlowHeatJob>,
    frames: mpsc::Receiver<FlowHeatImage>,
    pending: bool,
}

impl FlowHeatWorker {
    fn spawn(ctx: &egui::Context) -> Self {
        let (jobs, receive) = mpsc::channel::<FlowHeatJob>();
        let (send, frames) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for job in receive {
                let depth = job.window.as_ref().map_or_else(|| job.store.overview(), |window| &window.history);
                let spec = &job.key.heat.spec;
                let times = job.key.show_net.then(|| {
                    market_history::raster_times(spec, &job.axis, job.key.heat.replay_ns)
                });
                let trades = job.tape.iter().flat_map(|tape| {
                    tape.trades_between(job.trades_from, job.query.read_until)
                });
                let layers = heatmap_studies::flow_layers(
                    depth, &job.query, times.as_deref().map(|times| (spec, times)), trades,
                );
                let image = job.key.show_net.then(|| net_heat_image(&job.key.heat, &layers.net));
                if send.send(FlowHeatImage { key: job.key, layers, image }).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self { jobs, frames, pending: false }
    }

    fn poll(&mut self, key: &FlowHeatKey) -> Option<FlowHeatImage> {
        match self.frames.try_recv() {
            Ok(frame) => {
                self.pending = false;
                key.can_preview(&frame.key).then_some(frame)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = false;
                None
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

impl ChartState {
    pub fn new(total: f64, mode: AxisMode) -> Self {
        Self {
            x0: 0.0,
            x1: total.max(1.0),
            p0: 0.0,
            p1: 1.0,
            auto_y: true,
            follow_replay: false,
            total: total.max(1.0),
            mode,
            heat: None,
            heat_worker: None,
            heat_preview: None,
            flow_heat: None,
            flow_worker: None,
            replay_bubbles: ReplayBubbles::default(),
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
        self.heat_worker = None;
        self.heat_preview = None;
        self.flow_heat = None;
        self.flow_worker = None;
        self.replay_bubbles.reset();
    }

    pub fn follow_new_replay(&mut self) {
        self.follow_replay = true;
        self.heat = None;
        self.heat_worker = None;
        self.heat_preview = None;
        self.flow_heat = None;
        self.flow_worker = None;
        self.replay_bubbles.reset();
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
        self.follow_replay = false;
        self.clamp_x();
    }

    pub fn sync_replay(&mut self, replay_x: Option<f64>) {
        if let Some(x) = replay_x.filter(|_| self.follow_replay) {
            self.place_replay(x, self.x1 - self.x0);
        }
    }

    fn place_replay(&mut self, x: f64, span: f64) {
        self.x0 = x - REPLAY_FRACTION * span;
        self.x1 = x + (1.0 - REPLAY_FRACTION) * span;
    }

    fn zoom_time(&mut self, fraction: f64, factor: f64, replay_x: Option<f64>) {
        let span = self.x1 - self.x0;
        let next_span = self.clamp_span(span * factor);
        if let Some(x) = replay_x.filter(|_| self.follow_replay) {
            self.place_replay(x, next_span);
        } else {
            let anchor = self.x0 + fraction * span;
            self.x0 = anchor - fraction * next_span;
            self.x1 = self.x0 + next_span;
            self.clamp_x_with_replay_margin(replay_x.is_some());
        }
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
        self.clamp_x_with_replay_margin(false);
    }

    fn clamp_span(&self, span: f64) -> f64 {
        let min_span = self.min_span();
        span.clamp(min_span, self.total.max(min_span) * 1.1)
    }

    fn clamp_x_with_replay_margin(&mut self, replaying: bool) {
        let span = self.clamp_span(self.x1 - self.x0);
        let center = (self.x0 + self.x1) * 0.5;
        let (left, right) = if replaying {
            (-REPLAY_FRACTION * span, self.total + (1.0 - REPLAY_FRACTION) * span)
        } else {
            (-0.1 * span, self.total + 0.1 * span)
        };
        self.x0 = (center - span * 0.5).max(left).min(right - span);
        self.x1 = self.x0 + span;
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

fn replay_depth_width(model: &Model, width: f32) -> f32 {
    if model.replay_clock().is_some() && model.has_heatmap() {
        REPLAY_DEPTH_W.min(width * 0.25)
    } else {
        0.0
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
) -> egui::InnerResponse<Option<usize>> {
    let outer = ui.available_rect_before_wrap();
    let area = ui.allocate_rect(outer, Sense::hover());
    let replay_clock = model.replay_clock();
    let depth_width = replay_depth_width(model, outer.width());
    let price_axis_left = outer.right() - AXIS_W;
    let plot = Rect::from_min_max(
        outer.min,
        pos2(price_axis_left - depth_width, outer.bottom() - AXIS_H),
    );
    if plot.width() < 60.0 || plot.height() < 60.0 {
        return egui::InnerResponse::new(None, area);
    }

    let response = ui.interact(plot, area.id.with("plot"), Sense::click_and_drag());
    let price_axis = Rect::from_min_max(pos2(price_axis_left, plot.top()), pos2(outer.right(), plot.bottom()));
    let time_axis = Rect::from_min_max(plot.left_bottom(), pos2(plot.right(), outer.bottom()));
    let price_axis = ui.interact(price_axis, area.id.with("price axis"), Sense::drag());
    let time_axis = ui.interact(time_axis, area.id.with("time axis"), Sense::drag());
    let replay_x = model.replay_cut_x();

    let before = Mapper { rect: plot, x0: state.x0, x1: state.x1, p0: state.p0, p1: state.p1 };
    let drawing_vp = !pick_bar && vp.handle_input(ui, &response, &before, model);
    handle_input(&response, state, !drawing_vp, model.result.tick_size, replay_x);
    handle_axis_input(&price_axis, &time_axis, plot, state, model.result.tick_size, replay_x);
    let book_visible = model.book_visible();
    let show_quotes = book_visible && settings.show_bbo;
    let show_bubbles = book_visible && settings.show_trade_bubbles;
    let heat_visible = book_visible && settings.mode == ChartMode::Heatmap && settings.show_heat;
    let pixels = f64::from(plot.width());
    let seconds_per_pixel = (state.x1 - state.x0) / pixels;
    let replay_bubble_source = model.replay_clock().zip(model.tape.as_ref()).filter(|_| show_bubbles);
    let hover = response.hover_pos().filter(|p| plot.contains(*p));
    let event_time = seconds_per_pixel < 1.0 && (model.tape.is_some() || !model.depth.is_empty());
    let aggregate_bubbles = show_bubbles && (replay_bubble_source.is_none() || state.auto_y);
    let query_hover_trade = event_time && replay_bubble_source.is_some() && !aggregate_bubbles;
    let columns = if show_quotes || aggregate_bubbles || (heat_visible && state.auto_y)
        || (query_hover_trade && hover.is_some())
    {
        if event_time {
            let view = Viewport {
                start: state.x0, end: state.x1, pixels, replay_ns: model.replay_clock(),
            };
            let mut columns = market_history::fine_columns(
                &model.axis, model.tape.as_ref(), &model.depth, &view,
                aggregate_bubbles.then_some(settings.bubble_min_trade_size),
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
            let volume = if aggregate_bubbles {
                model.filtered_bubble_volume.as_deref().unwrap_or(&model.trade_volume)
            } else {
                &[]
            };
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

    let heat_edge = replay_x.map(|x| m.sx(x)).filter(|x| plot.x_range().contains(*x) && *x < plot.right());
    if heat_visible && model.has_heatmap() {
        let heat_painter = replay_x.map_or_else(|| painter.clone(), |x| {
            painter.with_clip_rect(Rect::from_min_max(
                plot.min, pos2(m.sx(x).clamp(plot.left(), plot.right()), plot.bottom()),
            ))
        });
        draw_heatmap(ui.ctx(), &heat_painter, model, settings, state, &m);
    }
    let edge = heat_edge.filter(|_| heat_visible).zip(state.heat.as_ref());
    let edge_spec = edge.map(|_| {
        RasterSpec::new(m.x0, m.x1, m.rect.width(), m.p0, m.p1, m.rect.height(), model.result.tick_size)
    });
    let replay_levels = replay_clock.filter(|_| depth_width > 0.0).and_then(|clock| {
        let (top, bottom) = edge_spec.as_ref().map_or((m.p1, m.p0), RasterSpec::price_range);
        model.replay_depth(clock, m.p0.min(bottom), m.p1.max(top))
    });
    if let (Some((left, cache)), Some(spec), Some(levels)) = (edge, edge_spec, &replay_levels) {
        let sigma_rows = price_sigma_rows(ui.ctx(), settings, &m, model.result.tick_size, spec.row_p);
        let repeat = price_row_repeat(&spec, physical_pixels_per_price(ui.ctx(), &m));
        draw_heat_depth(&painter, &m, left, cache, &spec, levels, repeat, sigma_rows);
    }
    draw_axes(&axis_painter, &painter, model, &m, price_axis_left);
    // Time-based layers stop at the replay cursor.
    let cut_px = replay_x.map(|x| m.sx(x) + 1.0);
    let full_painter = painter;
    let painter = match cut_px {
        Some(px) => full_painter
            .with_clip_rect(Rect::from_min_max(plot.min, pos2(px.clamp(plot.left(), plot.right()), plot.bottom()))),
        None => full_painter.clone(),
    };
    draw_boxes(&painter, model, settings, &m);
    if settings.show_strategy_profiles {
        crate::vp::draw_strategy_profiles(&painter, model, &m);
    }
    if settings.mode == ChartMode::Bar {
        // The replay supplies only completed bars and the current partial bar. Its full
        // on-screen footprint can extend past the clock without showing future trades.
        draw_bars(&full_painter, model, settings, &m);
    }
    if !heat_visible {
        draw_overlays(&painter, model, &m, settings);
    }
    if show_quotes {
        draw_quote_columns(&painter, &m, &columns);
    }
    let hovered_bubble = if show_bubbles {
        let bubble_pointer = hover.filter(|_| !pick_bar && !drawing_vp && !response.dragged());
        if let Some((clock, tape)) = replay_bubble_source {
            let view = BubbleView {
                x: [m.x0, m.x1],
                pixels_per_unit: bubble_pixels(&m),
                clock,
                follow: state.follow_replay,
                auto_y: state.auto_y,
                min_trade: settings.bubble_min_trade_size,
                clustering: bubble_clustering(settings),
            };
            state.replay_bubbles.update(ui.ctx(), view, &model.axis, tape).and_then(|frame| {
                draw_trade_bubbles(&painter, &m, frame.groups(), settings, bubble_pointer)
            })
        } else {
            let groups = cluster_trade_columns(&m, &columns, bubble_clustering(settings));
            draw_trade_bubbles(&painter, &m, &groups, settings, bubble_pointer)
        }
    } else {
        None
    };
    if show_quotes {
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
    if heat_visible {
        draw_overlays(&painter, model, &m, settings);
    }
    if settings.big_trades.visible {
        draw_big_trades(&painter, model, &m, &settings.big_trades);
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
    if let Some(px) = cut_px.filter(|px| plot.x_range().contains(*px)) {
        painter.vline(px, plot.y_range(), Stroke::new(1.0, REPLAY));
    }
    if depth_width > 0.0 {
        let rect = Rect::from_min_max(plot.right_top(), pos2(price_axis_left, outer.bottom()));
        draw_replay_depth(ui, rect, &m, model, replay_levels.as_deref());
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
        draw_crosshair(
            &axis_painter, &painter, model, &m, pos, event_time.then_some(columns.as_slice()),
            query_hover_trade.then_some(settings.bubble_min_trade_size), price_axis_left,
        );
    }
    indicator_legend(ui, plot, model, settings);
    if let Some(pos) = hover.filter(|_| !pick_bar && !drawing_vp && !response.dragged()) {
        let liquidity = if edge.is_some_and(|(left, _)| pos.x > left) {
            replay_levels.as_deref().and_then(|levels| depth_tooltip(model, &m, pos.y, levels))
        } else if !heat_visible || replay_x.is_some_and(|x| m.x_at(pos.x) > x) {
            None
        } else {
            heatmap_tooltip(state, model, &m, pos, settings.heat_metric)
        };
        let trade = hovered_bubble.map(|dot| bubble_tooltip(model, dot, settings.bubble_delta));
        let info = match (liquidity, trade) {
            (Some(liquidity), Some(trade)) => Some(format!("{liquidity}\n\n{trade}")),
            (liquidity, trade) => liquidity.or(trade),
        };
        if let Some(info) = info {
            draw_chart_tooltip(&painter, pos, info);
        }
    }
    egui::InnerResponse::new(picked, response)
}

fn draw_heat_depth(
    painter: &Painter, m: &Mapper, left: f32, cache: &HeatCache, spec: &RasterSpec, levels: &[LiquidityLevel],
    repeat: usize, sigma_rows: f32,
) {
    let mut sizes = vec![0.0_f32; spec.rows];
    for level in levels {
        let row = spec.k_hi - (level.price / spec.row_p).round() as i64;
        if let Some(cell) = sizes.get_mut(row as usize) {
            let size = cache.key.metric.size([level.bid_size, level.ask_size]) as f32;
            *cell = cell.max(size);
        }
    }
    let (pixels, _) = liquidity_colors::price_pixels(&sizes, 1, repeat, sigma_rows);
    let step = spec.row_p / (pixels.len() / spec.rows) as f64;
    let top = spec.price_range().0;
    let mut row = 0;
    for band in pixels.chunk_by(|a, b| a == b) {
        let start = row;
        row += band.len();
        let size = band[0];
        if size <= 0.0 {
            continue;
        }
        let [r, g, b] = liquidity_colors::palette(cache.scale.intensity(size));
        painter.rect_filled(
            Rect::from_min_max(pos2(left, m.sy(top - start as f64 * step)),
                pos2(m.rect.right(), m.sy(top - row as f64 * step))),
            0.0, Color32::from_rgb(r, g, b),
        );
    }
}

fn draw_replay_depth(ui: &egui::Ui, rect: Rect, m: &Mapper, model: &Model, levels: Option<&[LiquidityLevel]>) {
    let rows = Rect::from_min_max(rect.min, pos2(rect.right(), m.rect.bottom()));
    let painter = ui.painter_at(rows);
    painter.rect_filled(rect, 0.0, BACKGROUND);
    let font = FontId::monospace(11.0);
    let [bid_color, ask_color] = CYAN_VIOLET_BUBBLES;
    let footer = ui.painter_at(rect);
    let baseline = pos2(rect.center().x, rows.bottom() + 4.0);
    footer.text(baseline - egui::vec2(5.0, 0.0), Align2::RIGHT_TOP, "Bid", font.clone(), bid_color);
    footer.text(baseline + egui::vec2(5.0, 0.0), Align2::LEFT_TOP, "Ask", font.clone(), ask_color);
    let Some(levels) = levels else {
        painter.text(rows.center(), Align2::CENTER_CENTER, "Loading depth…", font, AXIS_TEXT);
        return;
    };
    let tick = model.result.tick_size.abs();
    let row_height = (m.sy(m.p0) - m.sy(m.p0 + tick)).max(0.0);
    let visible_levels = levels.iter().filter(|level| (m.p0..=m.p1).contains(&level.price));
    let largest = visible_levels.clone().max_by(|a, b| {
        (a.bid_size + a.ask_size).total_cmp(&(b.bid_size + b.ask_size))
    }).filter(|level| level.bid_size + level.ask_size > 0.0);
    let max_size = largest.map_or(0.0, |level| level.bid_size + level.ask_size);
    let right = rect.right() - 4.0;
    let bar_space = rect.width() - 8.0;
    let height = (row_height - 1.0).clamp(1.0, 23.0);
    for level in visible_levels {
        let y = m.sy(level.price);
        let highlight = largest.is_some_and(|largest| std::ptr::eq(largest, level));
        let mut offset = 0.0;
        for (size, color) in [(level.ask_size, ask_color), (level.bid_size, bid_color)] {
            if size <= 0.0 {
                continue;
            }
            let width = (bar_space * (size / max_size) as f32).max(1.0);
            let end = right - offset;
            painter.rect_filled(
                Rect::from_min_max(pos2(end - width, y - height * 0.5), pos2(end, y + height * 0.5)),
                0.0, if highlight { color } else { color.gamma_multiply(0.58) },
            );
            if !highlight && row_height >= 12.0 && (offset == 0.0 || width >= 32.0) {
                painter.text(pos2(end - 3.0, y), Align2::RIGHT_CENTER, tooltip_size(size), font.clone(), Color32::WHITE);
            }
            offset += width;
        }
    }
    if let Some(level) = largest {
        let text = painter.layout_no_wrap(tooltip_size(max_size), font, Color32::WHITE);
        let padding = egui::vec2(3.0, 1.0);
        let size = text.size() + padding * 2.0;
        let y = m.sy(level.price).clamp(rows.top() + size.y * 0.5, rows.bottom() - size.y * 0.5);
        let label = Rect::from_min_size(pos2(rows.left() + 4.0, y - size.y * 0.5), size);
        painter.rect_filled(label, 2.0, BACKGROUND);
        painter.galley(label.min + padding, text, Color32::WHITE);
    }
    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()).filter(|pos| rows.contains(*pos)) {
        if let Some(info) = depth_tooltip(model, m, pos.y, levels) {
            draw_chart_tooltip(ui.painter(), pos, info);
        }
    }
}

fn depth_tooltip(model: &Model, m: &Mapper, y: f32, levels: &[LiquidityLevel]) -> Option<String> {
    let price = m.p_at(y);
    let tick = model.result.tick_size.abs();
    let level = levels.iter().find(|level| (level.price - price).abs() <= tick * 0.5)?;
    Some(format!("Resting at {:.*}\nBid  {}\nAsk  {}", model.price_decimals, level.price,
        tooltip_size(level.bid_size), tooltip_size(level.ask_size)))
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
) {
    let rect = Rect::from_min_max(plot.left_top() + egui::vec2(6.0, 28.0), plot.right_bottom());
    let builder = egui::UiBuilder::new()
        .max_rect(rect)
        .layout(egui::Layout::top_down(egui::Align::Min));
    ui.scope_builder(builder, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;

        let overlays = &model.overlays;
        if !overlays.lines.is_empty()
            || overlays.boxes.iter().any(|plot| plot.group.is_none())
            || overlays.segments.iter().any(|plot| plot.group.is_none())
        {
            legend_row(ui, "Overlays (zones, lines)", &mut settings.show_overlays, true, false);
        }
        if overlays.markers.iter().any(|plot| plot.group.is_none()) {
            legend_row(ui, "Strategy markers (dots)", &mut settings.show_markers, true, false);
        }
        for group in &overlays.groups {
            let mut visible = !settings.hidden_overlay_groups.contains(group);
            let was_visible = visible;
            legend_row(ui, group, &mut visible, true, false);
            if visible != was_visible {
                if visible {
                    settings.hidden_overlay_groups.remove(group);
                } else {
                    settings.hidden_overlay_groups.insert(group.clone());
                }
            }
        }

        if !overlays.profiles.is_empty() {
            let label = format!("Volume profile ({})", overlays.profiles.len());
            legend_row(ui, &label, &mut settings.show_strategy_profiles, true, false);
        }
        if let Some(gear) = legend_row(
            ui, "Big Trades", &mut settings.big_trades.visible, model.big_trades.available(), true,
        ) {
            let mut open = settings.big_trades.settings_open ^ gear.clicked();
            egui::Window::new("Big Trades")
                .default_pos(gear.rect.left_bottom())
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ui.ctx(), |ui| settings.big_trades.controls(ui, &model.big_trades));
            settings.big_trades.settings_open = open;
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
    response: &egui::Response,
    state: &mut ChartState,
    pan: bool,
    tick: f64,
    replay_x: Option<f64>,
) {
    let plot = response.rect;
    if pan && response.dragged() {
        state.follow_replay = false;
        let delta = response.drag_delta();
        pan_time(state, -delta.x, plot.width(), replay_x);
        if !state.auto_y {
            let price_per_px = (state.p1 - state.p0) / plot.height() as f64;
            state.p0 += delta.y as f64 * price_per_px;
            state.p1 += delta.y as f64 * price_per_px;
        }
    }
    if response.double_clicked() {
        state.follow_replay = false;
        state.fit_all();
    }
    if let Some(pos) = response.hover_pos() {
        let (scroll, zoom) = scroll_zoom(response);
        if scroll.y != 0.0 {
            let fraction = f64::from((pos.x - plot.left()) / plot.width());
            state.zoom_time(fraction, (-f64::from(scroll.y) * 0.0025).exp(), replay_x);
        }
        if scroll.x != 0.0 {
            state.follow_replay = false;
            pan_time(state, -scroll.x, plot.width(), replay_x);
        }
        if zoom != 1.0 {
            let fraction = f64::from(((pos.y - plot.top()) / plot.height()).clamp(0.0, 1.0));
            state.zoom_price(fraction, 1.0 / zoom, tick);
        }
    }
}

fn pan_time(state: &mut ChartState, delta: f32, width: f32, replay_x: Option<f64>) {
    if delta != 0.0 {
        let shift = f64::from(delta / width) * (state.x1 - state.x0);
        state.x0 += shift;
        state.x1 += shift;
        state.clamp_x_with_replay_margin(replay_x.is_some());
    }
}

fn scroll_zoom(response: &egui::Response) -> (egui::Vec2, f64) {
    let (scroll, zoom) = response.ctx.input(|i| (i.smooth_scroll_delta(), i.zoom_delta()));
    (scroll, if zoom.is_finite() && zoom > 0.0 { f64::from(zoom) } else { 1.0 })
}

fn handle_axis_input(
    price_axis: &egui::Response,
    time_axis: &egui::Response,
    plot: Rect,
    state: &mut ChartState,
    tick: f64,
    replay_x: Option<f64>,
) {
    if let Some((pos, factor)) = axis_zoom(price_axis, egui::Direction::TopDown) {
        let fraction = f64::from(((pos.y - plot.top()) / plot.height()).clamp(0.0, 1.0));
        state.zoom_price(fraction, factor, tick);
    }
    if let Some((pos, factor)) = axis_zoom(time_axis, egui::Direction::LeftToRight) {
        let fraction = f64::from(((pos.x - plot.left()) / plot.width()).clamp(0.0, 1.0));
        state.zoom_time(fraction, factor, replay_x);
    }
}

fn axis_zoom(response: &egui::Response, direction: egui::Direction) -> Option<(Pos2, f64)> {
    let vertical = direction == egui::Direction::TopDown;
    let dragging = response.dragged_by(egui::PointerButton::Primary);
    if dragging || response.hovered() {
        response.ctx.set_cursor_icon(if vertical {
            egui::CursorIcon::ResizeVertical
        } else {
            egui::CursorIcon::ResizeHorizontal
        });
    }
    if dragging {
        let movement = response.drag_delta();
        let delta = if vertical { movement.y } else { movement.x };
        let anchor = response.ctx.input(|i| i.pointer.press_origin());
        return anchor.filter(|_| delta != 0.0).map(|pos| (pos, (f64::from(delta) / 250.0).exp()));
    }
    let pos = response.hover_pos()?;
    let (scroll, zoom) = scroll_zoom(response);
    let delta = if !vertical && scroll.x.abs() > scroll.y.abs() { scroll.x } else { scroll.y };
    (delta != 0.0 || zoom != 1.0)
        .then(|| (pos, (-f64::from(delta) * 0.0025).exp() / zoom))
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
    if settings.mode == ChartMode::Bar {
        let i0 = model.fp_x.partition_point(|f| f.1 < x0);
        let i1 = model.fp_x.partition_point(|f| f.0 <= x1);
        for fp in &model.bars()[i0..i1.max(i0)] {
            lo = lo.min(fp.low);
            hi = hi.max(fp.high);
        }
    }
    for column in columns.iter().flatten() {
        if settings.show_bbo || (settings.mode == ChartMode::Heatmap && settings.show_heat) {
            for price in column.prices.into_iter().filter(|price| price.is_finite()) {
                lo = lo.min(price);
                hi = hi.max(price);
            }
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
        let pending = state.heat_worker.as_ref().and_then(|worker| worker.pending.as_ref());
        let previous_width = pending.or_else(|| state.heat.as_ref().map(|cache| &cache.key))
            .map(|key| key.spec.col_w);
        spec = spec.with_event_time(m.x0, m.x1, m.rect.width(), previous_width);
    }
    let mut studies = settings.heat_studies;
    let pixels_per_price = physical_pixels_per_price(ctx, m);
    let sigma_rows = price_sigma_rows(ctx, settings, m, model.result.tick_size, spec.row_p);
    let (xa, xb) = spec.x_range();
    let total = model.axis.total();
    let at = |x: f64| model.axis.to_ts(x.clamp(0.0, total));
    let from = at(xa);
    let until = at(xb).min(model.replay_clock().unwrap_or(i64::MAX)).max(from).saturating_add(1);
    let lookback = settings.heat_layers_lookback();
    let depth = (!model.depth.is_empty()).then(|| {
        let start = from.saturating_sub(lookback.unwrap_or_default());
        model.depth_at(start, until.saturating_add(lookback.unwrap_or_default()))
    });
    let depth_window = depth.and_then(|(_, window)| window);
    let view_span = at(m.x1).saturating_add(1) - at(m.x0);
    let exact = depth.is_some()
        && (model.depth.is_exact()
            || (view_span <= crate::EXACT_DEPTH_SPAN_NS && depth_window.is_some()));
    if lookback.is_some() && !exact {
        let notice = if view_span > crate::EXACT_DEPTH_SPAN_NS {
            "Zoom to 12 hours or less for event-time layers."
        } else {
            "Waiting for event-time depth."
        };
        painter.text(m.rect.left_bottom() + egui::vec2(10.0, -8.0), Align2::LEFT_BOTTOM,
            notice, FontId::proportional(13.0), AXIS_TEXT);
    }
    studies.filter_levels &= exact;
    let resting_spec = if depth.is_some() {
        let cached = state.heat.iter().map(|cache| &cache.key.spec);
        let pending = state.heat_worker.iter().filter_map(|worker| worker.pending.as_ref().map(|key| &key.spec));
        resting_price_spec(spec, cached.chain(pending))
    } else {
        spec
    };
    let key = HeatKey {
        spec: resting_spec,
        metric: settings.heat_metric,
        studies,
        contrast: settings.heat_contrast,
        smooth: settings.heat_smooth,
        sigma_rows,
        low_pct: settings.heat_low_pct,
        high_pct: settings.heat_high_pct,
        replay_ns: model.replay_clock(),
        depth_window,
    };
    if !settings.heat_layer_enabled(Study::Resting) {
        state.heat = None;
        state.heat_worker = None;
        state.heat_preview = None;
    } else {
        update_resting_heat(ctx, model, state, &key, &spec, depth.map(|(history, _)| history), pixels_per_price);
        let visible = heat_clip(painter.clip_rect().intersect(m.rect), m, &model.axis, key.replay_ns);
        let detailed_clip = state.heat.as_ref().map(|cache| {
            let cutoff = cache.key.replay_ns
                .filter(|&then| key.replay_ns.is_some_and(|now| now > then));
            heat_clip(visible, m, &model.axis, cutoff)
        });
        let detailed_rect = state.heat.as_ref().zip(detailed_clip)
            .map_or(Rect::NOTHING, |(cache, clip)| heat_rect(m, &cache.key.spec).intersect(clip));
        let exposed = uncovered_heat_rects(visible, detailed_rect);
        if depth.is_some() && !key.studies.active() && exposed.iter().any(Rect::is_positive) {
            draw_heat_preview(ctx, painter, model, m, &key, state, exposed);
        }
        if let Some((cache, clip)) = state.heat.as_ref().zip(detailed_clip) {
            // Draw with the texture's own bounds so changing the mapper never shifts its data.
            paint_heat_texture(
                &painter.with_clip_rect(clip),
                m, &cache.key.spec, &cache.texture, cache.replay_texture.as_ref(),
            );
        }
    }

    let available = |study: Study| {
        exact && settings.heat_layer_enabled(study) && (!study.requires_trades() || model.tape.is_some())
    };
    let markers = std::array::from_fn(|index| {
        available(heatmap_studies::MARKER_STUDIES[index]).then_some(settings.flow_marker_minimum[index])
    });
    let show_net = available(Study::Net);
    if !show_net && markers.iter().all(Option::is_none) {
        state.flow_heat = None;
        state.flow_worker = None;
        return;
    }
    let visible = (at(m.x0), at(m.x1).saturating_add(1));
    let prices = (m.p0, m.p1);
    let flow_key = FlowHeatKey {
        heat: HeatKey { spec, studies: Studies::default(), smooth: false, sigma_rows: 0.0, ..key },
        visible,
        prices,
        markers,
        marker_limit: settings.flow_marker_limit,
        show_net,
    };
    if state.flow_heat.as_ref().is_some_and(|cache| !flow_key.can_preview(&cache.key)) {
        state.flow_heat = None;
    }
    let worker = state.flow_worker.get_or_insert_with(|| FlowHeatWorker::spawn(ctx));
    if let Some(ready) = worker.poll(&flow_key) {
        let old = state.flow_heat.take().and_then(|cache| cache.texture);
        let texture = ready.image.map(|image| {
            if let Some(mut texture) = old {
                texture.set(image, TextureOptions::NEAREST);
                texture
            } else {
                ctx.load_texture("net-liquidity", image, TextureOptions::NEAREST)
            }
        });
        state.flow_heat = Some(FlowHeatCache { key: ready.key, markers: ready.layers.markers, net: ready.layers.net, texture });
    }
    if !worker.pending && state.flow_heat.as_ref().is_none_or(|cache| cache.key != flow_key) {
        let replay_until = flow_key.heat.replay_ns.map_or(i64::MAX, |ts| ts.saturating_add(1));
        let (top, bottom) = spec.price_range();
        let query = FlowQuery {
            prices: if show_net { (prices.0.min(bottom), prices.1.max(top)) } else { prices },
            marker_prices: prices,
            visible_times: visible,
            read_until: heatmap_studies::flow_read_until(visible.1.max(until))
                .min(at(total).saturating_add(1)).min(replay_until),
            initial_depth_ns: model.depth.quote_changes().first().map(|quote| quote.0)
                .into_iter().chain(depth_window.map(|(from, _)| from)).max(),
            metric: flow_key.heat.metric,
            tick: model.result.tick_size,
            markers,
            marker_limit: flow_key.marker_limit,
        };
        let job = FlowHeatJob {
            key: flow_key, query, trades_from: from.saturating_sub(heatmap_studies::FLOW_LOOKBACK_NS),
            axis: model.axis.clone(), store: model.depth.clone(), window: depth_window.and(model.depth_window.clone()),
            tape: model.tape.as_ref().filter(|_| show_net || markers[2].is_some()).cloned(),
        };
        if worker.jobs.send(job).is_ok() {
            worker.pending = true;
        } else {
            state.flow_worker = None;
            ctx.request_repaint();
        }
    }
    if let Some(cache) = &state.flow_heat {
        if let Some(texture) = &cache.texture {
            let clip = heat_clip(painter.clip_rect(), m, &model.axis, cache.key.heat.replay_ns);
            paint_heat_texture(&painter.with_clip_rect(clip), m, &cache.key.heat.spec, texture, None);
            painter.text(
                m.rect.left_bottom() + egui::vec2(10.0, -8.0),
                Align2::LEFT_BOTTOM,
                "Net liquidity added (5 s)",
                FontId::proportional(13.0),
                study_color(Study::Net, 1.0),
            );
        }
        draw_flow_markers(painter, model, m, &cache.markers);
    }
}

fn resting_price_spec<'a>(mut visible: RasterSpec, saved: impl Iterator<Item = &'a RasterSpec>) -> RasterSpec {
    let margin = (visible.rows / 4).max(2);
    let lower = visible.k_hi - visible.rows as i64;
    if let Some(old) = saved.into_iter().find(|old| {
        old.row_p == visible.row_p && old.k_hi >= visible.k_hi
            && old.k_hi - old.rows as i64 <= lower && old.rows <= visible.rows + 4 * margin
    }) {
        visible.k_hi = old.k_hi;
        visible.rows = old.rows;
    } else {
        visible.k_hi += margin as i64;
        visible.rows += 2 * margin;
    }
    visible
}

fn heat_clip(mut rect: Rect, m: &Mapper, axis: &data::TimeAxis, cutoff: Option<i64>) -> Rect {
    if let Some(cutoff) = cutoff {
        rect.max.x = rect.max.x.min(m.sx(axis.to_x(cutoff))).max(rect.min.x);
    }
    rect
}

fn uncovered_heat_rects(view: Rect, covered: Rect) -> [Rect; 4] {
    if !covered.is_positive() {
        return [view, Rect::NOTHING, Rect::NOTHING, Rect::NOTHING];
    }
    [
        Rect::from_min_max(view.min, pos2(covered.left(), view.bottom())),
        Rect::from_min_max(pos2(covered.right(), view.top()), view.max),
        Rect::from_min_max(pos2(covered.left(), view.top()), covered.right_top()),
        Rect::from_min_max(covered.left_bottom(), pos2(covered.right(), view.bottom())),
    ]
}

const PREVIEW_TIME_COLUMNS: f64 = 144.0;

fn price_sigma_rows(ctx: &egui::Context, settings: &Settings, m: &Mapper, tick: f64, row_p: f64) -> f32 {
    let pixels_per_price = physical_pixels_per_price(ctx, m);
    settings.price_filter.sigma(tick.abs() * pixels_per_price, row_p * pixels_per_price, settings.price_filter_px)
}

fn physical_pixels_per_price(ctx: &egui::Context, m: &Mapper) -> f64 {
    f64::from(m.rect.height() * ctx.pixels_per_point()) / (m.p1 - m.p0)
}

fn price_row_repeat(spec: &RasterSpec, pixels_per_price: f64) -> usize {
    ((spec.row_p * pixels_per_price).round() as usize).clamp(1, (2048 / spec.rows).max(1))
}

fn preview_spec(m: &Mapper, row_p: f64) -> RasterSpec {
    // Keep the price grid identical to the detailed heatmap. Coarsen only time.
    let x_margin = (m.x1 - m.x0) * 0.25;
    let p_margin = (m.p1 - m.p0) * 0.125;
    let col_w = ((m.x1 - m.x0 + 2.0 * x_margin) / PREVIEW_TIME_COLUMNS).log2().ceil().exp2();
    let c_lo = ((m.x0 - x_margin) / col_w).floor() as i64;
    let c_hi = ((m.x1 + x_margin) / col_w).ceil() as i64;
    let k_hi = ((m.p1 + p_margin) / row_p).ceil() as i64;
    let k_lo = ((m.p0 - p_margin) / row_p).floor() as i64;
    RasterSpec { c_lo, cols: (c_hi - c_lo) as usize, col_w, k_hi, rows: (k_hi - k_lo + 1) as usize, row_p }
}

fn draw_heat_preview(
    ctx: &egui::Context,
    painter: &Painter,
    model: &Model,
    m: &Mapper,
    key: &HeatKey,
    state: &mut ChartState,
    exposed: [Rect; 4],
) {
    let mut preview_key = key.clone();
    let preferred = preview_spec(m, key.spec.row_p);
    if let Some(previous) = state.heat_preview.as_ref().filter(|previous| {
        heat_rect(m, &previous.key.spec).contains_rect(m.rect)
            && previous.key.spec.col_w <= preferred.col_w
            && previous.key.spec.row_p == preferred.row_p
    }) {
        preview_key.spec = previous.key.spec;
    } else {
        preview_key.spec = preferred;
    }
    let spec = &preview_key.spec;
    let (left, right) = spec.x_range();
    let at = |x: f64| model.axis.to_ts(x.clamp(0.0, model.axis.total()));
    let from = at(left);
    let until = at(right).min(key.replay_ns.unwrap_or(i64::MAX)).max(from).saturating_add(1);
    let (depth, window) = model.depth_at(from, until);
    preview_key.depth_window = window;
    let overview_bucket = model.depth.overview_bucket_ns();
    if window.is_none() && overview_bucket > 0 {
        // An overview mean contains its whole bucket. Use only buckets complete at the clock.
        preview_key.replay_ns = key.replay_ns.map(|ns| {
            ns.saturating_add(1).div_euclid(overview_bucket)
                .saturating_mul(overview_bucket).saturating_sub(1)
        });
    }
    let detail = state.heat.as_ref().filter(|cache| {
        let clip = heat_clip(painter.clip_rect(), m, &model.axis, cache.key.replay_ns);
        heat_rect(m, &cache.key.spec).intersect(clip).is_positive()
    });
    if let Some(detail) = detail {
        // During Auto Y or a price zoom, the displayed texture may still use
        // the previous price scale. Match its visible falloff until it is replaced.
        preview_key.sigma_rows = detail.key.sigma_rows * (detail.key.spec.row_p / spec.row_p) as f32;
    }
    let detailed_scale = detail.map(|cache| cache.scale);
    let repeat = if preview_key.sigma_rows > 0.0 {
        price_row_repeat(spec, physical_pixels_per_price(ctx, m))
    } else {
        1
    };
    if state.heat_preview.as_ref().is_none_or(|preview| {
        preview.key != preview_key || preview.detailed_scale != detailed_scale || preview.repeat != repeat
    }) {
        let times = market_history::raster_times(spec, &model.axis, preview_key.replay_ns);
        let cells = market_history::rasterize_preview_depth(spec, &times, depth, key.metric);
        // Match the price kernel. Adjacent preview columns span many more pixels,
        // so applying the time filter here would smear events across the screen.
        let (pixels, remaining_repeat) = liquidity_colors::price_pixels(&cells, spec.cols, repeat, preview_key.sigma_rows);
        let scale = detailed_scale.unwrap_or_else(|| {
            ColorScale::from_cells(&pixels, key.low_pct, key.high_pct, key.contrast)
        });
        let image = heat_image(&pixels, spec.cols, remaining_repeat, &scale, None);
        let texture = if let Some(mut previous) = state.heat_preview.take() {
            previous.texture.set(image, TextureOptions::NEAREST);
            previous.texture
        } else {
            ctx.load_texture("liquidity-heatmap-preview", image, TextureOptions::NEAREST)
        };
        state.heat_preview = Some(HeatPreview { key: preview_key, detailed_scale, repeat, texture });
    }
    if let Some(preview) = &state.heat_preview {
        for rect in exposed.into_iter().filter(Rect::is_positive) {
            let clip = heat_clip(rect, m, &model.axis, preview.key.replay_ns);
            if clip.is_positive() {
                paint_heat_texture(&painter.with_clip_rect(clip), m, &preview.key.spec, &preview.texture, None);
            }
        }
    }
}

fn update_resting_heat(
    ctx: &egui::Context,
    model: &Model,
    state: &mut ChartState,
    key: &HeatKey,
    visible_spec: &RasterSpec,
    depth: Option<&accelerando_core::DepthHistory>,
    pixels_per_price: f64,
) {
    if state.heat.as_ref().is_some_and(|cache| !key.can_preview(&cache.key)) {
        state.heat = None;
    }
    if let Some(worker) = &mut state.heat_worker {
        match worker.frames.try_recv() {
            Ok((ready, image)) => {
                worker.pending = None;
                if key.accepts(&ready) && state.heat.as_ref().is_none_or(|cache| {
                    cache.key != *key && cache.key.replay_ns <= ready.replay_ns
                }) {
                    state.heat = Some(image.upload(ctx, ready, state.heat.take()));
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => state.heat_worker = None,
        }
    }
    if state.heat.as_ref().is_some_and(|cache| cache.key == *key) {
        return;
    }

    let spec = &key.spec;
    let repeat = key.row_repeat(pixels_per_price);
    let replay_x = model.replay_cut_x();
    let groups = || key.smooth.then(|| liquidity_colors::column_groups(spec, &model.axis, replay_x));
    let Some(depth) = depth else {
        let cells = data::rasterize(spec, &model.grid, model.replay_snapshots(key.replay_ns), key.metric);
        let image = HeatImage::build(key, None, cells, groups(), repeat, 0..spec.rows);
        state.heat = Some(image.upload(ctx, key.clone(), state.heat.take()));
        return;
    };

    // Ordinary replay ticks can still update a few columns without a full upload.
    let incremental = state.heat.as_mut().filter(|cache| {
        let Some(ring) = &cache.replay_texture else { return false };
        let old = &cache.key;
        let overlap = key.overlap_columns(old);
        overlap > 0 && key.same_depth_grid(old) && key.sigma_rows == old.sigma_rows
            && ring.width >= spec.cols + 2 && ring.repeat == repeat && ring.has_colors
            && key.replay_ns.zip(old.replay_ns).is_some_and(|(now, then)| {
                let new_columns = spec.cols - overlap;
                let elapsed_columns = now.saturating_sub(then) as f64 / (spec.col_w * data::SEC_NS as f64);
                now >= then && new_columns as f64 + elapsed_columns <= 32.0
            })
    });
    if let Some(cache) = incremental {
        let previous = cache.depth_cells.as_ref().map(|raster| (&cache.key, raster));
        let raster = DepthRaster::build(key, &model.axis, depth, previous);
        let groups = groups();
        update_replay_heat(cache, key, &raster, groups.as_deref(), key.texture_options(true));
        cache.key = key.clone();
        cache.depth_cells = Some(raster);
        cache.replay_texture.as_mut().unwrap().groups = groups;
        return;
    }

    // Keep one task in flight. After it completes, submit the then-current viewport.
    let worker = state.heat_worker.get_or_insert_with(|| HeatWorker::spawn(ctx));
    if worker.pending.is_none() {
        let color_start = (spec.k_hi - visible_spec.k_hi) as usize;
        let job = HeatJob {
            key: key.clone(), axis: model.axis.clone(), replay_x, store: model.depth.clone(),
            window: key.depth_window.and(model.depth_window.clone()), repeat,
            color_rows: color_start..color_start + visible_spec.rows,
        };
        if worker.jobs.send(job).is_ok() {
            worker.pending = Some(key.clone());
        } else {
            state.heat_worker = None;
            ctx.request_repaint();
        }
    }
}

fn flow_marker_pos(model: &Model, m: &Mapper, marker: &FlowMarker) -> Pos2 {
    m.pos(model.axis.to_x(marker.ts), marker.price)
}

fn flow_marker_amount(marker: &FlowMarker) -> String {
    let sign = if marker.study == Study::Added { "+" } else { "−" };
    format!("{sign}{}", tooltip_size(marker.volume))
}

fn draw_flow_markers(painter: &Painter, model: &Model, m: &Mapper, markers: &[FlowMarker]) {
    let font = FontId::monospace(11.0);
    let mut occupied = Vec::<Rect>::new();
    for marker in markers {
        let color = study_color(marker.study, 1.0);
        let pos = flow_marker_pos(model, m, marker);
        let galley = painter.layout_no_wrap(flow_marker_amount(marker), font.clone(), color);
        let size = galley.size();
        let center = -size.y * 0.5;
        let offsets = [
            egui::vec2(6.0, center),
            egui::vec2(-size.x - 6.0, center),
            egui::vec2(6.0, center - size.y - 3.0),
            egui::vec2(6.0, center + size.y + 3.0),
        ];
        let label = offsets.into_iter().find_map(|offset| {
            let anchor = pos + offset;
            let rect = Rect::from_min_size(anchor, size).expand2(egui::vec2(3.0, 1.0));
            (m.rect.contains_rect(rect) && occupied.iter().all(|previous| !previous.intersects(rect)))
                .then_some((anchor, rect))
        });
        painter.circle(pos, 3.5, color, Stroke::new(1.0, BACKGROUND));
        if let Some((anchor, rect)) = label {
            painter.rect_filled(rect, 3.0, BACKGROUND.gamma_multiply(0.92));
            painter.galley(anchor, galley, color);
            occupied.push(rect);
        }
    }
}

fn paint_heat_texture(
    painter: &Painter,
    m: &Mapper,
    spec: &RasterSpec,
    texture: &TextureHandle,
    ring: Option<&ReplayHeatTexture>,
) {
    let rect = heat_rect(m, spec);
    let (u0, u1) = ring.map_or((0.0, 1.0), |ring| {
        let start = ring.slot(spec.c_lo);
        (start as f32 / ring.width as f32, (start + spec.cols) as f32 / ring.width as f32)
    });
    let uv = Rect::from_min_max(pos2(u0, 0.0), pos2(u1, 1.0));
    painter.image(texture.id(), rect, uv, Color32::WHITE);
}

fn heat_rect(m: &Mapper, spec: &RasterSpec) -> Rect {
    let (xa, xb) = spec.x_range();
    let (top, bottom) = spec.price_range();
    Rect::from_min_max(m.pos(xa, top), m.pos(xb, bottom))
}

pub(crate) fn study_color(study: Study, value: f32) -> Color32 {
    match study {
        Study::Added => CYAN_VIOLET_BUBBLES[0],
        Study::Decreased => Color32::from_rgb(255, 179, 77),
        Study::Cancelled => CYAN_VIOLET_BUBBLES[1],
        Study::Net if value < 0.0 => Color32::from_rgb(244, 199, 255),
        Study::Net => Color32::from_rgb(105, 155, 255),
        Study::Resting => AXIS_TEXT,
    }
}

fn net_heat_image(key: &HeatKey, cells: &[f32]) -> ColorImage {
    let magnitudes: Vec<_> = cells.iter().map(|value| value.abs()).collect();
    let scale = ColorScale::from_cells(&magnitudes, key.low_pct, key.high_pct, key.contrast);
    let pixels = cells.iter().map(|&value| {
        if !value.is_finite() || value == 0.0 {
            return Color32::TRANSPARENT;
        }
        let intensity = scale.intensity(value.abs());
        if intensity == 0.0 {
            return Color32::TRANSPARENT;
        }
        study_color(Study::Net, value).gamma_multiply(0.25 + intensity * 0.75)
    }).collect();
    ColorImage::new([key.spec.cols, key.spec.rows], pixels)
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

fn draw_axes(axis_painter: &Painter, painter: &Painter, model: &Model, m: &Mapper, price_axis_left: f32) {
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
            pos2(price_axis_left + 6.0, y),
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

/// Expand dashes only on the visible portion of each segment, preserving the phase
/// along the original path as the viewport moves.
pub(crate) fn draw_dashes(painter: &Painter, points: &[Pos2], stroke: Stroke, dash: f32, gap: f32) {
    let clip = painter.clip_rect().expand(stroke.width);
    let period = f64::from(dash + gap);
    let mut phase = 0.0;
    for edge in points.windows(2) {
        let [from, to] = [edge[0], edge[1]];
        let x = f64::from(to.x) - f64::from(from.x);
        let y = f64::from(to.y) - f64::from(from.y);
        let length = x.hypot(y);
        if !length.is_finite() || length == 0.0 {
            continue;
        }
        let ux = x / length;
        let uy = y / length;
        let mut first = 0.0_f64;
        let mut last = length;
        for (coordinate, direction, low, high) in [
            (from.x, ux, clip.left(), clip.right()),
            (from.y, uy, clip.top(), clip.bottom()),
        ] {
            if direction == 0.0 {
                if coordinate < low || coordinate > high {
                    last = 0.0;
                }
                continue;
            }
            let enter = (f64::from(low) - f64::from(coordinate)) / direction;
            let exit = (f64::from(high) - f64::from(coordinate)) / direction;
            first = first.max(enter.min(exit));
            last = last.min(enter.max(exit));
        }
        let span = last - first;
        let mut offset = -(phase + first.rem_euclid(period)).rem_euclid(period);
        let point = |offset| {
            let distance = first + offset;
            pos2((f64::from(from.x) + ux * distance) as f32, (f64::from(from.y) + uy * distance) as f32)
        };
        while span > 0.0 && offset < span {
            let a = offset.max(0.0);
            let b = (offset + f64::from(dash)).min(span);
            if a < b {
                painter.line_segment([point(a), point(b)], stroke);
            }
            offset += period;
        }
        phase = (phase + length.rem_euclid(period)).rem_euclid(period);
    }
}

fn draw_bars(painter: &Painter, model: &Model, settings: &Settings, m: &Mapper) {
    let (up, down) = if settings.red_up { (RED, GREEN) } else { (GREEN, RED) };
    let fps = model.bars();
    let i0 = model.fp_x.partition_point(|f| f.1 < m.x0);
    let i1 = model.fp_x.partition_point(|f| f.0 <= m.x1);
    if i1 <= i0 {
        return;
    }

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
    if slot >= FOOTPRINT_MIN_SLOT
        && draw_footprints(painter, model, settings, m, i0, i1, &center_px, slot)
    {
        return;
    }
    let half = (slot * 0.36).clamp(1.5, 30.0);
    let tick = model.result.tick_size.abs().max(1e-9);
    for i in i0..i1 {
        let fp = &fps[i];
        let color = if fp.close >= fp.open { up } else { down };
        let center = center_px(i);
        let (xa, xb) = (center - half, center + half);
        if settings.show_bar_value_area {
            draw_bar_value_area(painter, m, fp, xa, xb, tick);
        }
        painter.vline(center, m.sy(fp.high)..=m.sy(fp.low), Stroke::new(1.0, color));
        let top = m.sy(fp.open.max(fp.close));
        let bottom = m.sy(fp.open.min(fp.close)).max(top + 1.0);
        painter.rect(
            Rect::from_min_max(pos2(xa, top), pos2(xb, bottom)),
            0.0,
            with_alpha(color, 200),
            Stroke::new(1.0, color),
            StrokeKind::Middle,
        );
    }
}

/// Bars at least this many pixels apart are drawn as footprints (as in the web studio).
pub(crate) const FOOTPRINT_MIN_SLOT: f32 = 32.0;
const FOOTPRINT_TEXT: Color32 = Color32::from_rgb(225, 230, 240);
const BAR_POC: Color32 = Color32::from_rgb(240, 200, 80);
const BAR_VALUE_AREA: Color32 = Color32::from_rgb(110, 164, 235);
const BAR_VALUE_AREA_FRACTION: f64 = 0.70;

fn draw_bar_value_area(painter: &Painter, m: &Mapper, fp: &Footprint, left: f32, right: f32, tick: f64) {
    let levels: Vec<_> = fp.ladder.iter().map(|level| VpLevel {
        price: level.price,
        buy_vol: level.buy_vol,
        sell_vol: level.sell_vol,
    }).collect();
    if let Some(area) = volume_profile::value_area(&levels, BAR_VALUE_AREA_FRACTION) {
        let top = m.sy(area.vah + tick * 0.5);
        let bottom = m.sy(area.val - tick * 0.5).max(top + 1.0);
        painter.rect_filled(
            Rect::from_min_max(pos2(left, top), pos2(right, bottom)),
            0.0,
            with_alpha(BAR_VALUE_AREA, 45),
        );
        let stroke = Stroke::new(1.0, with_alpha(BAR_VALUE_AREA, 150));
        painter.hline(left..=right, top, stroke);
        painter.hline(left..=right, bottom, stroke);
    }
}

/// Footprint view: per traded price level a delta profile (left of the bar's middle, green /
/// red by sign) and a volume profile (right), the same shape at every zoom level; delta and
/// volume numbers appear once rows and bars are big enough. A thin candle at the bar's left edge
/// keeps open/close readable. POC and value area can be shown independently. Returns `false`
/// when the visible bars carry no ladders (the caller then draws candles).
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

        if settings.show_bar_value_area {
            draw_bar_value_area(painter, m, fp, cells_x0, cells_x0 + cells_w, tick);
        }
        let mut poc: Option<(f64, f64)> = None;
        for lv in &fp.ladder {
            let total = lv.buy_vol + lv.sell_vol;
            if settings.show_bar_poc && poc.is_none_or(|(_, v)| total > v) {
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
                crate::TOTAL_VOLUME_COLOR,
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
                Stroke::new(1.0, BAR_POC),
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

fn overlay_visible(settings: &Settings, group: Option<&str>, ungrouped: bool) -> bool {
    group.map_or(ungrouped, |name| !settings.hidden_overlay_groups.contains(name))
}

fn draw_boxes(painter: &Painter, model: &Model, settings: &Settings, m: &Mapper) {
    for b in &model.overlays.boxes {
        if b.x1 < m.x0 || b.x0 > m.x1
            || !overlay_visible(settings, b.group.as_deref(), settings.show_overlays)
        {
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

fn draw_big_trades(painter: &Painter, model: &Model, m: &Mapper, settings: &BigTradeSettings) {
    let opacity = settings.opacity();
    if opacity == 0 {
        return;
    }
    let from = model.xmap.to_ts(m.x0);
    let until = model.xmap.to_ts(m.x1);
    let clip = painter.clip_rect();
    let markers = model.big_trades.visible(from, until, model.replay_clock(), settings).filter_map(|marker| {
        let pos = m.pos(model.xmap.to_x(marker.ts_ns), marker.price);
        clip.contains(pos).then_some((marker, pos))
    });
    for (marker, pos) in big_trade_markers_for_display(markers, clip) {
        let radius = settings.radius(marker.volume);
        let color = with_alpha(settings.color(marker), opacity);
        let value = settings.show_value.then(|| tooltip_size(marker.volume));
        draw_big_trade_marker(painter, pos, radius, settings.shape, color, value);
    }
}

fn big_trade_markers_for_display<'a>(
    mut markers: impl Iterator<Item = (&'a Marker, Pos2)>,
    clip: Rect,
) -> Vec<(&'a Marker, Pos2)> {
    // Each circle and its label become multiple triangles. In an overview prefer the
    // strongest execution per screen cell so the amount of geometry stays bounded.
    const MAX_MARKERS: usize = 12_000;
    let first: Vec<_> = markers.by_ref().take(MAX_MARKERS + 1).collect();
    if first.len() <= MAX_MARKERS {
        return first;
    }
    let width = clip.width().max(1.0);
    let height = clip.height().max(1.0);
    let columns = ((MAX_MARKERS as f32 * width / height).sqrt() as usize).clamp(1, MAX_MARKERS);
    let rows = MAX_MARKERS / columns;
    let mut strongest = vec![None::<(&Marker, Pos2)>; rows * columns];
    for (marker, pos) in first.into_iter().chain(markers) {
        let x = (((pos.x - clip.left()) / width * columns as f32) as usize).min(columns - 1);
        let y = (((pos.y - clip.top()) / height * rows as f32) as usize).min(rows - 1);
        let cell = &mut strongest[y * columns + x];
        if cell.is_none_or(|(old, _)| marker.volume >= old.volume) {
            *cell = Some((marker, pos));
        }
    }
    let mut visible: Vec<_> = strongest.into_iter().flatten().collect();
    visible.sort_by_key(|(marker, _)| marker.ts_ns);
    visible
}

fn draw_big_trade_marker(
    painter: &Painter,
    pos: Pos2,
    radius: f32,
    shape: MarkerShape,
    color: Color32,
    value: Option<String>,
) {
    let (label_pos, label_size) = match shape {
        MarkerShape::Circle => {
            painter.circle_filled(pos, radius, color);
            (pos, egui::Vec2::splat(radius * 1.4))
        }
        MarkerShape::Square => {
            painter.rect_filled(Rect::from_center_size(pos, egui::Vec2::splat(radius * 2.0)), 0.0, color);
            (pos, egui::Vec2::splat(radius * 1.8))
        }
        MarkerShape::Triangle => {
            painter.add(Shape::convex_polygon(
                vec![pos + egui::vec2(0.0, -radius), pos + egui::vec2(-radius, radius), pos + egui::vec2(radius, radius)],
                color,
                Stroke::NONE,
            ));
            (pos + egui::vec2(0.0, radius * 0.48), egui::vec2(radius * 1.08, radius * 0.8))
        }
    };
    if let Some(value) = value {
        let font = FontId::proportional(11.0);
        let label = painter.layout_no_wrap(value.clone(), font.clone(), Color32::WHITE);
        let scale = (label_size.x / label.size().x).min(label_size.y / label.size().y).min(1.0);
        let clip = Rect::from_center_size(label_pos, label_size);
        painter.with_clip_rect(clip).text(
            label_pos,
            Align2::CENTER_CENTER,
            value,
            FontId::new(font.size * scale, font.family),
            Color32::WHITE,
        );
    }
}

fn draw_overlays(painter: &Painter, model: &Model, m: &Mapper, settings: &Settings) {
    let overlays = &model.overlays;
    let max_points = (m.rect.width() * 4.0) as usize;
    for line in overlays.lines.iter().filter(|_| settings.show_overlays) {
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
        if seg.from.0.max(seg.to.0) < m.x0 || seg.from.0.min(seg.to.0) > m.x1
            || !overlay_visible(settings, seg.group.as_deref(), settings.show_overlays)
        {
            continue;
        }
        let (a, b) = (m.pos(seg.from.0, seg.from.1), m.pos(seg.to.0, seg.to.1));
        let stroke = Stroke::new(1.4, seg.color);
        if seg.dashed {
            draw_dashes(painter, &[a, b], stroke, 6.0, 4.0);
        } else {
            painter.line_segment([a, b], stroke);
        }
        if !seg.text.is_empty() {
            painter.text(b, Align2::LEFT_BOTTOM, &seg.text, FontId::proportional(11.0), seg.color);
        }
    }
    let i0 = overlays.markers.partition_point(|mk| mk.x < m.x0);
    let i1 = overlays.markers.partition_point(|mk| mk.x <= m.x1);
    let markers = overlays.markers[i0..i1.max(i0)].iter().filter(|marker| {
        overlay_visible(settings, marker.group.as_deref(), settings.show_markers)
    });
    let visible = markers.clone().count();
    let step = (visible / 4000).max(1);
    // Shrink markers as they crowd, so a zoomed-out view is not buried under them.
    let radius = match visible {
        0..=200 => 5.0,
        201..=1000 => 3.0,
        _ => 1.5,
    };
    for marker in markers.step_by(step) {
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
    let pixels_per_point = painter.ctx().pixels_per_point();
    let width = QUOTE_WIDTH_PX / pixels_per_point;
    let mut options = painter.ctx().tessellation_options(|options| *options);
    options.feathering = true;
    options.feathering_size_in_pixels = QUOTE_FEATHER_PX;
    let mut tessellator = egui::epaint::Tessellator::new(pixels_per_point, options, [0, 0], Vec::new());
    tessellator.set_clip_rect(painter.clip_rect());
    let mut mesh = egui::Mesh::default();

    for (side, color) in [BID_LINE, ASK_LINE].into_iter().enumerate() {
        let stroke = Stroke::new(width, color);
        for segment in segments {
            for run in segment.split(|column| !column.prices[side].is_finite()) {
                let shape = match run {
                    [] => continue,
                    [column] => Shape::circle_filled(m.pos(column.x, column.prices[side]), width * 0.5, color),
                    _ => Shape::line(
                        run.iter().map(|column| m.pos(column.x, column.prices[side])).collect(),
                        stroke,
                    ),
                };
                tessellator.tessellate_shape(shape, &mut mesh);
            }
        }
    }

    if !mesh.is_empty() {
        painter.add(Shape::mesh(mesh));
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

fn bubble_clustering(settings: &Settings) -> data::BubbleClustering {
    data::BubbleClustering {
        strength: settings.bubble_aggregation,
        target: data::bubble_size_target(settings.bubble_size),
        spherical: settings.bubble_3d,
        delta: settings.bubble_delta,
    }
}

fn bubble_pixels(m: &Mapper) -> [f64; 2] {
    [
        f64::from(m.rect.width()) / (m.x1 - m.x0),
        f64::from(m.rect.height()) / (m.p1 - m.p0),
    ]
}

fn cluster_trade_columns(
    m: &Mapper,
    segments: &[Vec<QuoteColumn>],
    options: data::BubbleClustering,
) -> Vec<BubbleGroup> {
    let pixels_per_unit = bubble_pixels(m);
    segments.iter().map(|segment| {
        let trades = data::cluster_bubbles(
            segment.iter().filter_map(|column| column.bubble),
            pixels_per_unit,
            m.x0,
            options,
        );
        BubbleGroup::new(trades, None, options.delta)
    }).collect()
}

fn draw_trade_bubbles<'a>(
    painter: &Painter,
    m: &Mapper,
    groups: impl IntoIterator<Item = &'a BubbleGroup>,
    settings: &Settings,
    pointer: Option<Pos2>,
) -> Option<PlottedBubble> {
    let target = data::bubble_size_target(settings.bubble_size);
    let delta = settings.bubble_delta;
    let colors = if settings.bubble_colorblind { CYAN_VIOLET_BUBBLES } else { BOOKMAP_BUBBLES };
    let tolerance = BUBBLE_HOVER_TOLERANCE_PX / painter.ctx().pixels_per_point();
    let pointer = pointer.filter(|&pos| painter.clip_rect().contains(pos));
    let mut hovered: Option<(PlottedBubble, f32)> = None;
    let mut mesh = egui::Mesh::default();
    for group in groups {
        for &bubble in group.trades.iter().chain(&group.partial) {
            // In Delta mode Bookmap cancels matched buy/sell size before sending the dot
            // to the renderer, while the cluster center still uses total executed volume.
            let volume = bubble.clustering_volume(delta);
            if volume <= 0.0 || volume < settings.bubble_min_displayed_volume {
                continue;
            }
            let radius = data::bubble_radius(volume, group.largest, target, settings.bubble_3d);
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
            let buy_share = if delta {
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

fn bubble_tooltip(model: &Model, dot: PlottedBubble, delta: bool) -> String {
    let trade = dot.trade;
    let mut info = format!(
        "Trade volume  {}\nBuy {}   Sell {}\nPrice {:.digits$}",
        tooltip_size(trade.total()), tooltip_size(trade.buy), tooltip_size(trade.sell), trade.price,
        digits = model.price_decimals,
    );
    if delta {
        info.push_str(&format!("   Δ {}", tooltip_size(trade.buy - trade.sell)));
    }
    info
}

fn resting_liquidity_tooltip(model: &Model, m: &Mapper, pointer: Pos2, metric: HeatMetric) -> Option<String> {
    let (price, [bid, ask]) = model.resting_liquidity_at(m.x_at(pointer.x), m.p_at(pointer.y))?;
    let visible_size = match metric {
        HeatMetric::Total => bid + ask,
        HeatMetric::Bid => bid,
        HeatMetric::Ask => ask,
    };
    if visible_size <= 0.0 {
        return None;
    }
    Some(format!(
        "Resting liquidity @ {:.digits$}\nBid {}   Ask {}",
        price,
        tooltip_size(bid),
        tooltip_size(ask),
        digits = model.price_decimals,
    ))
}

fn heatmap_tooltip(
    state: &ChartState,
    model: &Model,
    m: &Mapper,
    pointer: Pos2,
    metric: HeatMetric,
) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(cache) = &state.heat {
        if let Some(depth) = cache.depth_cells.as_ref().filter(|_| cache.key.studies.active()) {
            if let Some(value) = heat_cell(&cache.key.spec, &depth.cells, m, pointer) {
                lines.push(format!("Filtered resting liquidity  {}", tooltip_size(f64::from(value))));
            }
        } else if let Some(info) = resting_liquidity_tooltip(model, m, pointer, metric) {
            lines.push(info);
        }
    }
    if let Some(cache) = &state.flow_heat {
        if cache.key.show_net {
            if let Some(value) = heat_cell(&cache.key.heat.spec, &cache.net, m, pointer) {
                lines.push(format!("Net liquidity added (5 s)  {}", tooltip_size(f64::from(value))));
            }
        }
        for study in heatmap_studies::MARKER_STUDIES {
            let marker = cache.markers.iter().filter(|marker| marker.study == study)
                .map(|marker| (flow_marker_pos(model, m, marker).distance_sq(pointer), marker))
                .filter(|(distance, _)| *distance <= 64.0)
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, marker)| marker);
            if let Some(marker) = marker {
                let mut line = format!("{} (5 s)  {}", study.label(), flow_marker_amount(marker));
                if let Some([start, end]) = marker.resting {
                    line.push_str(&format!("\nResting size  {} → {}", tooltip_size(start), tooltip_size(end)));
                }
                lines.push(line);
            }
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn heat_cell(spec: &RasterSpec, cells: &[f32], m: &Mapper, pointer: Pos2) -> Option<f32> {
    let col = (m.x_at(pointer.x) / spec.col_w).floor() as i64 - spec.c_lo;
    let price = m.p_at(pointer.y);
    let row = spec.k_hi - (price / spec.row_p).round() as i64;
    if !(0..spec.cols as i64).contains(&col) || !(0..spec.rows as i64).contains(&row) {
        return None;
    }
    let value = cells[row as usize * spec.cols + col as usize];
    (value != 0.0 && value.is_finite()).then_some(value)
}

fn tooltip_size(value: f64) -> String {
    format!("{value:.8}").trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn draw_chart_tooltip(painter: &Painter, pointer: Pos2, info: String) {
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
        draw_dashes(painter, &[entry, exit], Stroke::new(width, color), 5.0, 3.0);
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
    hover_trade_min: Option<f64>,
    price_axis_left: f32,
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
        pos2(price_axis_left + 4.0, pos.y),
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
                let volume = column.bubble.map(|bubble| [bubble.buy, bubble.sell]).or_else(|| {
                    let (from, until) = column.trade_bounds?;
                    model.tape.as_ref()?.volume_between(from, until, hover_trade_min?)
                });
                if let Some([buy, sell]) = volume {
                    info.push_str(&format!("   buy {}  sell {}", format_size(buy), format_size(sell)));
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
    let depth_width = replay_depth_width(model, outer.width());
    let plot = Rect::from_min_max(
        pos2(outer.left(), outer.top() + 2.0),
        pos2(outer.right() - AXIS_W - depth_width, outer.bottom() - 2.0),
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
    fn zoomed_trade_connector_generates_only_visible_diagonal_dashes() {
        let view = Rect::from_min_max(pos2(0.0, 0.0), pos2(800.0, 400.0));
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(view), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            let painter = ui.painter().with_clip_rect(view);
            let stroke = Stroke::new(1.5, GAIN);
            draw_dashes(&painter, &[pos2(-200_000_000.0, -100_000_000.0), pos2(200_000_000.0, 100_000_000.0)], stroke, 5.0, 3.0);
            draw_dashes(&painter, &[pos2(-200_000_000.0, -200.0), pos2(200_000_000.0, -200.0)], stroke, 5.0, 3.0);
        });
        output.textures_delta.clear();
        let points: Vec<_> = output.shapes.iter().filter_map(|shape| {
            match shape.shape {
                Shape::LineSegment { points, stroke } if stroke.color == GAIN => Some(points),
                _ => None,
            }
        }).collect();
        assert!((110..=114).contains(&points.len()));
        assert!(points.into_iter().flatten().all(|point| view.expand(1.5).contains(point)));
        let indices: usize = ctx.tessellate(output.shapes, output.pixels_per_point).iter()
            .filter_map(|shape| match &shape.primitive {
                egui::epaint::Primitive::Mesh(mesh) => Some(mesh.indices.len()),
                _ => None,
            }).sum();
        assert!(indices < 4_000);
    }

    #[test]
    fn replay_heat_uploads_only_changed_columns_across_texture_wrap() {
        let second = data::SEC_NS;
        let axis = data::TimeAxis::build(vec![(0, 18 * second - 1), (21 * second, 100 * second)], 0);
        let ctx = egui::Context::default();
        let mut key = HeatKey {
            spec: RasterSpec { c_lo: 0, cols: 32, col_w: 1.0, k_hi: 402, rows: 5, row_p: 0.25 },
            metric: HeatMetric::Total, studies: Studies::default(), contrast: 2.0,
            smooth: true, sigma_rows: 0.5 / 21.0, low_pct: 0.0, high_pct: 99.0,
            replay_ns: Some(16 * second), depth_window: None,
        };
        // A 0.5 px Gaussian on 21 px price rows must keep the same sampling as Off.
        let pixels_per_price = 21.0 / key.spec.row_p;
        let off = HeatKey { sigma_rows: 0.0, ..key.clone() };
        let repeat = key.row_repeat(pixels_per_price);
        assert_eq!(repeat, 21);
        assert_eq!(repeat, off.row_repeat(pixels_per_price));
        let price_only = HeatKey { smooth: false, ..key.clone() };
        assert_eq!(price_only.row_repeat(pixels_per_price), 21);
        assert_eq!(price_only.texture_options(false), TextureOptions::NEAREST);
        assert_eq!(price_only.texture_options(true), TextureOptions::NEAREST_REPEAT);
        let raster = |key: &HeatKey| {
            let times = market_history::raster_times(&key.spec, &axis, key.replay_ns);
            let cells = (0..key.spec.rows).flat_map(|row| {
                times.iter().map(move |time| time.map_or(0.0, |(from, until)| {
                    ((from / second) % 23 + row as i64 + 1) as f32 * (until - from) as f32 / second as f32
                }))
            }).collect();
            DepthRaster { times, cells }
        };
        let groups = |key: &HeatKey| {
            liquidity_colors::column_groups(&key.spec, &axis, key.replay_ns.map(|ns| axis.to_x(ns)))
        };
        let filtered = |key: &HeatKey, depth: &DepthRaster, groups: &[Option<usize>]| {
            let mut cells = depth.cells.clone();
            liquidity_colors::smooth_time(&mut cells, key.spec.cols, groups);
            liquidity_colors::price_pixels(&cells, key.spec.cols, repeat, key.sigma_rows).0.into_owned()
        };
        let initial = raster(&key);
        let initial_groups = groups(&key);
        let scale = ColorScale::from_cells(&filtered(&key, &initial, &initial_groups), 0.0, 99.0, 2.0);
        let ring = ReplayHeatTexture { first_column: -1, width: 35, repeat, has_colors: true, groups: Some(initial_groups) };
        let options = key.texture_options(true);
        let image = heat_image(&filtered(&key, &initial, ring.groups.as_deref().unwrap()), 32, 1, &scale, Some(ring.width));
        let mut pixels = image.pixels.clone();
        let texture = ctx.load_texture("test-replay-heat", image, options);
        let mut cache = HeatCache { key: key.clone(), texture, depth_cells: Some(initial), scale, replay_texture: Some(ring) };
        ctx.tex_manager().write().take_delta().clear();

        for step in 1..=86 {
            key.spec.c_lo = step / 2;
            key.replay_ns = Some(16 * second + step * (second / 2));
            let depth = raster(&key);
            let groups = groups(&key);
            update_replay_heat(&mut cache, &key, &depth, Some(&groups), options);
            let ring = cache.replay_texture.as_mut().unwrap();
            let mut delta = ctx.tex_manager().write().take_delta();
            let mut uploaded = 0;
            for (id, patch) in delta.set.iter().flat_map(|(id, patches)| {
                patches.iter().map(move |patch| (id, patch))
            }) {
                assert_eq!(*id, cache.texture.id());
                let [start, 0] = patch.pos.expect("only partial texture uploads") else { panic!("unexpected row offset") };
                let egui::ImageData::Color(image) = &patch.image;
                let width = image.width();
                uploaded += width;
                for (row, patch) in pixels.chunks_exact_mut(ring.width).zip(image.pixels.chunks_exact(width)) {
                    row[start..start + width].copy_from_slice(patch);
                }
            }
            delta.clear();
            assert!(uploaded <= 10, "step {step} uploaded {uploaded} columns");

            let expected = heat_image(&filtered(&key, &depth, &groups), key.spec.cols, 1, &cache.scale, Some(ring.width));
            for (actual, expected) in pixels.chunks_exact(ring.width).zip(expected.pixels.chunks_exact(ring.width)) {
                for (col, &color) in expected[..key.spec.cols + 2].iter().enumerate() {
                    assert_eq!(actual[ring.slot(key.spec.c_lo + col as i64 - 1)], color, "step {step}, column {col}");
                }
            }
            cache.key = key.clone();
            cache.depth_cells = Some(depth);
            ring.groups = Some(groups);
        }
    }

    #[test]
    fn replay_depth_cache_matches_full_raster_when_scrolling_seeking_and_zooming() {
        use accelerando_core::{prepare_market_history, DataSource, OrderFlowEvent, Side};

        struct DepthSource(Vec<OrderFlowEvent>);
        impl DataSource for DepthSource {
            fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
                Box::new(self.0.into_iter())
            }
        }
        let second = data::SEC_NS;
        let events = (0..100).map(|i| OrderFlowEvent::SetLevel {
            ts_ns: i * second / 10,
            price: 100.0 + (i % 3) as f64 * 0.25,
            size: (i % 11) as f64 * 2.0,
            side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
        });
        let (_, depth) = prepare_market_history(Box::new(DepthSource(events.collect())));
        let axis = data::TimeAxis::build(vec![(0, 4 * second - 1), (6 * second, 10 * second - 1)], 0);
        let spec = RasterSpec { c_lo: 0, cols: 12, col_w: 0.5, k_hi: 402, rows: 3, row_p: 0.25 };
        let mut key = HeatKey {
            spec, metric: HeatMetric::Total, studies: Studies::default(), contrast: 4.0,
            smooth: true, sigma_rows: 0.5, low_pct: 2.0, high_pct: 99.5,
            replay_ns: Some(2 * second + second / 4), depth_window: None,
        };
        for filter_levels in [false, true] {
            key.spec = spec;
            key.studies = Studies { filter_levels, min_size: 6, hold_ms: 250, ..Studies::default() };
            let mut old_key = key.clone();
            let mut old = DepthRaster::build(&old_key, &axis, &depth, None);
            for (left, clock, width) in [
                (0, 2 * second + second / 3, 0.5),
                (1, 3 * second, 0.5),
                (4, 7 * second + second / 2, 0.5),
                (0, second / 2, 0.5),
                (0, 3 * second, 0.25),
            ] {
                key.spec.c_lo = left;
                key.spec.col_w = width;
                key.replay_ns = Some(clock);
                let updated = DepthRaster::build(&key, &axis, &depth, Some((&old_key, &old)));
                let fresh = DepthRaster::build(&key, &axis, &depth, None);
                assert_eq!(updated.times, fresh.times);
                assert_eq!(updated.cells, fresh.cells);
                old_key = key.clone();
                old = updated;
            }
        }
    }

    #[test]
    fn zoom_preview_keeps_earlier_replay_data_and_rejects_stale_results() {
        let mut key = HeatKey {
            spec: RasterSpec { c_lo: 0, cols: 32, col_w: 1.0, k_hi: 402, rows: 5, row_p: 0.25 },
            metric: HeatMetric::Total, studies: Studies::default(), contrast: 2.0,
            smooth: true, sigma_rows: 0.5, low_pct: 0.0, high_pct: 99.0,
            replay_ns: Some(16 * data::SEC_NS), depth_window: None,
        };
        let old = key.clone();
        key.spec.col_w = 0.5;
        key.replay_ns = Some(17 * data::SEC_NS);
        assert!(key.can_preview(&old));
        assert!(!key.accepts(&old));
        let mut ready = key.clone();
        ready.replay_ns = old.replay_ns;
        assert!(key.accepts(&ready));
        assert!(!ready.can_preview(&key));
        let mut live = old;
        live.replay_ns = None;
        assert!(!key.can_preview(&live));
    }

    #[test]
    fn heat_preview_fills_all_four_exposed_sides_without_covering_detail() {
        let view = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 80.0));
        let detail = Rect::from_min_max(pos2(10.0, 20.0), pos2(70.0, 65.0));
        let exposed = uncovered_heat_rects(view, detail);
        assert!(exposed.iter().all(Rect::is_positive));
        let area = |rect: &Rect| rect.width() * rect.height();
        assert_eq!(exposed.iter().map(area).sum::<f32>() + area(&detail), area(&view));
        for (i, rect) in exposed.iter().enumerate() {
            assert!(view.contains_rect(*rect));
            assert!(!rect.intersect(detail).is_positive());
            assert!(exposed[..i].iter().all(|other| !rect.intersect(*other).is_positive()));
        }
    }

    #[test]
    fn wide_heat_preview_preserves_price_bands_at_screen_resolution() {
        let mapper = Mapper {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(1645.0, 905.0)),
            x0: 0.0, x1: 20.25 * 3600.0, p0: 7434.0, p1: 7730.0,
        };
        let detail = RasterSpec::new(mapper.x0, mapper.x1, mapper.rect.width(),
            mapper.p0, mapper.p1, mapper.rect.height(), 0.25);
        let spec = preview_spec(&mapper, detail.row_p);
        assert!(heat_rect(&mapper, &spec).contains_rect(mapper.rect));
        assert!(spec.cols <= PREVIEW_TIME_COLUMNS as usize + 2);
        assert_eq!(detail.row_p, spec.row_p);
    }

    #[test]
    fn vp_legend_only_counts_backtest_profiles() {
        for has_profile in [false, true] {
            let mut bar = accelerando_core::testkit::bar(0).price(100.0).build();
            if has_profile {
                bar.plots.push(accelerando_core::Plot::VolumeProfile {
                    id: "test_vp".into(),
                    levels: vec![accelerando_core::VpLevel {
                        price: 100.0,
                        buy_vol: 5.0,
                        sell_vol: 2.0,
                    }],
                    span: 1,
                });
            }
            let result = accelerando_core::BacktestResult {
                metrics: Default::default(),
                trades: Vec::new(),
                equity: Vec::new(),
                footprints: vec![bar],
                liquidity_heatmap: Default::default(),
                series: Vec::new(),
                counters: Default::default(),
                tick_size: 0.25,
                multiplier: 50.0,
            };
            let mut viewer = crate::Viewer::new(result, Default::default(), &[], None, Default::default());
            viewer.vp.items.push(crate::vp::ManualVp { t0: 0, t1: 0 });
            let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(680.0, 420.0));
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = egui::Context::default().run_ui(input, |ui| {
                indicator_legend(ui, screen, &viewer.model, &mut viewer.settings);
            });
            output.textures_delta.clear();
            let vp_labels: Vec<_> = output.shapes.iter().filter_map(|entry| match &entry.shape {
                Shape::Text(text) if text.galley.text().starts_with("Volume profile") => {
                    Some(text.galley.text())
                }
                _ => None,
            }).collect();
            if has_profile {
                assert_eq!(vp_labels, vec!["Volume profile (1)"]);
            } else {
                assert!(vp_labels.is_empty());
            }
        }
    }

    #[test]
    fn replay_depth_renders_the_current_book_in_both_modes() {
        use accelerando_core::{LiquidityHeatmap, LiquiditySnapshot};

        let second = data::SEC_NS;
        let bars: Vec<_> = (0..3)
            .map(|i| accelerando_core::testkit::bar(0).minute(i).price(100.0).build())
            .collect();
        let start = bars[0].ts_first_ns;
        let level = |price, bid_size, ask_size| LiquidityLevel { price, bid_size, ask_size };
        let result = accelerando_core::BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: bars,
            liquidity_heatmap: LiquidityHeatmap {
                interval_ns: second,
                snapshots: vec![
                    LiquiditySnapshot { ts_ns: start, levels: vec![
                        level(99.75, 8.0, 0.0), level(100.0, 20.0, 0.0), level(120.0, 0.0, 200.0),
                    ] },
                    LiquiditySnapshot { ts_ns: start + second, levels: vec![level(100.25, 0.0, 35.0)] },
                    LiquiditySnapshot { ts_ns: start + 2 * second, levels: Vec::new() },
                ],
            },
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        };
        let options = crate::ViewerOptions { axis_mode: Some(AxisMode::Time), ..Default::default() };
        let mut viewer = crate::Viewer::new(result, options, &[], None, Default::default());
        viewer.chart.show_range(0.0, 4.0);
        viewer.chart.auto_y = false;
        (viewer.chart.p0, viewer.chart.p1) = (90.0, 110.0);
        viewer.settings.show_heat = true;
        viewer.settings.price_filter = crate::liquidity_colors::PriceFilter::Off;
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(680.0, 420.0));
        let ctx = egui::Context::default();
        let mut draw = |mode, clock| {
            let ts_ns = start + clock;
            if viewer.model.replay_clock().is_none_or(|previous| previous > ts_ns) {
                viewer.model.start_replay(0);
            }
            viewer.model.replay_apply(&[crate::Tick { ts_ns, price: 100.0, size: 1.0, side: 1 }], ts_ns);
            viewer.settings.mode = mode;
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                price_panel(ui, &viewer.model, &mut viewer.settings, &mut viewer.chart, None, &mut VpTool::default(), false);
            });
            output.textures_delta.clear();
            output.shapes
        };

        let plot_right = screen.right() - AXIS_W - REPLAY_DEPTH_W;
        let axis_left = screen.right() - AXIS_W;
        let heat_rows = |shapes: &[egui::epaint::ClippedShape]| {
            shapes.iter().filter_map(|entry| match &entry.shape {
                Shape::Rect(rect) if rect.rect.left() > 100.0 && rect.rect.width() > 100.0
                    && rect.rect.right() <= plot_right && rect.rect.height() < 100.0
                    && rect.fill != Color32::TRANSPARENT => Some(rect.rect.center().y),
                _ => None,
            }).collect::<Vec<_>>()
        };
        let depth_bars = |shapes: &[egui::epaint::ClippedShape]| {
            shapes.iter().filter_map(|entry| match &entry.shape {
                Shape::Rect(rect) if CYAN_VIOLET_BUBBLES.contains(&rect.fill) => {
                    assert!(rect.rect.left() >= plot_right && rect.rect.right() <= axis_left);
                    Some((rect.fill, rect.rect.center().y))
                }
                _ => None,
            }).collect::<Vec<_>>()
        };
        for mode in [ChartMode::Heatmap, ChartMode::Bar] {
            let bid = draw(mode, second);
            let ask = draw(mode, 2 * second);
            let cleared = draw(mode, 3 * second);
            let bid_bars = depth_bars(&bid);
            let ask_bars = depth_bars(&ask);
            assert_eq!((bid_bars.len(), ask_bars.len()), (1, 1));
            assert_eq!(bid_bars[0].0, CYAN_VIOLET_BUBBLES[0]);
            assert_eq!(ask_bars[0].0, CYAN_VIOLET_BUBBLES[1]);
            assert!(ask_bars[0].1 < bid_bars[0].1);
            assert!(depth_bars(&cleared).is_empty());
            for (shapes, size) in [(&bid, "20"), (&ask, "35")] {
                assert!(shapes.iter().any(|shape| matches!(&shape.shape, Shape::Text(text)
                    if text.galley.text() == size && (plot_right..axis_left).contains(&text.pos.x))));
            }
            assert!(bid.iter().any(|shape| matches!(&shape.shape, Shape::Text(text)
                if text.galley.text() == "100.00" && text.pos.x >= axis_left)));
            if mode == ChartMode::Heatmap {
                for (shapes, y, count) in [(&bid, bid_bars[0].1, 2), (&ask, ask_bars[0].1, 1)] {
                    let rows = heat_rows(shapes);
                    assert_eq!(rows.len(), count);
                    assert!(rows.iter().any(|row| (row - y).abs() < 0.01));
                }
                assert!(heat_rows(&cleared).is_empty());
            }
        }
    }

    #[test]
    fn bar_replay_draws_the_partial_footprint_beyond_the_cursor() {
        let bars = (0..3)
            .map(|i| accelerando_core::testkit::bar(0).minute(i).price(100.0).build())
            .collect();
        let result = accelerando_core::BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: bars,
            liquidity_heatmap: Default::default(),
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        };
        let options = crate::ViewerOptions { axis_mode: Some(AxisMode::Time), ..Default::default() };
        let mut viewer = crate::Viewer::new(result, options, &[], None, Default::default());
        viewer.model.start_replay(0);
        let ts_ns = viewer.model.full_bars()[0].ts_first_ns;
        viewer.model.replay_apply(&[crate::Tick { ts_ns, price: 100.0, size: 10.0, side: 1 }], ts_ns);
        assert_eq!(viewer.model.bars().len(), 1);

        let mut state = ChartState::new(viewer.model.xmap.total(), AxisMode::Time);
        state.show_range(0.0, 120.0);
        state.auto_y = false;
        (state.p0, state.p1) = (99.0, 101.0);
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(680.0, 420.0));
        let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
        let mut output = egui::Context::default().run_ui(input, |ui| {
            price_panel(ui, &viewer.model, &mut viewer.settings, &mut state, None, &mut VpTool::default(), false);
        });
        output.textures_delta.clear();
        let footprints: Vec<_> = output.shapes.iter().filter_map(|entry| {
            match &entry.shape {
                Shape::Rect(rect) if rect.fill == crate::TOTAL_VOLUME_COLOR => Some((rect.rect, entry.clip_rect)),
                _ => None,
            }
        }).collect();
        assert_eq!(footprints.len(), 1);
        let (volume, clip) = footprints[0];
        assert!(volume.left() > 50.0);
        assert!(clip.contains_rect(volume));
    }

    #[test]
    fn chart_gestures_keep_axis_zoom_and_panning_independent() {
        let ctx = egui::Context::default();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(600.0 + AXIS_W, 400.0 + AXIS_H));
        let plot = Rect::from_min_max(outer.min, pos2(600.0, 400.0));
        let mut state = ChartState::new(300.0, AxisMode::Time);
        state.show_range(50.0, 150.0);
        state.p0 = 90.0;
        state.p1 = 110.0;
        state.follow_replay = true;
        let replay_x = Some(135.0);
        let mut frame = |events| {
            let input = egui::RawInput { screen_rect: Some(outer), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                let area = ui.allocate_rect(outer, Sense::hover());
                let response = ui.interact(plot, area.id.with("plot"), Sense::click_and_drag());
                let price_axis = Rect::from_min_max(plot.right_top(), pos2(outer.right(), plot.bottom()));
                let time_axis = Rect::from_min_max(plot.left_bottom(), pos2(plot.right(), outer.bottom()));
                let price = ui.interact(price_axis, area.id.with("price"), Sense::drag());
                let time = ui.interact(time_axis, area.id.with("time"), Sense::drag());
                handle_input(&response, &mut state, true, 0.25, replay_x);
                handle_axis_input(&price, &time, plot, &mut state, 0.25, replay_x);
            });
            output.textures_delta.clear();
            (state.x0, state.x1, state.p0, state.p1, state.auto_y, state.follow_replay)
        };
        let wheel = |delta, phase| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            phase,
            source: egui::MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::NONE,
        };
        let move_to = |x, y| egui::Event::PointerMoved(pos2(x, y));
        let button = |x, y, pressed| egui::Event::PointerButton {
            pos: pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![move_to(650.0, 100.0)]);
        assert_eq!(
            frame(vec![wheel(egui::vec2(120.0, 0.0), egui::TouchPhase::Move)]),
            (50.0, 150.0, 90.0, 110.0, true, true),
        );
        frame(vec![wheel(egui::Vec2::ZERO, egui::TouchPhase::End)]);
        let (x0, x1, _, high, auto_y, follow) = frame(vec![
            wheel(egui::vec2(0.0, 120.0), egui::TouchPhase::Move),
        ]);
        assert_eq!((x0, x1), (50.0, 150.0));
        assert!(!auto_y);
        assert!(follow);
        assert!(high < 110.0);
        frame(vec![wheel(egui::Vec2::ZERO, egui::TouchPhase::End)]);

        frame(vec![button(650.0, 100.0, true)]);
        assert_eq!(frame(vec![move_to(630.0, 100.0)]).3, high);
        let (left, right, low, new_high, _, follow) = frame(vec![move_to(630.0, 80.0)]);
        assert_eq!((left, right), (x0, x1));
        assert!(follow);
        assert!(new_high < high);
        frame(vec![button(630.0, 80.0, false)]);

        frame(vec![move_to(300.0, 411.0)]);
        frame(vec![button(300.0, 411.0, true)]);
        let (left, right, new_low, high, _, follow) = frame(vec![move_to(250.0, 411.0)]);
        assert!(right - left < x1 - x0);
        assert!((left + (right - left) * REPLAY_FRACTION - replay_x.unwrap()).abs() < 1e-9);
        assert_eq!((new_low, high), (low, new_high));
        assert!(follow);
        frame(vec![button(250.0, 411.0, false)]);

        frame(vec![move_to(300.0, 100.0)]);
        frame(vec![button(300.0, 100.0, true)]);
        let (panned_left, panned_right, .., follow) = frame(vec![move_to(260.0, 100.0)]);
        assert!(panned_left > left && panned_right > right);
        assert!(!follow);
        frame(vec![button(260.0, 100.0, false)]);

        let (left, right, low, high, _, follow) = frame(vec![
            wheel(egui::vec2(0.0, 120.0), egui::TouchPhase::Move),
        ]);
        assert!(right - left < panned_right - panned_left);
        assert_eq!((low, high), (new_low, new_high));
        assert!(!follow);
    }

    #[test]
    fn replay_follows_each_update_even_at_data_boundaries() {
        let mut state = ChartState::new(300.0, AxisMode::Time);
        state.show_range(0.0, 100.0);
        state.follow_replay = true;
        for x in [0.0, 0.5, 45.0, 299.5, 300.0] {
            state.sync_replay(Some(x));
            assert!((state.x0 + 100.0 * REPLAY_FRACTION - x).abs() < 1e-9);
            assert!((state.x1 - state.x0 - 100.0).abs() < 1e-9);
        }
        state.follow_replay = false;
        let range = (state.x0, state.x1);
        state.sync_replay(Some(100.0));
        assert_eq!((state.x0, state.x1), range);
    }

    #[test]
    fn big_trade_shapes_keep_the_optional_volume_label_inside() {
        let pos = pos2(80.0, 80.0);
        for shape in [MarkerShape::Circle, MarkerShape::Square, MarkerShape::Triangle] {
            let ctx = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(160.0, 160.0))),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                draw_big_trade_marker(ui.painter(), pos, 20.0, shape, Color32::RED, Some("250".into()));
            });
            output.textures_delta.clear();
            assert!(output.shapes.iter().any(|paint| match (&paint.shape, shape) {
                (Shape::Circle(circle), MarkerShape::Circle) => circle.center == pos,
                (Shape::Rect(rect), MarkerShape::Square) => rect.rect.center() == pos,
                (Shape::Path(path), MarkerShape::Triangle) => path.points.len() == 3,
                _ => false,
            }));
            assert!(output.shapes.iter().any(|paint| {
                matches!(&paint.shape, Shape::Text(text) if text.galley.job.text == "250")
                    && Rect::from_center_size(pos, egui::Vec2::splat(40.0)).contains_rect(paint.clip_rect)
            }));
        }
    }

    #[test]
    fn viewer_enables_big_trades_and_redraws_at_the_current_threshold() {
        let mut bar = accelerando_core::Footprint::seed(0, 100.0);
        for (price, volume, color) in [(99.5, "505", "#ff6d6d"), (100.5, "510", "#6d6dff")] {
            bar.plots.push(accelerando_core::Plot::Marker {
                price,
                shape: "circle".into(),
                color: color.into(),
                text: volume.into(),
                text_dx: None,
                text_dy: None,
                group: Some("big_trades".into()),
            });
        }
        let result = accelerando_core::BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: vec![bar],
            liquidity_heatmap: Default::default(),
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        };
        let mut viewer = crate::Viewer::new(result, Default::default(), &[], None, Default::default());
        assert!(viewer.settings.big_trades.visible);
        assert!(!viewer.settings.show_markers);
        let mapper = Mapper {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 300.0)),
            x0: -1.0,
            x1: 1.0,
            p0: 99.0,
            p1: 101.0,
        };
        let drawn = |viewer: &crate::Viewer, mapper: &Mapper| {
            let ctx = egui::Context::default();
            let input = egui::RawInput { screen_rect: Some(mapper.rect), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                draw_big_trades(ui.painter(), &viewer.model, mapper, &viewer.settings.big_trades);
            });
            output.textures_delta.clear();
            output.shapes.iter().filter(|shape| matches!(shape.shape, Shape::Circle(_))).count()
        };
        assert_eq!(drawn(&viewer, &mapper), 2);
        let zoomed = Mapper { x0: -0.5, x1: 0.5, ..mapper };
        assert_eq!(drawn(&viewer, &zoomed), 2);
        viewer.settings.big_trades.min_volume = 506.0;
        assert_eq!(drawn(&viewer, &zoomed), 1);
    }

    #[test]
    fn dense_big_trades_draw_the_largest_execution_per_screen_cell() {
        let mut bar = accelerando_core::Footprint::seed(0, 100.0);
        for (price, volume) in (0..30_000).map(|i| (if i % 2 == 0 { 99.5 } else { 100.5 }, i + 25)) {
            bar.plots.push(accelerando_core::Plot::Marker {
                price,
                shape: "circle".into(),
                color: "#ff6d6d".into(),
                text: volume.to_string(),
                text_dx: None,
                text_dy: None,
                group: Some("big_trades".into()),
            });
        }
        let overlay = crate::big_trades::BigTradeOverlay::build(None, &[bar]);
        let settings = BigTradeSettings::default();
        let clip = Rect::from_min_size(Pos2::ZERO, egui::vec2(1920.0, 1080.0));
        let candidates = overlay.visible(0, 0, None, &settings).map(|marker| {
            (marker, pos2(500.0, if marker.price < 100.0 { 700.0 } else { 400.0 }))
        });
        let visible = big_trade_markers_for_display(candidates, clip);
        let mut volumes: Vec<_> = visible.iter().map(|(marker, _)| marker.volume as usize).collect();
        volumes.sort_unstable();
        assert_eq!(volumes, [30_023, 30_024]);
    }

    #[test]
    fn bar_value_area_and_poc_are_independent() {
        let fp = accelerando_core::testkit::bar(0)
            .level(99.75, 1.0, 0.0)
            .level(100.0, 6.0, 0.0)
            .level(100.25, 3.0, 0.0)
            .build();
        let result = accelerando_core::BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: vec![fp],
            liquidity_heatmap: Default::default(),
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        };
        let mut viewer = crate::Viewer::new(result, Default::default(), &[], None, Default::default());
        let mut m = Mapper {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 300.0)),
            x0: 0.0,
            x1: viewer.model.xmap.total(),
            p0: 99.0,
            p1: 101.0,
        };
        let draw = |viewer: &crate::Viewer, m: &Mapper| {
            let ctx = egui::Context::default();
            let input = egui::RawInput { screen_rect: Some(m.rect), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                draw_bars(ui.painter(), &viewer.model, &viewer.settings, m);
            });
            output.textures_delta.clear();
            let mut poc = false;
            let mut value_area = None;
            for shape in output.shapes {
                if let Shape::Rect(rect) = shape.shape {
                    poc |= rect.stroke.color == BAR_POC;
                    if rect.fill == with_alpha(BAR_VALUE_AREA, 45) {
                        value_area = Some((rect.rect.top(), rect.rect.bottom()));
                    }
                }
            }
            (poc, value_area)
        };
        assert_eq!(draw(&viewer, &m), (false, None));
        viewer.settings.show_bar_poc = true;
        assert_eq!(draw(&viewer, &m), (true, None));
        viewer.settings.show_bar_poc = false;
        viewer.settings.show_bar_value_area = true;
        let expected = (false, Some((m.sy(100.375), m.sy(99.875))));
        assert_eq!(draw(&viewer, &m), expected);
        viewer.settings.show_bar_poc = true;
        assert_eq!(draw(&viewer, &m), (true, expected.1));
        m.x1 *= 20.0; // Candles, below the footprint threshold.
        assert_eq!(draw(&viewer, &m), expected);
    }

    #[test]
    fn heatmap_layers_render_together_and_report_overlapping_studies() {
        use accelerando_core::{prepare_market_history, BacktestResult, DataSource, OrderFlowEvent, Side};

        struct DepthSource(Vec<OrderFlowEvent>);
        impl DataSource for DepthSource {
            fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
                Box::new(self.0.into_iter())
            }
        }

        let level = |ts_ns, size| OrderFlowEvent::SetLevel {
            ts_ns, price: 100.0, size, side: Side::Buy,
        };
        let (data, depth) = prepare_market_history(Box::new(DepthSource(vec![
            OrderFlowEvent::Contract { tick_size: 0.25, multiplier: 50.0 },
            level(100_000_000, 10.0),
            level(200_000_000, 25.0),
            OrderFlowEvent::Trade {
                ts_ns: 300_000_000, price: 100.25, size: 1.0, aggressor: Side::Buy,
            },
            level(500_000_000, 18.0),
        ])));
        let result = BacktestResult {
            metrics: Default::default(), trades: Vec::new(), equity: Vec::new(),
            footprints: vec![accelerando_core::testkit::bar(0).minute(0).price(100.0).build()],
            liquidity_heatmap: Default::default(), series: Vec::new(),
            counters: Default::default(), tick_size: 0.25, multiplier: 50.0,
        };
        let tape = crate::market_history::TradeTape::new(std::sync::Arc::new(data.unwrap()), &[]);
        let mut viewer = crate::Viewer::new(result, Default::default(), &[], Some(tape), depth.into());
        let m = Mapper {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 300.0)),
            x0: 0.0, x1: 1.0, p0: 99.0, p1: 101.0,
        };
        let ctx = egui::Context::default();
        viewer.settings.heat_layers.fill(true);
        viewer.settings.flow_marker_minimum = [5; 3];
        viewer.settings.price_filter = crate::liquidity_colors::PriceFilter::Gaussian;
        let draw = |viewer: &mut crate::Viewer, m: &Mapper| {
            let input = egui::RawInput { screen_rect: Some(m.rect), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                draw_heatmap(ui.ctx(), ui.painter(), &viewer.model, &viewer.settings, &mut viewer.chart, m);
            });
            output.textures_delta.clear();
        };
        draw(&mut viewer, &m);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while (viewer.chart.heat.is_none() || viewer.chart.flow_heat.is_none())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            draw(&mut viewer, &m);
        }
        assert!(viewer.chart.heat.is_some());
        let flow = viewer.chart.flow_heat.as_ref().unwrap();
        assert!(flow.texture.is_some());
        assert_eq!(flow.markers.len(), 2);
        let added = flow.markers.iter().find(|marker| marker.study == Study::Added).unwrap();
        let pos = flow_marker_pos(&viewer.model, &m, added);
        let added_tooltip = heatmap_tooltip(&viewer.chart, &viewer.model, &m, pos, HeatMetric::Total).unwrap();
        assert!(added_tooltip.contains("Liquidity added (5 s)  +8"));
        assert!(added_tooltip.contains("Resting size  10 → 18"));

        let pointer = m.pos(viewer.model.axis.to_x(500_000_000) + 0.001, 100.0);
        let tooltip = heatmap_tooltip(&viewer.chart, &viewer.model, &m, pointer, HeatMetric::Total).unwrap();
        assert!(tooltip.contains("Resting liquidity"));
        assert!(!tooltip.contains("Depth decreases (5 s)"));
        assert!(tooltip.contains("Estimated cancellations (5 s)  −7"));
        assert!(tooltip.contains("Net liquidity added (5 s)  8"));

        let old = viewer.chart.heat.as_ref().unwrap().key.spec;
        let vertical_pan = Mapper { p0: 99.25, p1: 101.25, ..m };
        draw(&mut viewer, &vertical_pan);
        assert_eq!(viewer.chart.heat.as_ref().unwrap().key.spec, old);
        assert!(viewer.chart.heat_worker.as_ref().unwrap().pending.is_none());
        let flow = viewer.chart.flow_heat.as_ref().unwrap();
        assert_eq!(flow.key.prices, (m.p0, m.p1));
        assert_eq!(flow.markers.len(), 2);
        assert!(viewer.chart.flow_worker.as_ref().unwrap().pending);

        let zoom = Mapper { x1: 0.75, ..m };
        draw(&mut viewer, &zoom);
        assert_eq!(viewer.chart.heat.as_ref().unwrap().key.spec, old);
        let latest = Mapper { x0: 0.125, x1: 0.625, ..zoom };
        let target_width = (latest.x1 - latest.x0) / f64::from(latest.rect.width());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while viewer.chart.heat.as_ref().unwrap().key.spec.col_w != target_width
            && std::time::Instant::now() < deadline
        {
            draw(&mut viewer, &latest);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(viewer.chart.heat.as_ref().unwrap().key.spec.col_w, target_width);

        let wide = Mapper { p0: 98.0, p1: 102.0, ..m };
        let detail_id = viewer.chart.heat.as_ref().unwrap().texture.id();
        draw(&mut viewer, &wide);
        let detail = viewer.chart.heat.as_ref().unwrap();
        assert_eq!(detail.key.spec.col_w, target_width);
        assert_eq!(detail.texture.id(), detail_id);
        let preview = viewer.chart.heat_preview.as_ref().unwrap();
        assert!(heat_rect(&wide, &preview.key.spec).contains_rect(wide.rect));
        assert!(preview.key.spec.cols <= PREVIEW_TIME_COLUMNS as usize + 2);
        assert_eq!(preview.key.spec.row_p, 0.25);
        assert!(detail.key.sigma_rows > 0.0);
        assert_eq!(preview.key.sigma_rows, detail.key.sigma_rows);

        viewer.settings.heat_layers[Study::Decreased.index()] = false;
        viewer.settings.heat_layers[Study::Resting.index()] = false;
        draw(&mut viewer, &m);
        assert!(viewer.chart.heat.is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while viewer.chart.flow_heat.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
            draw(&mut viewer, &m);
        }
        let flow = viewer.chart.flow_heat.as_ref().unwrap();
        assert!(flow.texture.is_some());
        assert_eq!(flow.markers.iter().map(|marker| marker.study).collect::<Vec<_>>(), [Study::Added, Study::Cancelled]);
    }

    #[test]
    fn liquidity_hover_reads_the_snapshot_at_the_cursor_tick_and_time() {
        use accelerando_core::{BacktestResult, LiquidityHeatmap, LiquidityLevel, LiquiditySnapshot};

        let levels = vec![LiquidityLevel { price: 100.25, bid_size: 0.0, ask_size: 48.0 }];
        let result = BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: Vec::new(),
            liquidity_heatmap: LiquidityHeatmap {
                interval_ns: 0,
                snapshots: vec![
                    LiquiditySnapshot { ts_ns: 500_000_000, levels },
                    LiquiditySnapshot { ts_ns: 1_500_000_000, levels: Vec::new() },
                ],
            },
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        };
        let model = Model::new(result, &Default::default(), &[], None, Default::default());
        let m = Mapper {
            rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(200.0, 200.0)),
            x0: 0.0,
            x1: 2.0,
            p0: 99.0,
            p1: 101.0,
        };
        let hover = |x, price, metric| resting_liquidity_tooltip(&model, &m, m.pos(x, price), metric);
        assert_eq!(
            hover(0.75, 100.21, HeatMetric::Total).as_deref(),
            Some("Resting liquidity @ 100.25\nBid 0   Ask 48")
        );
        assert!(hover(0.75, 100.0, HeatMetric::Total).is_none());
        assert!(hover(0.75, 100.21, HeatMetric::Bid).is_none());
        assert!(hover(0.25, 100.21, HeatMetric::Total).is_none());
        assert!(hover(1.75, 100.21, HeatMetric::Total).is_none());
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
    fn displayed_bubble_threshold_uses_clustered_total_or_absolute_delta() {
        let result = accelerando_core::BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: Vec::new(),
            liquidity_heatmap: Default::default(),
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        };
        let mut viewer = crate::Viewer::new(result, Default::default(), &[], None, Default::default());
        viewer.settings.bubble_min_displayed_volume = 100.0;
        let m = Mapper {
            rect: Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 300.0)),
            x0: 0.0, x1: 2.0, p0: 99.0, p1: 101.0,
        };
        let columns = [vec![
            QuoteColumn {
                x: 1.0, prices: [f64::NAN; 2], trade_bounds: None,
                bubble: Some(TradeBubble { x: 1.0, price: 100.0, buy: 30.0, sell: 0.0 }),
            },
            QuoteColumn {
                x: 1.001, prices: [f64::NAN; 2], trade_bounds: None,
                bubble: Some(TradeBubble { x: 1.001, price: 100.0, buy: 0.0, sell: 70.0 }),
            },
        ]];
        let hovered = |settings: &Settings| {
            let ctx = egui::Context::default();
            let input = egui::RawInput { screen_rect: Some(m.rect), ..Default::default() };
            let mut dot = None;
            let mut output = ctx.run_ui(input, |ui| {
                let groups = cluster_trade_columns(&m, &columns, bubble_clustering(settings));
                dot = draw_trade_bubbles(ui.painter(), &m, &groups, settings, Some(m.pos(1.0, 100.0)));
            });
            output.textures_delta.clear();
            dot
        };
        viewer.settings.bubble_delta = false;
        assert_eq!(hovered(&viewer.settings).unwrap().trade.total(), 100.0);
        viewer.settings.bubble_delta = true;
        assert!(hovered(&viewer.settings).is_none());
        viewer.settings.bubble_min_displayed_volume = 40.0;
        assert_eq!(hovered(&viewer.settings).unwrap().trade.clustering_volume(true), 40.0);
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
