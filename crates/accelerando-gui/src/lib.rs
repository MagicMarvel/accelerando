//! Native egui/eframe viewer for a [`BacktestResult`].
//!
//! ```no_run
//! # let result: accelerando_core::BacktestResult = unimplemented!();
//! accelerando_gui::view(result, accelerando_gui::ViewerOptions::default()).unwrap();
//! ```
//!
//! The window shows the price panel (zoom-adaptive liquidity heatmap, best-bid / best-ask
//! lines, bars, trades and strategy overlays) and an optional equity panel on the same x axis.
//! The toolbar switches between bars and the heatmap. The heatmap gear controls the
//! liquidity layer, Bid/Ask lines and trade bubbles. Settings contains the equity toggle,
//! metrics and trade list. The x axis uses either equal-width bar slots or proportional time,
//! with closed-market time skipped in both modes.
//!
//! The heatmap needs `Pipeline.keep_liquidity_heatmap = true` and bars need
//! `Pipeline.keep_footprints = true` in the runner that produced the result.

pub mod bars;
mod chart;
pub mod data;
mod heatmap_studies;
mod liquidity_colors;
mod market_history;
#[cfg(windows)]
mod native_windows;
mod big_trades;
mod overlay;
pub mod replay;
mod replay_bubbles;
mod vp;

use std::sync::{mpsc, Arc};

use accelerando_core::{BacktestResult, DepthHistory, DepthStore, DepthWindow, Footprint, LiquidityLevel, PreparedEventData};
use eframe::egui;

/// Total executed volume in footprint and volume-profile histograms.
const TOTAL_VOLUME_COLOR: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(200, 194, 178, 145);

use chart::ChartState;
pub use bars::{aggregate, BarKind, BarSpec, Rebar};
pub use data::{AxisMode, TimeZone};
use data::{BarAxis, BookGrid, HeatMetric, TimeAxis, XMap, SEC_NS};
use liquidity_colors::PriceFilter;
use overlay::Overlays;
use replay::{Replay, ReplayBars};
pub use replay::{ticks_from, Tick, TickSource};
use vp::VpTool;

/// Display settings for [`view`].
#[derive(Clone)]
pub struct ViewerOptions {
    /// Window title.
    pub title: String,
    /// Offset of the displayed wall-clock time from UTC, in hours (e.g. `Some(8.0)` for China).
    /// `None` (the default) uses the computer's own time zone, daylight saving included.
    pub utc_offset_hours: Option<f64>,
    /// Initial width of the bid/ask smoothing kernel in seconds (`0` or `1` = raw lines).
    pub smoothing_secs: u32,
    /// Draw rising bars red and falling bars green (China / Japan convention).
    pub red_up: bool,
    /// Gaps in the data longer than this are cut out of the time axis.
    pub max_gap_secs: i64,
    /// Open zoomed in on the most recent this-many seconds instead of the whole data set.
    pub initial_span_secs: Option<f64>,
    /// x axis: `Some(AxisMode::Bars)` gives every K-line the same width, `Some(AxisMode::Time)`
    /// is proportional to time (needed for the heatmap). `None` picks Time when the
    /// result carries a liquidity heatmap or trade-derived quotes, and Bars otherwise.
    pub axis_mode: Option<AxisMode>,
    /// What `result.footprints` are (shown in the bar-type chooser), e.g. `BarSpec::parse("reverse:16")`.
    pub bar_spec: Option<BarSpec>,
    /// Builds bars of another type for display (usually [`aggregate`] over the raw trades). Without
    /// it the bar-type chooser is disabled.
    pub rebar: Option<Rebar>,
    /// Initial display bars. `None` uses five-minute bars if a bar builder is available.
    pub display_bars: Option<BarSpec>,
    /// Reads raw trades for the tick-by-tick replay (usually [`ticks_from`] over the raw events).
    /// Without it the replay button is disabled.
    pub ticks: Option<TickSource>,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        Self {
            title: "Accelerando".to_string(),
            utc_offset_hours: None,
            smoothing_secs: 10,
            red_up: false,
            max_gap_secs: 300,
            initial_span_secs: None,
            axis_mode: None,
            bar_spec: None,
            rebar: None,
            display_bars: None,
            ticks: None,
        }
    }
}

/// Open the viewer window for `result` and block until it is closed.
///
/// Must be called from the main thread (a windowing-system requirement).
pub fn view(result: BacktestResult, options: ViewerOptions) -> std::io::Result<()> {
    open_viewer(result, options, &[], None, false)
}

/// Open the viewer with trade bubbles and trade-derived quotes when depth is absent.
/// Pass trades in timestamp order.
pub fn view_with_trade_quotes(
    result: BacktestResult,
    options: ViewerOptions,
    events: &[accelerando_core::OrderFlowEvent],
) -> std::io::Result<()> {
    use accelerando_core::OrderFlowEvent;

    let mut tape: Vec<_> = events
        .iter()
        .copied()
        .filter(|event| matches!(event, OrderFlowEvent::Contract { .. } | OrderFlowEvent::Trade { .. }))
        .collect();
    if matches!(tape.first(), Some(OrderFlowEvent::Trade { .. })) {
        tape.insert(0, OrderFlowEvent::Contract {
            tick_size: result.tick_size,
            multiplier: result.multiplier,
        });
    }
    match PreparedEventData::from_events(tape) {
        Ok(tape) => view_with_market_history(result, options, Some(Arc::new(tape)), DepthStore::default()),
        Err(_) => open_viewer(result, options, &data::trade_quotes(events), None, false),
    }
}

/// Open the native viewer with original trade timestamps and sparse L2 history.
///
/// `depth` is a [`DepthHistory`] (all in memory) or a chunked [`DepthStore`]: then zoomed-out
/// views draw the store's overview, and the exact depth of the visible stretch is loaded in
/// the background whenever the view spans at most [`EXACT_DEPTH_SPAN_NS`].
pub fn view_with_market_history(
    result: BacktestResult,
    options: ViewerOptions,
    trades: Option<Arc<PreparedEventData>>,
    depth: impl Into<DepthStore>,
) -> std::io::Result<()> {
    let depth = depth.into();
    if trades.is_none() && depth.is_empty() {
        return view(result, options);
    }
    open_viewer(result, options, &[], Some(MarketLoad {
        load: Box::new(move || Ok((trades, depth))),
        enable_trade_controls: false,
    }), false)
}

type MarketHistory = (Option<Arc<PreparedEventData>>, DepthStore);

struct MarketLoad {
    load: Box<dyn FnOnce() -> Result<MarketHistory, String> + Send>,
    enable_trade_controls: bool,
}

/// Open on the backtest chart while loading market history in the background. Use the returned
/// raw trades for bar switching and tick replay when no custom sources are supplied.
pub fn view_with_market_loader(
    result: BacktestResult,
    options: ViewerOptions,
    load: impl FnOnce() -> Result<(Arc<PreparedEventData>, DepthStore), String> + Send + 'static,
) -> std::io::Result<()> {
    open_with_market_loader(result, options, load, false)
}

/// Open the viewer for a standalone CLI. On Windows a native close request terminates
/// the current process with exit code 0. Complete any required writes before calling.
pub fn view_cli_with_market_loader(
    result: BacktestResult,
    options: ViewerOptions,
    load: impl FnOnce() -> Result<(Arc<PreparedEventData>, DepthStore), String> + Send + 'static,
) -> std::io::Result<()> {
    open_with_market_loader(result, options, load, true)
}

fn open_with_market_loader(
    result: BacktestResult,
    options: ViewerOptions,
    load: impl FnOnce() -> Result<(Arc<PreparedEventData>, DepthStore), String> + Send + 'static,
    exit_on_close: bool,
) -> std::io::Result<()> {
    open_viewer(result, options, &[], Some(MarketLoad {
        load: Box::new(move || load().map(|(trades, depth)| (Some(trades), depth))),
        enable_trade_controls: true,
    }), exit_on_close)
}

fn open_viewer(
    result: BacktestResult,
    options: ViewerOptions,
    quotes: &[data::TradeQuote],
    load: Option<MarketLoad>,
    _exit_on_close: bool,
) -> std::io::Result<()> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(options.title.clone())
            .with_inner_size([1500.0, 900.0]),
        ..Default::default()
    };
    let title = options.title.clone();
    let create = move |cc: &eframe::CreationContext<'_>| {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        install_cjk_font(&cc.egui_ctx);
        let mut viewer = Viewer::new(result, options.clone(), quotes, None, DepthStore::default());
        if let Some(load) = load {
            viewer.load_market(load, options, &cc.egui_ctx);
        }
        viewer
    };
    #[cfg(windows)]
    let result = native_windows::run(&title, native, create, _exit_on_close);
    #[cfg(not(windows))]
    let result = eframe::run_native(&title, native, Box::new(move |cc| Ok(Box::new(create(cc)))));
    result.map_err(|e| std::io::Error::other(e.to_string()))
}

/// Add a system CJK font as a fallback so Chinese strategy labels render (egui's built-in fonts
/// have no CJK glyphs). Silently does nothing when none of the usual fonts exists.
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: [&str; 6] = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Views spanning at most this long load exact depth from a chunked [`DepthStore`].
pub const EXACT_DEPTH_SPAN_NS: i64 = 12 * 3600 * SEC_NS;

/// Bar-axis pauses longer than this become an empty spacer (overnight, weekend, lunch break).
const BAR_GAP_SECS: i64 = 1800;
/// Width of that spacer in bar slots.
const BAR_GAP_SLOTS: f64 = 3.0;
/// Bars in view when the window opens (wide enough for the footprint view).
const INITIAL_BARS: f64 = 20.0;
const DEFAULT_DISPLAY_BARS: BarSpec = BarSpec { kind: BarKind::Time, size: 300 };

/// Everything derived once from the result at startup.
pub(crate) struct Model {
    pub result: Arc<BacktestResult>,
    /// Per-second grid axis of the heatmap and bid/ask lines (independent of the display axis).
    pub axis: TimeAxis,
    pub bounds: Vec<(usize, usize)>,
    pub grid: BookGrid,
    /// Smoothed copies of `grid.bid` / `grid.ask` for the current smoothing window.
    pub bid_smooth: Vec<f32>,
    pub ask_smooth: Vec<f32>,
    /// Display x axis; every x below is in its units.
    pub xmap: XMap,
    /// `(empty-space start, session start x, session start ts)` from [`XMap::sessions`].
    pub sessions: Vec<(f64, f64, i64)>,
    /// Session start x only, for [`data::crosses_session`].
    pub session_x: Vec<f64>,
    /// `(x of first trade, x of last trade)` per visible bar (see [`Model::bars`]).
    pub fp_x: Vec<(f64, f64)>,
    /// The same for every bar, replayed or not.
    fp_x_all: Vec<(f64, f64)>,
    /// Bars visible during a replay; `None` shows everything.
    replay: Option<ReplayBars>,
    /// Strategy bars the overlays were built from (fewer than all during a replay).
    overlay_bars: usize,
    pub equity_x: Vec<f64>,
    /// `(entry x, exit x)` per trade.
    pub trade_x: Vec<(f64, f64)>,
    pub overlays: Overlays,
    pub big_trades: big_trades::BigTradeOverlay,
    pub has_trade_quotes: bool,
    pub trade_volume: Vec<data::TradeVolume>,
    pub filtered_bubble_volume: Option<Vec<data::TradeVolume>>,
    pub tape: Option<market_history::TradeTape>,
    pub depth: DepthStore,
    /// Exact depth of the stretch last viewed closely (chunked stores only).
    pub depth_window: Option<Arc<DepthWindow>>,
    /// Time zone of every time shown.
    pub tz: TimeZone,
    pub price_decimals: usize,
    /// Median footprint duration, to translate time spans into bar counts.
    pub median_bar_secs: f64,
    /// Bars shown instead of the strategy's own `result.footprints` (display only).
    pub display: Option<(BarSpec, Vec<Footprint>)>,
}

struct MarketData {
    axis: TimeAxis,
    bounds: Vec<(usize, usize)>,
    grid: BookGrid,
    bid_smooth: Vec<f32>,
    ask_smooth: Vec<f32>,
    has_trade_quotes: bool,
    trade_volume: Vec<data::TradeVolume>,
    big_trades: big_trades::BigTradeOverlay,
    tape: Option<market_history::TradeTape>,
    depth: DepthStore,
}

impl MarketData {
    fn build(
        result: &BacktestResult,
        axis: &TimeAxis,
        max_gap_ns: i64,
        quotes: &[data::TradeQuote],
        tape: Option<market_history::TradeTape>,
        depth: DepthStore,
    ) -> Self {
        let snapshots = &result.liquidity_heatmap.snapshots;
        let axis = axis.with_market_activity(
            depth.quote_changes().iter().map(|&(ts, _)| ts),
            quotes.iter().map(|quote| quote.ts_ns),
            max_gap_ns,
        );
        let bounds = axis.bounds();
        let mut grid = if snapshots.is_empty() && depth.is_empty() && quotes.is_empty() {
            BookGrid::default()
        } else {
            BookGrid::build(&axis, snapshots)
        };
        if !depth.is_empty() {
            market_history::fill_overview_quotes(&mut grid, &axis, &depth);
        }
        let has_trade_quotes = depth.is_empty() && snapshots.is_empty() && !quotes.is_empty();
        let trade_volume = grid.apply_trade_samples(&axis, &bounds, quotes, has_trade_quotes);
        let big_trades = big_trades::BigTradeOverlay::build(tape.as_ref(), &result.footprints);
        Self {
            axis, bounds, grid, bid_smooth: Vec::new(), ask_smooth: Vec::new(), has_trade_quotes,
            trade_volume, big_trades, tape, depth,
        }
    }

    fn smooth(&mut self, window: u32) {
        let smooth = if self.has_trade_quotes { data::smooth_estimates } else { data::smooth };
        self.bid_smooth = smooth(&self.grid.bid, &self.bounds, window as usize);
        self.ask_smooth = smooth(&self.grid.ask, &self.bounds, window as usize);
    }
}

impl Model {
    fn new(
        result: BacktestResult,
        options: &ViewerOptions,
        quotes: &[data::TradeQuote],
        tape: Option<market_history::TradeTape>,
        depth: DepthStore,
    ) -> Self {
        let snapshots = &result.liquidity_heatmap.snapshots;
        let mut intervals = Vec::with_capacity(
            snapshots.len() + result.footprints.len() + result.equity.len() + 2 * result.trades.len(),
        );
        intervals.extend(snapshots.iter().map(|s| (s.ts_ns, s.ts_ns)));
        intervals.extend(result.footprints.iter().map(|f| (f.ts_first_ns, f.ts_last_ns)));
        intervals.extend(result.equity.iter().map(|e| (e.ts_ns, e.ts_ns)));
        intervals.extend(result.trades.iter().map(|t| (t.entry_ts_ns, t.entry_ts_ns)));
        intervals.extend(result.trades.iter().map(|t| (t.exit_ts_ns, t.exit_ts_ns)));
        let max_gap_ns = options.max_gap_secs.max(1) * SEC_NS;
        let axis = TimeAxis::build(intervals, max_gap_ns);
        let MarketData {
            axis, bounds, grid, bid_smooth, ask_smooth, has_trade_quotes, trade_volume,
            big_trades, tape, depth,
        } = MarketData::build(&result, &axis, max_gap_ns, quotes, tape, depth);
        let tick = result.tick_size;
        let price_decimals = if tick > 0.0 && tick < 1.0 {
            (1..=6)
                .find(|&digits| {
                    let scaled = tick * 10_f64.powi(digits as i32);
                    scaled.round() >= 1.0 && (scaled - scaled.round()).abs() < 1e-8
                })
                .unwrap_or(6)
        } else {
            2
        };
        let mut model = Self {
            bid_smooth,
            ask_smooth,
            xmap: XMap::Time(axis.clone()),
            sessions: Vec::new(),
            session_x: Vec::new(),
            fp_x: Vec::new(),
            fp_x_all: Vec::new(),
            replay: None,
            overlay_bars: usize::MAX,
            equity_x: Vec::new(),
            trade_x: Vec::new(),
            overlays: Overlays::default(),
            big_trades,
            result: Arc::new(result),
            axis,
            bounds,
            grid,
            has_trade_quotes,
            trade_volume,
            filtered_bubble_volume: None,
            tape,
            depth,
            depth_window: None,
            tz: TimeZone::from_hours(options.utc_offset_hours),
            price_decimals,
            median_bar_secs: 60.0,
            display: None,
        };
        let mode = options.axis_mode.unwrap_or_else(|| model.default_axis_mode());
        model.set_axis_mode(mode);
        model
    }

    fn default_axis_mode(&self) -> AxisMode {
        if self.result.footprints.is_empty() || self.has_heatmap() || !self.trade_volume.is_empty() {
            AxisMode::Time
        } else {
            AxisMode::Bars
        }
    }

    /// The bars on screen: the display bars if any, else the strategy's own - during a replay
    /// only the ones replayed so far, the last of them still being built.
    pub fn bars(&self) -> &[Footprint] {
        match &self.replay {
            Some(replay) => &replay.bars,
            None => self.full_bars(),
        }
    }

    /// Every bar of the current bar type, replay or not.
    pub fn full_bars(&self) -> &[Footprint] {
        match &self.display {
            Some((_, bars)) => bars,
            None => &self.result.footprints,
        }
    }

    /// Hide everything from bar `index` (of [`Self::full_bars`]) on and replay from there.
    pub fn start_replay(&mut self, index: usize) {
        self.replay = Some(ReplayBars::new(self.full_bars(), index, self.result.tick_size));
        self.sync_replay();
    }

    pub fn stop_replay(&mut self) {
        self.replay = None;
        self.sync_replay();
    }

    /// Feed trades into the replay and move its clock to `clock_ns`.
    pub fn replay_apply(&mut self, ticks: &[replay::Tick], clock_ns: i64) {
        let full: &[Footprint] = match &self.display {
            Some((_, bars)) => bars,
            None => &self.result.footprints,
        };
        let Some(replay) = self.replay.as_mut() else { return };
        replay.apply_ticks(ticks, full);
        replay.set_clock(clock_ns, full);
        self.sync_replay();
    }

    /// Replay time, when one is running: nothing later is shown.
    pub fn replay_clock(&self) -> Option<i64> {
        self.replay.as_ref().map(|r| r.clock_ns)
    }

    pub fn replay_finished(&self) -> bool {
        self.replay.as_ref().is_some_and(|r| r.finished(self.full_bars()))
    }

    /// Last timestamp of the bar being replayed.
    pub fn replay_bar_end(&self) -> Option<i64> {
        self.replay.as_ref().and_then(|r| r.bar_end_ns(self.full_bars()))
    }

    /// x of the replay position: time-based layers stop here.
    pub fn replay_cut_x(&self) -> Option<f64> {
        let replay = self.replay.as_ref()?;
        Some(match self.xmap.mode() {
            AxisMode::Time => self.xmap.to_x(replay.clock_ns),
            AxisMode::Bars => self.fp_x.last().map_or(0.0, |f| f.1),
        })
    }

    /// Bring `fp_x` and the strategy overlays in line with the visible bars.
    fn sync_replay(&mut self) {
        let n = self.bars().len().min(self.fp_x_all.len());
        if self.fp_x.len() > n {
            self.fp_x.truncate(n);
        } else {
            let from = self.fp_x.len();
            self.fp_x.extend_from_slice(&self.fp_x_all[from..n]);
        }
        // Strategy output of a bar appears once that bar has closed.
        let fps = &self.result.footprints;
        let k = match &self.replay {
            Some(replay) => fps.partition_point(|f| f.ts_last_ns < replay.clock_ns),
            None => fps.len(),
        };
        if k != self.overlay_bars {
            self.overlay_bars = k;
            let xmap = &self.xmap;
            self.overlays = Overlays::build(&fps[..k], &self.result.series, &|i| {
                (xmap.to_x(fps[i].ts_first_ns), xmap.to_x(fps[i].ts_last_ns))
            });
        }
    }

    /// Show other bars (`None` = the strategy's own) and rebuild every x coordinate.
    fn set_display(&mut self, display: Option<(BarSpec, Vec<Footprint>)>) {
        self.display = display;
        self.set_axis_mode(self.xmap.mode());
    }

    /// Rebuild the display axis and every x coordinate derived from it.
    fn set_axis_mode(&mut self, mode: AxisMode) {
        let mut durations: Vec<f64> = self
            .bars()
            .windows(2)
            .map(|w| (w[1].ts_last_ns - w[0].ts_last_ns) as f64 / SEC_NS as f64)
            .filter(|d| *d > 0.0 && *d < BAR_GAP_SECS as f64)
            .collect();
        durations.sort_by(f64::total_cmp);
        self.median_bar_secs = durations.get(durations.len() / 2).copied().unwrap_or(60.0).max(1.0);
        let bars = match &self.display {
            Some((_, bars)) => bars,
            None => &self.result.footprints,
        };
        let result = &self.result;
        self.xmap = match mode {
            AxisMode::Time => XMap::Time(self.axis.clone()),
            AxisMode::Bars => {
                let bars: Vec<(i64, i64)> =
                    bars.iter().map(|f| (f.ts_first_ns, f.ts_last_ns)).collect();
                XMap::Bars(BarAxis::build(&bars, BAR_GAP_SECS * SEC_NS, BAR_GAP_SLOTS))
            }
        };
        let xmap = &self.xmap;
        self.sessions = xmap.sessions();
        self.session_x = self.sessions.iter().map(|s| s.1).collect();
        self.fp_x_all = match xmap {
            // One slot per bar, by construction of the bar axis.
            XMap::Bars(_) => bars
                .iter()
                .map(|f| {
                    let start = xmap.to_x(f.ts_first_ns).floor();
                    (start, start + 1.0)
                })
                .collect(),
            XMap::Time(_) => bars
                .iter()
                .map(|f| (xmap.to_x(f.ts_first_ns), xmap.to_x(f.ts_last_ns)))
                .collect(),
        };
        self.equity_x = result.equity.iter().map(|e| xmap.to_x(e.ts_ns)).collect();
        self.trade_x = result
            .trades
            .iter()
            .map(|t| (xmap.to_x(t.entry_ts_ns), xmap.to_x(t.exit_ts_ns)))
            .collect();
        // Strategy overlays are anchored to the strategy's own bars; map them by time so they
        // land in the right place whatever bars are displayed.
        self.fp_x.clear();
        self.overlay_bars = usize::MAX;
        self.sync_replay();
    }

    fn set_smoothing(&mut self, window_secs: usize) {
        let smooth = if self.has_trade_quotes { data::smooth_estimates } else { data::smooth };
        self.bid_smooth = smooth(&self.grid.bid, &self.bounds, window_secs);
        self.ask_smooth = smooth(&self.grid.ask, &self.bounds, window_secs);
    }

    pub fn completed_seconds(&self) -> usize {
        self.replay_cut_x().map_or(self.grid.bid.len(), |x| {
            (x as usize).min(self.grid.bid.len())
        })
    }

    pub fn display_quotes(&self) -> (&[f32], &[f32]) {
        let len = if self.has_trade_quotes { self.completed_seconds() } else { self.grid.bid.len() };
        (&self.bid_smooth[..len], &self.ask_smooth[..len])
    }

    /// Exact depth for `[from, until)` when loaded (or the store is in memory), else the
    /// overview; the second value identifies the window for heatmap caching.
    pub fn depth_at(&self, from: i64, until: i64) -> (&DepthHistory, Option<(i64, i64)>) {
        match &self.depth_window {
            Some(window) if window.covers(from, until) => {
                (&window.history, Some((window.from, window.until)))
            }
            _ => (self.depth.overview(), None),
        }
    }

    pub fn has_heatmap(&self) -> bool {
        !self.depth.is_empty() || !self.result.liquidity_heatmap.snapshots.is_empty()
    }

    fn replay_snapshots(&self, clock: Option<i64>) -> &[accelerando_core::LiquiditySnapshot] {
        let heat = &self.result.liquidity_heatmap;
        let Some(clock) = clock else { return &heat.snapshots };
        // Fixed-grid snapshots store the end-of-bucket book at the bucket's start.
        let end = heat.snapshots.partition_point(|s| s.ts_ns.saturating_add(heat.interval_ns.max(0)) <= clock);
        &heat.snapshots[..end]
    }

    fn replay_depth(&self, clock: i64, low: f64, high: f64) -> Option<Vec<LiquidityLevel>> {
        if self.depth.is_empty() {
            let segment = self.axis.segments.partition_point(|s| s.t0_ns <= clock);
            let levels = self.replay_snapshots(Some(clock)).last().filter(|snapshot| {
                segment > 0 && snapshot.ts_ns >= self.axis.segments[segment - 1].t0_ns
            })
                .map_or(&[][..], |snapshot| snapshot.levels.as_slice());
            return Some(levels.iter().copied().filter(|l| (low..=high).contains(&l.price)).collect());
        }

        let (depth, window) = self.depth_at(clock, clock.saturating_add(1));
        // Overview buckets can include updates after the replay clock.
        if window.is_none() && !self.depth.is_exact() {
            return None;
        }
        Some(depth.levels_between(low, high).filter_map(|(price, level)| {
            let changes = level.changes();
            let index = changes.partition_point(|change| change.0 <= clock).checked_sub(1)?;
            let [bid_size, ask_size] = changes[index].1;
            (bid_size > 0.0 || ask_size > 0.0).then_some(LiquidityLevel { price, bid_size, ask_size })
        }).collect())
    }

    /// The heatmap and bid/ask lines live on the per-second grid: time axis only.
    pub fn book_visible(&self) -> bool {
        (self.has_heatmap() || self.has_trade_quotes) && self.xmap.mode() == AxisMode::Time
    }

    /// Resting size at the hovered tick, within the visible part of the book.
    fn resting_liquidity_at(&self, x: f64, price: f64) -> Option<(f64, [f64; 2])> {
        if !self.book_visible() || !(0.0..self.axis.total()).contains(&x) {
            return None;
        }
        let ts = self.axis.to_ts(x);
        if self.replay_clock().is_some_and(|clock| ts > clock) {
            return None;
        }
        if !self.depth.is_empty() {
            return self.depth_at(ts, ts + 1).0.resting_size_at(price, ts);
        }

        let snapshots = &self.result.liquidity_heatmap.snapshots;
        let i = snapshots.partition_point(|snapshot| snapshot.ts_ns <= ts).checked_sub(1)?;
        let snapshot = &snapshots[i];
        let segment = self.axis.segments.partition_point(|segment| segment.x0 <= x);
        if snapshot.ts_ns < self.axis.segments[segment - 1].t0_ns {
            return None;
        }
        let tick = self.result.tick_size;
        if !tick.is_finite() || tick <= 0.0 {
            return None;
        }
        let price = (price / tick).round() * tick;
        let level = snapshot.levels.iter().find(|level| (level.price - price).abs() < tick * 0.5)?;
        Some((level.price, [level.bid_size, level.ask_size]))
    }

    /// A time span expressed in x units of the current axis.
    pub fn span_x(&self, secs: f64) -> f64 {
        match self.xmap.mode() {
            AxisMode::Time => secs,
            AxisMode::Bars => (secs / self.median_bar_secs).max(5.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartMode {
    Bar,
    Heatmap,
}

/// Chart display settings.
pub(crate) struct Settings {
    pub mode: ChartMode,
    pub big_trades: big_trades::BigTradeSettings,
    pub footprint_imbalance: chart::FootprintImbalance,
    pub show_heat: bool,
    pub show_bbo: bool,
    pub show_trade_bubbles: bool,
    pub bubble_min_trade_size: f64,
    pub bubble_min_displayed_volume: f64,
    pub bubble_aggregation: u32,
    pub bubble_size: u32,
    pub bubble_3d: bool,
    pub bubble_colorblind: bool,
    pub bubble_delta: bool,
    pub bubble_transparency: u32,
    pub show_bar_poc: bool,
    pub show_bar_value_area: bool,
    pub show_trades: bool,
    pub show_overlays: bool,
    pub show_strategy_profiles: bool,
    pub hidden_overlay_groups: std::collections::BTreeSet<String>,
    /// Point markers strategies attach to bars (e.g. labelled entry dots).
    pub show_markers: bool,
    pub show_equity: bool,
    pub heat_metric: HeatMetric,
    pub heat_layers: [bool; heatmap_studies::Study::COUNT],
    pub heat_studies: heatmap_studies::Studies,
    pub flow_marker_minimum: [u32; 3],
    pub flow_marker_limit: usize,
    pub heat_smooth: bool,
    pub price_filter: PriceFilter,
    pub price_filter_px: f32,
    pub heat_low_pct: f32,
    pub heat_high_pct: f32,
    pub heat_contrast: f32,
    pub smoothing_secs: u32,
    pub red_up: bool,
}

impl Settings {
    fn heat_layer_enabled(&self, study: heatmap_studies::Study) -> bool {
        self.heat_layers[study.index()]
    }

    fn heat_layers_lookback(&self) -> Option<i64> {
        heatmap_studies::Study::ALL.into_iter().filter_map(|study| {
            let settings = heatmap_studies::Studies { study, ..self.heat_studies };
            (self.heat_layer_enabled(study) && settings.active()).then(|| settings.lookback_ns())
        }).max()
    }

    fn select_mode(&mut self, mode: ChartMode, has_bubbles: bool) {
        self.mode = mode;
        self.show_bbo = mode == ChartMode::Heatmap;
        self.show_trade_bubbles = self.show_bbo && has_bubbles;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolbarPopup {
    Settings,
    Heatmap,
}

struct LoadedMarket {
    data: MarketData,
    options: ViewerOptions,
    smoothing_secs: u32,
}

#[derive(Clone, Copy)]
struct DisplayBarRequest {
    spec: BarSpec,
    fit_latest: bool,
}

struct Viewer {
    model: Model,
    settings: Settings,
    chart: ChartState,
    applied_smoothing: u32,
    applied_bubble_min_trade_size: f64,
    metrics_rows: Vec<(String, String)>,
    selected_trade: Option<usize>,
    toolbar_popup: Option<ToolbarPopup>,
    metrics_open: bool,
    trades_open: bool,
    /// Bars the strategy ran on, and the host's bar builder.
    strategy_spec: Option<BarSpec>,
    rebar: Option<Rebar>,
    bar_dialog_open: bool,
    bar_draft: BarSpec,
    pending: Option<(DisplayBarRequest, mpsc::Receiver<Result<Vec<Footprint>, String>>)>,
    bar_error: Option<String>,
    /// Requested once the bar builder becomes available.
    startup_bars: Option<DisplayBarRequest>,
    /// Profiles drawn by hand and their tool settings.
    vp: VpTool,
    /// Tick-by-tick replay and the host's tick reader.
    replay: Replay,
    ticks: Option<TickSource>,
    /// Background load of exact depth: the requested range and its result.
    depth_loading: Option<((i64, i64), mpsc::Receiver<Result<DepthWindow, String>>)>,
    market_loading: Option<mpsc::Receiver<Result<LoadedMarket, String>>>,
    market_error: Option<String>,
}

impl Viewer {
    fn new(
        result: BacktestResult,
        options: ViewerOptions,
        quotes: &[data::TradeQuote],
        tape: Option<market_history::TradeTape>,
        depth: DepthStore,
    ) -> Self {
        let metrics_rows = metrics_rows(&result);
        let mut model = Model::new(result, &options, quotes, tape, depth);
        let smoothing_secs = if model.has_trade_quotes { 3 } else { options.smoothing_secs };
        model.set_smoothing(smoothing_secs as usize);
        let mode = if model.bars().is_empty() && model.book_visible() {
            ChartMode::Heatmap
        } else {
            ChartMode::Bar
        };
        let settings = Settings {
            mode,
            big_trades: Default::default(),
            footprint_imbalance: Default::default(),
            show_heat: true,
            show_bbo: mode == ChartMode::Heatmap,
            show_trade_bubbles: mode == ChartMode::Heatmap && !model.trade_volume.is_empty(),
            bubble_min_trade_size: 0.0,
            bubble_min_displayed_volume: 1.0,
            bubble_aggregation: 52,
            bubble_size: 40,
            bubble_3d: true,
            bubble_colorblind: false,
            bubble_delta: true,
            bubble_transparency: 41,
            show_bar_poc: false,
            show_bar_value_area: false,
            show_trades: true,
            show_overlays: true,
            show_strategy_profiles: true,
            hidden_overlay_groups: Default::default(),
            show_markers: false,
            show_equity: false,
            heat_metric: HeatMetric::Total,
            heat_layers: [true, false, false, false, false],
            heat_studies: Default::default(),
            flow_marker_minimum: [40; 3],
            flow_marker_limit: 35,
            heat_smooth: true,
            price_filter: PriceFilter::Auto,
            price_filter_px: 1.5,
            heat_low_pct: 2.0,
            heat_high_pct: 99.5,
            heat_contrast: 4.0,
            smoothing_secs,
            red_up: options.red_up,
        };
        let mut chart = ChartState::new(model.xmap.total(), model.xmap.mode());
        let initial_span = options.initial_span_secs.filter(|s| *s > 0.0);
        match initial_span {
            Some(span) => chart.show_latest(model.span_x(span)),
            None => Self::fit_latest_bars(&model, &mut chart),
        }
        let initial_bars = options.display_bars.unwrap_or(DEFAULT_DISPLAY_BARS);
        Self {
            model,
            settings,
            chart,
            applied_smoothing: smoothing_secs,
            applied_bubble_min_trade_size: 0.0,
            metrics_rows,
            selected_trade: None,
            toolbar_popup: None,
            metrics_open: false,
            trades_open: false,
            strategy_spec: options.bar_spec,
            rebar: options.rebar.clone(),
            bar_dialog_open: false,
            bar_draft: initial_bars,
            pending: None,
            bar_error: None,
            startup_bars: (Some(initial_bars) != options.bar_spec).then_some(DisplayBarRequest {
                spec: initial_bars,
                fit_latest: initial_span.is_none() && options.display_bars.is_none(),
            }),
            vp: VpTool::default(),
            replay: Replay::default(),
            ticks: options.ticks.clone(),
            depth_loading: None,
            market_loading: None,
            market_error: None,
        }
    }

    fn fit_latest_bars(model: &Model, chart: &mut ChartState) {
        if !model.bars().is_empty() {
            let bars = INITIAL_BARS.min(model.bars().len() as f64);
            let span = match model.xmap.mode() {
                AxisMode::Bars => bars,
                AxisMode::Time => bars * model.median_bar_secs,
            };
            chart.show_latest(span.max(5.0));
        }
    }

    fn load_market(&mut self, load: MarketLoad, mut options: ViewerOptions, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let result = Arc::clone(&self.model.result);
        let axis = self.model.axis.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let loaded = (load.load)().map(|(trades, depth)| {
                if let Some(events) = trades.as_ref().filter(|_| load.enable_trade_controls) {
                    if options.rebar.is_none() {
                        let events = Arc::clone(events);
                        options.rebar = Some(Arc::new(move |spec| Ok(aggregate(events.events(), spec))));
                    }
                    if options.ticks.is_none() {
                        let events = Arc::clone(events);
                        options.ticks = Some(Arc::new(move |from, max| Ok(ticks_from(events.events(), from, max))));
                    }
                }
                let quotes = trades.as_ref().map_or_else(Vec::new, |events| data::trade_quotes(events.events()));
                let tape = trades.map(|events| market_history::TradeTape::new(events, &quotes));
                let mut data = MarketData::build(
                    &result, &axis, options.max_gap_secs.max(1) * SEC_NS, &quotes, tape, depth,
                );
                let smoothing_secs = if data.has_trade_quotes { 3 } else { options.smoothing_secs };
                data.smooth(smoothing_secs);
                LoadedMarket { data, options, smoothing_secs }
            });
            if tx.send(loaded).is_ok() {
                ctx.request_repaint();
            }
        });
        self.market_loading = Some(rx);
    }

    fn poll_market(&mut self) {
        let Some(rx) = &self.market_loading else { return };
        let loaded = match rx.try_recv() {
            Ok(loaded) => loaded,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("Market data loader stopped".to_string()),
        };
        self.market_loading = None;
        match loaded {
            Ok(loaded) => self.apply_market(loaded),
            Err(error) => {
                eprintln!("Market data unavailable: {error}");
                self.market_error = Some(error);
            }
        }
    }

    fn apply_market(&mut self, loaded: LoadedMarket) {
        let LoadedMarket { data, options, smoothing_secs } = loaded;
        let model = &mut self.model;
        let old_mode = model.xmap.mode();
        let old_total = model.xmap.total();
        let showing_all = self.chart.x0 <= 0.0 && self.chart.x1 >= old_total;
        let had_book = model.book_visible();
        let had_bubbles = !model.trade_volume.is_empty();
        let visible = [self.chart.x0, self.chart.x1].map(|x| model.xmap.to_ts(x));
        let axis_changed = model.axis != data.axis;
        model.axis = data.axis;
        model.bounds = data.bounds;
        model.grid = data.grid;
        model.bid_smooth = data.bid_smooth;
        model.ask_smooth = data.ask_smooth;
        model.has_trade_quotes = data.has_trade_quotes;
        model.trade_volume = data.trade_volume;
        model.big_trades = data.big_trades;
        model.tape = data.tape;
        model.depth = data.depth;
        model.depth_window = None;
        self.depth_loading = None;
        self.rebar = options.rebar;
        self.ticks = options.ticks;
        if self.settings.smoothing_secs == options.smoothing_secs {
            self.settings.smoothing_secs = smoothing_secs;
        }
        self.applied_smoothing = smoothing_secs;
        model.filtered_bubble_volume = None;
        self.applied_bubble_min_trade_size = 0.0;

        let mode = options.axis_mode.unwrap_or_else(|| model.default_axis_mode());
        let remap = mode != old_mode || (mode == AxisMode::Time && axis_changed);
        if remap {
            model.set_axis_mode(mode);
        }
        self.chart.set_total(model.xmap.total(), mode);
        if old_total <= 0.0 {
            self.chart.fit_all();
            if let Some(span) = options.initial_span_secs.filter(|span| *span > 0.0) {
                self.chart.show_latest(model.span_x(span));
            }
        } else if remap && showing_all {
            self.chart.show_range(0.0, model.xmap.total());
        } else if remap {
            let [x0, x1] = visible.map(|ts| model.xmap.to_x(ts));
            self.chart.show_range(x0, x1);
        }
        let has_bubbles = !model.trade_volume.is_empty();
        if !had_book && model.book_visible() && model.bars().is_empty() {
            self.settings.select_mode(ChartMode::Heatmap, has_bubbles);
        } else if self.settings.mode == ChartMode::Heatmap && !had_bubbles && has_bubbles {
            self.settings.show_trade_bubbles = true;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let settings = ui.selectable_label(
                self.toolbar_popup == Some(ToolbarPopup::Settings),
                "⚙ Settings",
            );
            ui.separator();
            let label = self.current_bar_label();
            if ui
                .add_enabled(self.rebar.is_some(), egui::Button::new(format!("{label} ▾")))
                .on_hover_text("Bar type")
                .on_disabled_hover_text("Bar type (the host did not provide raw trades)")
                .clicked()
            {
                self.bar_dialog_open = !self.bar_dialog_open;
            }
            if ui
                .selectable_label(self.settings.mode == ChartMode::Bar, "Bar")
                .clicked()
            {
                self.settings.select_mode(ChartMode::Bar, false);
                self.toolbar_popup = None;
            }
            let heatmap = egui::Button::selectable(self.settings.mode == ChartMode::Heatmap, "Heatmap");
            if ui
                .add_enabled(self.model.book_visible(), heatmap)
                .on_disabled_hover_text("Requires order-book or trade data and a time-proportional x axis.")
                .clicked()
            {
                self.settings.select_mode(ChartMode::Heatmap, !self.model.trade_volume.is_empty());
                self.toolbar_popup = None;
            }
            let heatmap_gear = ui
                .selectable_label(self.toolbar_popup == Some(ToolbarPopup::Heatmap), "⚙")
                .on_hover_text("Heatmap settings");
            self.show_toolbar_popup(&settings, &heatmap_gear);
            if self.market_loading.is_some() {
                ui.spinner();
                ui.label("Loading market data…");
            } else if let Some(error) = &self.market_error {
                ui.label("Market data unavailable").on_hover_text(error);
            }
            if let Some((request, _)) = &self.pending {
                ui.spinner();
                ui.label(format!("building {} bars ...", request.spec.label()));
            }
            ui.separator();
            ui.checkbox(&mut self.chart.auto_y, "Auto Y")
                .on_hover_text("Double-click the chart to show everything.");
            ui.separator();
            ui.toggle_value(&mut self.vp.drawing, "📊 Draw VP").on_hover_text(
                "Drag across bars to draw one volume profile. Right-click a profile to delete it, \
                 or press Esc to cancel drawing.",
            );
            let vp_settings = ui.button("⚙").on_hover_text("Manual VP settings");
            egui::Popup::from_toggle_button_response(&vp_settings)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| self.vp.controls(ui));
            let n = self.vp.items.len();
            if ui.add_enabled(n > 0, egui::Button::new(format!("Clear VPs ({n})"))).clicked() {
                self.vp.items.clear();
            }
            ui.separator();
            if self.model.replay_clock().is_none() {
                ui.add_enabled_ui(self.ticks.is_some(), |ui| {
                    ui.toggle_value(&mut self.replay.picking, "▶ Replay")
                        .on_hover_text(
                            "Click a bar: everything after it is hidden and the trades are \
                             played back tick by tick.",
                        )
                        .on_disabled_hover_text("Replay (the host did not provide raw trades)");
                });
                if self.replay.picking {
                    ui.weak("click the bar to start from (Esc cancels)");
                }
            } else {
                let clock = self.model.replay_clock().unwrap_or(0);
                ui.label(
                    egui::RichText::new(format!("▶ {}", data::format_ts_full(clock, self.model.tz)))
                        .monospace()
                        .color(chart::REPLAY),
                );
            }
        });
        if self.model.replay_clock().is_some() {
            let heatmap = self.settings.mode == ChartMode::Heatmap;
            ui.horizontal_wrapped(|ui| self.replay.controls(ui, &mut self.model, &mut self.chart, heatmap));
        }
    }

    fn current_bar_label(&self) -> String {
        match (&self.model.display, self.strategy_spec) {
            (Some((spec, _)), _) => spec.label(),
            (None, Some(spec)) => spec.label(),
            (None, None) => "Strategy bars".to_string(),
        }
    }

    fn bar_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.bar_dialog_open;
        let mut apply: Option<Option<BarSpec>> = None;
        egui::Window::new("Bar type")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_pos(egui::pos2(140.0, 44.0))
            .show(ctx, |ui| {
                let draft = &mut self.bar_draft;
                ui.horizontal(|ui| {
                    for kind in BarKind::ALL {
                        if ui.selectable_label(draft.kind == kind, kind.name()).clicked() && draft.kind != kind {
                            draft.kind = kind;
                            draft.size = if kind == BarKind::Time { DEFAULT_DISPLAY_BARS.size } else { kind.presets()[1] };
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut draft.size).range(1..=1_000_000).speed(1.0));
                    ui.label(draft.kind.unit());
                });
                ui.horizontal(|ui| {
                    for &size in draft.kind.presets() {
                        let text = BarSpec::new(draft.kind, size).label();
                        if ui.selectable_label(draft.size == size, text).clicked() {
                            draft.size = size;
                        }
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let busy = self.pending.is_some();
                    if ui.add_enabled(!busy, egui::Button::new("Apply")).clicked() {
                        apply = Some(Some(*draft));
                    }
                    let back = match self.strategy_spec {
                        Some(spec) => format!("Strategy bars ({})", spec.label()),
                        None => "Strategy bars".to_string(),
                    };
                    if ui
                        .add_enabled(!busy && self.model.display.is_some(), egui::Button::new(back))
                        .clicked()
                    {
                        apply = Some(None);
                    }
                });
                if let Some(error) = &self.bar_error {
                    ui.colored_label(chart::LOSS, error);
                }
                ui.add_space(2.0);
                ui.weak(
                    "Display only: the chart is rebuilt from the raw trades, while trades, equity \
                     and strategy zones are placed by time. The backtest itself is not rerun - \
                     to run the strategy on other bars, restart the runner with another --agg.",
                );
            });
        self.bar_dialog_open = open;
        match apply {
            Some(Some(spec)) if Some(spec) == self.strategy_spec => self.show_bars(None),
            Some(Some(spec)) => self.request_bars(DisplayBarRequest { spec, fit_latest: false }, ctx),
            Some(None) => self.show_bars(None),
            None => {}
        }
    }

    /// Build display bars on a background thread (they arrive in [`Self::poll_bars`]).
    fn request_bars(&mut self, request: DisplayBarRequest, ctx: &egui::Context) {
        let Some(rebar) = self.rebar.clone() else { return };
        self.bar_error = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(rebar(&request.spec));
            ctx.request_repaint();
        });
        self.pending = Some((request, rx));
    }

    fn poll_bars(&mut self, ctx: &egui::Context) {
        if let Some(request) = self.rebar.as_ref().and_then(|_| self.startup_bars.take()) {
            self.request_bars(request, ctx);
        }
        let Some((request, rx)) = &self.pending else { return };
        let DisplayBarRequest { spec, fit_latest } = *request;
        match rx.try_recv() {
            Ok(Ok(bars)) if bars.is_empty() => {
                self.pending = None;
                self.bar_error = Some(format!("{} produced no bars", spec.label()));
            }
            Ok(Ok(bars)) => {
                self.pending = None;
                self.show_bars(Some((spec, bars)));
                if fit_latest {
                    Self::fit_latest_bars(&self.model, &mut self.chart);
                }
            }
            Ok(Err(error)) => {
                self.pending = None;
                self.bar_error = Some(error);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                self.bar_error = Some("bar builder stopped".to_string());
            }
        }
    }

    /// Swap the displayed bars, keeping the same stretch of time in view.
    fn show_bars(&mut self, display: Option<(BarSpec, Vec<Footprint>)>) {
        self.replay.stop(&mut self.model);
        self.bar_error = None;
        let xmap = &self.model.xmap;
        let (t0, t1) = (xmap.to_ts(self.chart.x0), xmap.to_ts(self.chart.x1));
        self.model.set_display(display);
        let xmap = &self.model.xmap;
        self.chart.set_total(xmap.total(), xmap.mode());
        self.chart.show_range(xmap.to_x(t0), xmap.to_x(t1));
    }

    fn show_toolbar_popup(&mut self, settings: &egui::Response, heatmap_gear: &egui::Response) {
        let clicked = if settings.clicked() {
            Some(ToolbarPopup::Settings)
        } else if heatmap_gear.clicked() {
            Some(ToolbarPopup::Heatmap)
        } else {
            None
        };
        if let Some(popup) = clicked {
            self.toolbar_popup = (self.toolbar_popup != Some(popup)).then_some(popup);
        }
        let Some(popup) = self.toolbar_popup else {
            return;
        };
        let anchor = match popup {
            ToolbarPopup::Settings => settings,
            ToolbarPopup::Heatmap => heatmap_gear,
        };
        // The chart handles click dismissal so interacting with a ComboBox popup
        // (which has its own layer) leaves the toolbar popup open.
        let mut open = true;
        egui::Popup::from_response(anchor)
            .open_bool(&mut open)
            .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
            .show(|ui| match popup {
                ToolbarPopup::Settings => self.settings_controls(ui),
                ToolbarPopup::Heatmap => self.heatmap_controls(ui),
            });
        if !open {
            self.toolbar_popup = None;
        }
    }

    fn heatmap_controls(&mut self, ui: &mut egui::Ui) {
        let has_heat = self.model.has_heatmap();
        let has_quotes = self.model.has_trade_quotes;
        let book_visible = self.model.book_visible();
        let has_bubbles = !self.model.trade_volume.is_empty();
        let has_tape = self.model.tape.is_some();
        let has_depth = !self.model.depth.is_empty();
        let s = &mut self.settings;
        if !has_heat && !has_quotes {
            ui.weak("No order-book or trade-derived quote data in this result.");
            return;
        }
        if !book_visible {
            ui.weak("Heatmap requires a time-proportional x axis.");
        }
        ui.add_enabled_ui(book_visible, |ui| {
            if has_quotes {
                ui.checkbox(&mut s.show_bbo, "Estimated Bid / Ask lines")
                    .on_hover_text("Highest buy and lowest sell per visible time column. A missing side keeps its previous price.");
            } else {
                ui.checkbox(&mut s.show_bbo, "Bid / Ask lines");
            }
            if has_bubbles {
                trade_bubble_controls(ui, s, has_tape);
            }
            if has_quotes {
                ui.add(egui::Slider::new(&mut s.smoothing_secs, 0..=15).suffix(" s").text("line smoothing"))
                    .on_hover_text("Trailing average for the overview. Close zoom uses event-time quotes.");
                return;
            }
            ui.add(
                egui::Slider::new(&mut s.smoothing_secs, 0..=300)
                    .logarithmic(true)
                    .suffix(" s")
                    .text("line smoothing"),
            )
            .on_hover_text(
                "Width of the zero-lag (centered) smoothing kernel applied to the \
                 overview bid/ask lines. Close zoom uses event-time quotes.",
            );
            ui.add_enabled_ui(s.mode == ChartMode::Heatmap, |ui| {
                ui.checkbox(&mut s.show_heat, "Heatmap layers");
                if s.show_heat {
                    heatmap_study_controls(ui, s, has_depth, has_tape);
                }
            });
            egui::ComboBox::from_label("heat")
                .selected_text(match s.heat_metric {
                    HeatMetric::Total => "Bid + Ask",
                    HeatMetric::Bid => "Bid",
                    HeatMetric::Ask => "Ask",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.heat_metric, HeatMetric::Total, "Bid + Ask");
                    ui.selectable_value(&mut s.heat_metric, HeatMetric::Bid, "Bid");
                    ui.selectable_value(&mut s.heat_metric, HeatMetric::Ask, "Ask");
                });
            ui.add_enabled_ui(s.heat_layer_enabled(heatmap_studies::Study::Resting), |ui| {
                ui.checkbox(&mut s.heat_smooth, "Time smoothing");
                egui::ComboBox::from_label("Price smoothing")
                    .selected_text(match s.price_filter {
                        PriceFilter::Off => "Off",
                        PriceFilter::Auto => "Auto",
                        PriceFilter::Gaussian => "Gaussian",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.price_filter, PriceFilter::Off, "Off");
                        ui.selectable_value(&mut s.price_filter, PriceFilter::Auto, "Auto");
                        ui.selectable_value(&mut s.price_filter, PriceFilter::Gaussian, "Gaussian");
                    });
                if s.price_filter == PriceFilter::Gaussian {
                    ui.add(egui::Slider::new(&mut s.price_filter_px, 0.5..=15.0).suffix(" px").text("σ"));
                }
            });
            ui.add(egui::Slider::new(&mut s.heat_low_pct, 0.0..=30.0).suffix("%").text("Lower cutoff"));
            ui.add(egui::Slider::new(&mut s.heat_high_pct, 80.0..=100.0).suffix("%").text("Upper cutoff"));
            ui.add(egui::Slider::new(&mut s.heat_contrast, 0.5..=10.0).text("Contrast"));
        });
    }

    fn settings_controls(&mut self, ui: &mut egui::Ui) {
        let has_equity = !self.model.result.equity.is_empty();
        let trade_count = self.model.result.trades.len();
        let s = &mut self.settings;
        ui.strong("Panels");
        ui.add_enabled_ui(has_equity, |ui| {
            ui.checkbox(&mut s.show_equity, "Equity curve");
        });
        ui.checkbox(&mut self.metrics_open, "Metrics");
        ui.checkbox(&mut self.trades_open, format!("Trade list ({trade_count})"));

        ui.separator();
        ui.strong("Chart");
        ui.checkbox(&mut s.show_bar_poc, "Show POC")
            .on_hover_text("Outline the highest-volume row in each footprint.");
        ui.checkbox(&mut s.show_bar_value_area, "Show Value Area")
            .on_hover_text("Shade the price range containing at least 70% of each bar's traded volume.");
        ui.checkbox(&mut s.show_trades, "Trades");
    }

    fn metrics_window(&mut self, ctx: &egui::Context) {
        let rows = &self.metrics_rows;
        egui::Window::new("Metrics")
            .open(&mut self.metrics_open)
            .resizable(false)
            .default_pos(egui::pos2(300.0, 44.0))
            .show(ctx, |ui| {
                egui::Grid::new("metrics").num_columns(2).striped(true).show(ui, |ui| {
                    for (key, value) in rows {
                        ui.label(key);
                        ui.monospace(value);
                        ui.end_row();
                    }
                });
            });
    }

    fn trades_window(&mut self, ctx: &egui::Context) {
        let mut open = self.trades_open;
        let mut clicked = None;
        let trades = &self.model.result.trades;
        let tz = self.model.tz;
        let selected = self.selected_trade;
        egui::Window::new("Trades")
            .open(&mut open)
            .default_pos(egui::pos2(620.0, 44.0))
            .default_size([340.0, 480.0])
            .show(ctx, |ui| {
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
                    ui,
                    row_h,
                    trades.len(),
                    |ui, rows| {
                        for i in rows {
                            let t = &trades[i];
                            let text = format!(
                                "{:>4} {} {}  {:>10.2}",
                                i + 1,
                                if t.dir > 0 { "L" } else { "S" },
                                data::format_ts(t.entry_ts_ns, tz, true, false),
                                t.pnl
                            );
                            let color = if t.pnl >= 0.0 { chart::GAIN } else { chart::LOSS };
                            let label = egui::RichText::new(text).monospace().color(color);
                            if ui.selectable_label(selected == Some(i), label).clicked() {
                                clicked = Some(i);
                            }
                        }
                    },
                );
            });
        self.trades_open = open;
        if let Some(i) = clicked {
            self.selected_trade = Some(i);
            let (entry_x, exit_x) = self.model.trade_x[i];
            let min_span = self.model.span_x(600.0);
            self.chart.focus((entry_x + exit_x) * 0.5, (exit_x - entry_x) * 1.5, min_span);
        }
    }

    /// Load exact depth for a close time viewport or for the replay clock.
    /// Wide time views use the overview for history and load only the replay clock's chunk.
    fn sync_depth(&mut self, ctx: &egui::Context) {
        let model = &mut self.model;
        if model.depth.is_exact() {
            return;
        }
        if let Some((_, rx)) = &self.depth_loading {
            match rx.try_recv() {
                Ok(Ok(window)) => {
                    model.depth_window = Some(Arc::new(window));
                    self.depth_loading = None;
                }
                Ok(Err(error)) => {
                    eprintln!("depth: {error}");
                    self.depth_loading = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.depth_loading = None,
            }
        }
        let replay = model.replay_clock();
        let total = model.axis.total();
        let mut from = model.axis.to_ts(self.chart.x0.clamp(0.0, total));
        let mut until = model.axis.to_ts(self.chart.x1.clamp(0.0, total)).saturating_add(1);
        if let Some(clock) = replay {
            // Keep the current book available while the chart is panned near the cursor.
            from = from.min(clock);
            until = until.max(clock.saturating_add(1));
        }
        let span = until.saturating_sub(from);
        let time_view = model.xmap.mode() == AxisMode::Time && total > 0.0;
        let cursor_only = !time_view || span > EXACT_DEPTH_SPAN_NS;
        if cursor_only {
            let Some(clock) = replay else {
                if time_view && span > 2 * EXACT_DEPTH_SPAN_NS {
                    model.depth_window = None;
                }
                return;
            };
            // A wide chart can use the overview while the replay edge still uses exact depth.
            from = clock;
            until = clock.saturating_add(1);
        }
        if !cursor_only && self.settings.mode == ChartMode::Heatmap && self.settings.show_heat {
            if let Some(lookback) = self.settings.heat_layers_lookback() {
                let margin = span / 16;
                from = from.saturating_sub(margin + lookback);
                until = until.saturating_add(margin + lookback);
            }
        }
        let covered = |range: (i64, i64)| range.0 <= from && until <= range.1;
        if model.depth_window.as_ref().is_some_and(|w| w.covers(from, until))
            || self.depth_loading.as_ref().is_some_and(|(range, _)| covered(*range))
        {
            return;
        }
        // Load some margin so panning does not reload at once.
        let margin = if cursor_only { 0 } else { span / 2 };
        let range = (from.saturating_sub(margin), until.saturating_add(margin));
        let (tx, rx) = mpsc::channel();
        let (store, ctx) = (model.depth.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(store.load_window(range.0, range.1));
            ctx.request_repaint();
        });
        self.depth_loading = Some((range, rx));
    }

    fn sync(&mut self) {
        if self.settings.smoothing_secs != self.applied_smoothing {
            self.applied_smoothing = self.settings.smoothing_secs;
            self.model.set_smoothing(self.applied_smoothing as usize);
        }
        let min_size = self.settings.bubble_min_trade_size;
        if self.applied_bubble_min_trade_size != min_size {
            self.applied_bubble_min_trade_size = min_size;
            self.model.filtered_bubble_volume = self.model.tape.as_ref()
                .filter(|_| min_size > 0.0)
                .map(|tape| tape.filtered_volume(&self.model.axis, self.model.trade_volume.len(), min_size));
        }
    }
}

fn heatmap_study_controls(ui: &mut egui::Ui, settings: &mut Settings, has_depth: bool, has_tape: bool) {
    use heatmap_studies::Study;

    ui.indent("heatmap-studies", |ui| {
        for study in Study::ALL {
            let enabled = study == Study::Resting || (has_depth && (!study.requires_trades() || has_tape));
            let mut label = egui::RichText::new(study.label());
            if study != Study::Resting {
                label = label.color(chart::study_color(study, 1.0));
            }
            let response = ui.add_enabled(enabled, egui::Checkbox::new(&mut settings.heat_layers[study.index()], label));
            if enabled && study.requires_trades() {
                response.on_hover_text(match study {
                    Study::Net => "Added minus estimated cancellations per fixed 5-second window. Blue is positive, lilac is negative.",
                    _ => "Subtracts matching executions up to 10 ms before the depth decrease.",
                });
            } else if !enabled {
                response.on_disabled_hover_text("Requires event-time depth and, for cancellation estimates, a trade tape.");
            }
        }
        let mut has_markers = false;
        for (index, label) in ["Minimum resting increase / 5 s", "Minimum resting decrease / 5 s", "Minimum estimated cancelled / 5 s"]
            .into_iter().enumerate()
        {
            let study = heatmap_studies::MARKER_STUDIES[index];
            if has_depth && settings.heat_layer_enabled(study) && (!study.requires_trades() || has_tape) {
                has_markers = true;
                let response = ui.add(egui::Slider::new(&mut settings.flow_marker_minimum[index], 1..=10_000)
                    .logarithmic(true).text(label));
                if matches!(study, Study::Added | Study::Decreased) {
                    response.on_hover_text("Compare resting size at the start and end of the window. The marker is placed where the change crossed the threshold and held through the close.");
                }
            }
        }
        if has_markers {
            ui.add(egui::Slider::new(&mut settings.flow_marker_limit, 1..=100).text("Maximum markers per layer"));
        }
        if has_depth && settings.heat_layer_enabled(Study::Resting) {
            let filter = &mut settings.heat_studies;
            ui.checkbox(&mut filter.filter_levels, "Filter price levels");
            if filter.filter_levels {
                ui.add(egui::Slider::new(&mut filter.min_size, 0..=10_000)
                    .logarithmic(true).text("Minimum level size"));
                let hold_time = egui::Slider::from_get_set(0.0..=1800.0, |seconds| {
                    if let Some(seconds) = seconds {
                        filter.hold_ms = (seconds * 1_000.0).round() as u32;
                    }
                    f64::from(filter.hold_ms) / 1_000.0
                })
                .logarithmic(true)
                .smallest_positive(0.001)
                .max_decimals(3)
                .suffix(" s")
                .text("Minimum hold time");
                ui.add(hold_time).on_hover_text(
                    "Show a bid or ask level after its size has been positive and met the minimum for this long. Enter up to 1800 seconds (30 minutes).",
                );
            }
        }
    });
}

fn trade_bubble_controls(ui: &mut egui::Ui, settings: &mut Settings, has_tape: bool) {
    ui.checkbox(&mut settings.show_trade_bubbles, "Trade bubbles");
    ui.add_enabled_ui(settings.show_trade_bubbles, |ui| {
        ui.add_enabled(
            has_tape,
            egui::DragValue::new(&mut settings.bubble_min_trade_size)
                .range(0.0..=f64::MAX)
                .prefix("Min trade size "),
        )
        .on_hover_text("Minimum size of each trade included in a bubble. 0 includes all trades.")
        .on_disabled_hover_text("Single-trade filtering requires raw trades.");
        ui.add(
            egui::DragValue::new(&mut settings.bubble_min_displayed_volume)
                .range(0.0..=f64::MAX)
                .prefix("Min displayed volume "),
        )
        .on_hover_text("Minimum total volume or absolute Delta of the final bubble.");
        ui.add(egui::Slider::new(&mut settings.bubble_aggregation, 0..=100).text("Smart aggregation"))
            .on_hover_text("Higher values merge dots farther apart on screen. 0 shows the base dots.");
        ui.add(egui::Slider::new(&mut settings.bubble_size, 0..=100).text("Dot size"));
        ui.add(egui::Slider::new(&mut settings.bubble_transparency, 0..=100).text("Transparency"));
        ui.checkbox(&mut settings.bubble_3d, "3D bubbles");
        ui.checkbox(&mut settings.bubble_colorblind, "Cyan / violet bubbles")
            .on_hover_text("Use cyan for buys and violet for sells. The default uses Bookmap's green and red.");
        ui.checkbox(&mut settings.bubble_delta, "Delta clustering");
    });
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.input(|input| input.viewport().close_requested()) {
            return;
        }
        self.poll_market();
        let ctx = ui.ctx().clone();
        self.poll_bars(&ctx);
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        self.bar_dialog(&ctx);
        self.metrics_window(&ctx);
        self.trades_window(&ctx);
        self.sync();
        if self.replay.picking && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.replay.picking = false;
        }
        if let Some(source) = self.ticks.clone() {
            self.replay.shortcuts(&ctx, &mut self.model);
            let heatmap = self.settings.mode == ChartMode::Heatmap;
            self.replay.update(&mut self.model, &source, &ctx, heatmap);
        }
        self.chart.sync_replay(self.model.replay_cut_x());
        self.sync_depth(&ctx);
        if self.settings.show_equity && !self.model.result.equity.is_empty() {
            egui::Panel::bottom("equity")
                .resizable(true)
                .default_size(150.0)
                .show(ui, |ui| chart::equity_panel(ui, &self.model, &self.chart));
        }
        let chart = egui::CentralPanel::default()
            .show(ui, |ui| {
                chart::price_panel(
                    ui,
                    &self.model,
                    &mut self.settings,
                    &mut self.chart,
                    self.selected_trade,
                    &mut self.vp,
                    self.replay.picking,
                )
            })
            .inner;
        if chart.response.clicked() && self.toolbar_popup.take().is_some() {
            ctx.request_repaint();
        }
        if let Some(index) = chart.inner {
            let heatmap = self.settings.mode == ChartMode::Heatmap;
            self.replay.start(&mut self.model, index, heatmap);
            self.chart.follow_new_replay();
            ctx.request_repaint();
        }
    }
}

fn metrics_rows(result: &BacktestResult) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&result.metrics) else {
        return Vec::new();
    };
    // serde_json maps iterate alphabetically; keep the headline numbers on top.
    const HEAD: [&str; 8] = [
        "net_pnl",
        "return_pct",
        "trades",
        "win_rate",
        "profit_factor",
        "sharpe",
        "max_drawdown",
        "max_drawdown_pct",
    ];
    let fmt = |v: &serde_json::Value| match v {
        serde_json::Value::Number(n) if n.is_f64() => format!("{:.4}", n.as_f64().unwrap_or(0.0)),
        serde_json::Value::Null => "∞".to_string(),
        other => other.to_string(),
    };
    let mut rows: Vec<(String, String)> = HEAD
        .iter()
        .filter_map(|k| map.get(*k).map(|v| (k.to_string(), fmt(v))))
        .collect();
    rows.extend(
        map.iter()
            .filter(|(k, _)| !HEAD.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), fmt(v))),
    );
    // The strategy's own tallies (`Strategy::counters`), prefixed so they read apart.
    rows.extend(result.counters.iter().map(|(k, v)| {
        let v = if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v:.4}") };
        (format!("· {k}"), v)
    }));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::{OrderFlowEvent, Side};

    fn sample_result() -> BacktestResult {
        BacktestResult {
            metrics: Default::default(),
            trades: Vec::new(),
            equity: Vec::new(),
            footprints: vec![accelerando_core::testkit::bar(0).price(100.0).build()],
            liquidity_heatmap: Default::default(),
            series: Vec::new(),
            counters: Default::default(),
            tick_size: 0.25,
            multiplier: 50.0,
        }
    }

    #[test]
    fn viewer_can_show_backtest_bars_while_market_data_is_loading() {
        let ctx = egui::Context::default();
        let options = ViewerOptions::default();
        let mut viewer = Viewer::new(sample_result(), options.clone(), &[], None, DepthStore::default());
        let (release, wait) = mpsc::channel();
        viewer.load_market(MarketLoad {
            load: Box::new(move || {
                wait.recv().map_err(|error| error.to_string())?;
                let events = PreparedEventData::from_events(vec![
                    OrderFlowEvent::Contract { tick_size: 0.25, multiplier: 50.0 },
                    OrderFlowEvent::Trade { ts_ns: SEC_NS / 2, price: 100.0, size: 310.0, aggressor: Side::Buy },
                ])?;
                Ok((Some(Arc::new(events)), DepthStore::default()))
            }),
            enable_trade_controls: true,
        }, options, &ctx);

        viewer.poll_market();
        assert!(viewer.market_loading.is_some());
        assert_eq!(viewer.model.bars().len(), 1);
        assert_eq!(viewer.model.xmap.mode(), AxisMode::Bars);
        assert!(!viewer.model.book_visible());
        assert!(viewer.ticks.is_none());
        assert!(viewer.rebar.is_none());

        release.send(()).unwrap();
        let loaded = viewer.market_loading.take().unwrap()
            .recv_timeout(std::time::Duration::from_secs(5)).unwrap().unwrap();
        viewer.apply_market(loaded);
        assert_eq!(viewer.model.bars().len(), 1);
        assert!(viewer.model.book_visible());
        assert!(viewer.model.big_trades.available());
        assert!(viewer.ticks.is_some());
        assert!(viewer.rebar.is_some());
        assert_eq!(viewer.settings.smoothing_secs, 3);
        assert_eq!(viewer.chart.x1, viewer.model.xmap.total());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        viewer.poll_bars(&ctx);
        while viewer.pending.is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
            viewer.poll_bars(&ctx);
        }
        assert_eq!(viewer.current_bar_label(), "5m");
        assert_eq!(viewer.model.result.footprints.len(), 1);
    }

    #[test]
    fn toolbar_switches_modes_and_gear_overlays_work_in_bar_mode() {
        let result = sample_result();
        let quotes = data::trade_quotes(&[OrderFlowEvent::Trade {
            ts_ns: 500_000_000,
            price: 100.0,
            size: 10.0,
            aggressor: Side::Buy,
        }]);
        let mut viewer = Viewer::new(result, Default::default(), &quotes, None, Default::default());
        let ctx = egui::Context::default();
        let frame = |viewer: &mut Viewer, events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| viewer.toolbar(ui));
            output.textures_delta.clear();
            output.shapes
        };
        let click = |viewer: &mut Viewer, label| {
            // Popups place their contents after the first layout pass.
            frame(viewer, Vec::new());
            let pos = frame(viewer, Vec::new())
                .into_iter()
                .find_map(|entry| match entry.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.rect.center().to_vec2())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing toolbar control: {label}"));
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(viewer, vec![egui::Event::PointerMoved(pos), button(true)]);
            frame(viewer, vec![button(false)]);
        };

        assert!(viewer.settings.mode == ChartMode::Bar);
        assert!(!viewer.settings.show_bbo && !viewer.settings.show_trade_bubbles);
        click(&mut viewer, "Heatmap");
        assert!(viewer.settings.mode == ChartMode::Heatmap);
        assert!(viewer.settings.show_bbo && viewer.settings.show_trade_bubbles);
        assert!(viewer.toolbar_popup.is_none());

        click(&mut viewer, "Bar");
        assert!(viewer.settings.mode == ChartMode::Bar);
        assert!(!viewer.settings.show_bbo && !viewer.settings.show_trade_bubbles);
        click(&mut viewer, "⚙");
        assert!(viewer.toolbar_popup == Some(ToolbarPopup::Heatmap));
        click(&mut viewer, "Trade bubbles");
        assert!(!viewer.settings.show_bbo && viewer.settings.show_trade_bubbles);
        click(&mut viewer, "Estimated Bid / Ask lines");
        assert!(viewer.settings.mode == ChartMode::Bar);
        assert!(viewer.settings.show_bbo && viewer.settings.show_trade_bubbles);

        click(&mut viewer, "Bar");
        assert!(!viewer.settings.show_bbo && !viewer.settings.show_trade_bubbles);

        viewer.model.start_replay(0);
        viewer.chart.follow_replay = false;
        let before = (viewer.chart.x0, viewer.chart.x1);
        click(&mut viewer, "Follow");
        assert!(viewer.chart.follow_replay);
        assert_ne!((viewer.chart.x0, viewer.chart.x1), before);
        assert!((viewer.chart.x1 - viewer.chart.x0 - (before.1 - before.0)).abs() < 1e-9);
    }
}
