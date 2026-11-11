//! Native egui/eframe viewer for a [`BacktestResult`].
//!
//! ```no_run
//! # let result: accelerando_core::BacktestResult = unimplemented!();
//! accelerando_gui::view(result, accelerando_gui::ViewerOptions::default()).unwrap();
//! ```
//!
//! The window shows the price panel (1-second liquidity heatmap, smoothed best-bid / best-ask
//! lines, bars, trades and strategy overlays) and an optional equity panel on the same x axis.
//! Display options, the equity toggle, metrics and the trade list live behind the Settings
//! button. The x axis is either one equal-width slot per bar or proportional to time; either
//! way closed-market time is never drawn over.
//!
//! The heatmap needs `Pipeline.keep_liquidity_heatmap = true` and bars need
//! `Pipeline.keep_footprints = true` in the runner that produced the result.

pub mod bars;
mod chart;
pub mod data;
mod overlay;
pub mod replay;
mod vp;

use std::sync::mpsc;

use accelerando_core::{BacktestResult, Footprint};
use eframe::egui;

use chart::ChartState;
pub use bars::{aggregate, BarKind, BarSpec, Rebar};
pub use data::{AxisMode, TimeZone};
use data::{BarAxis, BookGrid, HeatMetric, TimeAxis, XMap, SEC_NS};
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
    /// is proportional to time (needed for the 1-second heatmap). `None` picks Time when the
    /// result carries a liquidity heatmap and Bars otherwise.
    pub axis_mode: Option<AxisMode>,
    /// What `result.footprints` are (shown in the bar-type chooser), e.g. `BarSpec::parse("reverse:16")`.
    pub bar_spec: Option<BarSpec>,
    /// Builds bars of another type for display (usually [`aggregate`] over the raw trades). Without
    /// it the bar-type chooser is disabled.
    pub rebar: Option<Rebar>,
    /// Open showing these bars instead of the strategy's own (needs `rebar`).
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
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(options.title.clone())
            .with_inner_size([1500.0, 900.0]),
        ..Default::default()
    };
    let title = options.title.clone();
    eframe::run_native(
        &title,
        native,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            install_cjk_font(&cc.egui_ctx);
            Ok(Box::new(Viewer::new(result, options)))
        }),
    )
    .map_err(|e| std::io::Error::other(e.to_string()))
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

/// Bar-axis pauses longer than this become an empty spacer (overnight, weekend, lunch break).
const BAR_GAP_SECS: i64 = 1800;
/// Width of that spacer in bar slots.
const BAR_GAP_SLOTS: f64 = 3.0;
/// Bars in view when the window opens (wide enough for the footprint view).
const INITIAL_BARS: f64 = 20.0;

/// Everything derived once from the result at startup.
pub(crate) struct Model {
    pub result: BacktestResult,
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
    pub heat_ref: f32,
    /// Time zone of every time shown.
    pub tz: TimeZone,
    pub price_decimals: usize,
    /// Median footprint duration, to translate time spans into bar counts.
    pub median_bar_secs: f64,
    /// Bars shown instead of the strategy's own `result.footprints` (display only).
    pub display: Option<(BarSpec, Vec<Footprint>)>,
}

impl Model {
    fn new(result: BacktestResult, options: &ViewerOptions) -> Self {
        let snapshots = &result.liquidity_heatmap.snapshots;
        let mut intervals: Vec<(i64, i64)> = Vec::with_capacity(
            snapshots.len() + result.footprints.len() + result.equity.len() + result.trades.len(),
        );
        intervals.extend(snapshots.iter().map(|s| (s.ts_ns, s.ts_ns)));
        intervals.extend(result.footprints.iter().map(|f| (f.ts_first_ns, f.ts_last_ns)));
        intervals.extend(result.equity.iter().map(|e| (e.ts_ns, e.ts_ns)));
        intervals.extend(result.trades.iter().map(|t| (t.entry_ts_ns, t.entry_ts_ns)));
        intervals.extend(result.trades.iter().map(|t| (t.exit_ts_ns, t.exit_ts_ns)));
        let axis = TimeAxis::build(intervals, options.max_gap_secs.max(1) * SEC_NS);
        let bounds = axis.bounds();
        let grid = BookGrid::build(&axis, snapshots);
        let heat_ref = data::reference_size(snapshots);
        let tick = result.tick_size;
        let price_decimals = if tick > 0.0 && tick < 1.0 {
            ((-tick.log10()).ceil() as usize).clamp(1, 6)
        } else {
            2
        };
        let mode = options.axis_mode.unwrap_or(
            if result.footprints.is_empty() || !snapshots.is_empty() {
                AxisMode::Time
            } else {
                AxisMode::Bars
            },
        );
        let mut model = Self {
            bid_smooth: Vec::new(),
            ask_smooth: Vec::new(),
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
            result,
            axis,
            bounds,
            grid,
            heat_ref,
            tz: TimeZone::from_hours(options.utc_offset_hours),
            price_decimals,
            median_bar_secs: 60.0,
            display: None,
        };
        model.set_axis_mode(mode);
        model.set_smoothing(options.smoothing_secs as usize);
        model
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
        for tick in ticks {
            replay.apply(tick, full);
        }
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

    /// x of the replay position: the chart is blank to the right of it.
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
        self.bid_smooth = data::smooth(&self.grid.bid, &self.bounds, window_secs);
        self.ask_smooth = data::smooth(&self.grid.ask, &self.bounds, window_secs);
    }

    pub fn has_heatmap(&self) -> bool {
        !self.result.liquidity_heatmap.snapshots.is_empty()
    }

    /// The heatmap and bid/ask lines live on the per-second grid: time axis only.
    pub fn book_visible(&self) -> bool {
        self.has_heatmap() && self.xmap.mode() == AxisMode::Time
    }

    /// A time span expressed in x units of the current axis.
    pub fn span_x(&self, secs: f64) -> f64 {
        match self.xmap.mode() {
            AxisMode::Time => secs,
            AxisMode::Bars => (secs / self.median_bar_secs).max(5.0),
        }
    }
}

/// User-facing display toggles.
pub(crate) struct Settings {
    pub axis_mode: AxisMode,
    pub show_heat: bool,
    pub show_bbo: bool,
    pub show_bars: bool,
    /// Draw bars as footprints once they are wide enough.
    pub show_footprint: bool,
    pub show_trades: bool,
    pub show_overlays: bool,
    /// Point markers strategies attach to bars (e.g. labelled entry dots).
    pub show_markers: bool,
    pub show_equity: bool,
    pub heat_metric: HeatMetric,
    pub heat_smooth: bool,
    pub heat_contrast: f32,
    pub smoothing_secs: u32,
    pub red_up: bool,
}

struct Viewer {
    model: Model,
    settings: Settings,
    chart: ChartState,
    applied_smoothing: u32,
    metrics_rows: Vec<(String, String)>,
    selected_trade: Option<usize>,
    settings_open: bool,
    metrics_open: bool,
    trades_open: bool,
    /// Bars the strategy ran on, and the host's bar builder.
    strategy_spec: Option<BarSpec>,
    rebar: Option<Rebar>,
    bar_dialog_open: bool,
    bar_draft: BarSpec,
    pending: Option<(BarSpec, mpsc::Receiver<Result<Vec<Footprint>, String>>)>,
    bar_error: Option<String>,
    /// Requested at the first frame (needs the egui context).
    startup_bars: Option<BarSpec>,
    /// Volume profiles: strategy ones and the ones drawn by hand.
    vp: VpTool,
    /// Tick-by-tick replay and the host's tick reader.
    replay: Replay,
    ticks: Option<TickSource>,
}

impl Viewer {
    fn new(result: BacktestResult, options: ViewerOptions) -> Self {
        let metrics_rows = metrics_rows(&result);
        let model = Model::new(result, &options);
        let settings = Settings {
            axis_mode: model.xmap.mode(),
            show_heat: model.has_heatmap(),
            show_bbo: model.has_heatmap(),
            show_bars: true,
            show_footprint: true,
            show_trades: true,
            show_overlays: true,
            show_markers: false,
            show_equity: !model.result.equity.is_empty(),
            heat_metric: HeatMetric::Total,
            heat_smooth: true,
            heat_contrast: 2.0,
            smoothing_secs: options.smoothing_secs,
            red_up: options.red_up,
        };
        let mut chart = ChartState::new(model.xmap.total());
        match options.initial_span_secs.filter(|s| *s > 0.0) {
            Some(span) => chart.show_latest(model.span_x(span)),
            // Like the web studio: open on the latest bars, zoomed in far enough for footprints.
            None if !model.bars().is_empty() => {
                let bars = INITIAL_BARS.min(model.bars().len() as f64);
                let span = match model.xmap.mode() {
                    AxisMode::Bars => bars,
                    AxisMode::Time => bars * model.median_bar_secs,
                };
                chart.show_latest(span.max(5.0));
            }
            None => {}
        }
        Self {
            model,
            settings,
            chart,
            applied_smoothing: options.smoothing_secs,
            metrics_rows,
            selected_trade: None,
            settings_open: false,
            metrics_open: false,
            trades_open: false,
            strategy_spec: options.bar_spec,
            rebar: options.rebar.clone(),
            bar_dialog_open: false,
            bar_draft: options.bar_spec.unwrap_or(BarSpec::new(BarKind::Time, 60)),
            pending: None,
            bar_error: None,
            startup_bars: options.display_bars.filter(|spec| Some(*spec) != options.bar_spec),
            vp: VpTool::default(),
            replay: Replay::default(),
            ticks: options.ticks.clone(),
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.toggle_value(&mut self.settings_open, "⚙ Settings");
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
            if let Some((spec, _)) = &self.pending {
                ui.spinner();
                ui.label(format!("building {} bars ...", spec.label()));
            }
            ui.separator();
            ui.checkbox(&mut self.chart.auto_y, "Auto Y")
                .on_hover_text("Double-click the chart to show everything.");
            ui.separator();
            ui.toggle_value(&mut self.vp.drawing, "📊 Draw VP").on_hover_text(
                "Drag across bars to draw a volume profile. Right-click a profile to delete it, \
                 Esc to stop drawing. Profile settings are in ⚙ Settings.",
            );
            ui.label("σ");
            ui.add(
                egui::DragValue::new(&mut self.vp.smooth_ticks)
                    .range(0.0..=30.0)
                    .speed(0.1)
                    .suffix(" t"),
            )
            .on_hover_text("Gaussian smoothing of drawn profiles, in ticks (0 = raw).");
            let n = self.vp.items.len();
            if ui.add_enabled(n > 0, egui::Button::new(format!("Clear VPs ({n})"))).clicked() {
                self.vp.items.clear();
            }
            ui.separator();
            if self.model.replay_clock().is_none() {
                ui.add_enabled_ui(self.ticks.is_some(), |ui| {
                    ui.toggle_value(&mut self.replay.picking, "⏵ Replay")
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
                    egui::RichText::new(format!("⏵ {}", data::format_ts_full(clock, self.model.tz)))
                        .monospace()
                        .color(chart::REPLAY),
                );
            }
        });
        if self.model.replay_clock().is_some() {
            ui.horizontal(|ui| self.replay.controls(ui, &mut self.model));
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
                            draft.size = kind.presets()[1];
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
            Some(Some(spec)) => self.request_bars(spec, ctx),
            Some(None) => self.show_bars(None),
            None => {}
        }
    }

    /// Build `spec` bars on a background thread (they arrive in [`Self::poll_bars`]).
    fn request_bars(&mut self, spec: BarSpec, ctx: &egui::Context) {
        let Some(rebar) = self.rebar.clone() else { return };
        self.bar_error = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(rebar(&spec));
            ctx.request_repaint();
        });
        self.pending = Some((spec, rx));
    }

    fn poll_bars(&mut self) {
        let Some((spec, rx)) = &self.pending else { return };
        let spec = *spec;
        match rx.try_recv() {
            Ok(Ok(bars)) if bars.is_empty() => {
                self.pending = None;
                self.bar_error = Some(format!("{} produced no bars", spec.label()));
            }
            Ok(Ok(bars)) => {
                self.pending = None;
                self.show_bars(Some((spec, bars)));
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
        self.chart.set_total(xmap.total());
        self.chart.show_range(xmap.to_x(t0), xmap.to_x(t1));
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let has_heat = self.model.has_heatmap();
        let has_equity = !self.model.result.equity.is_empty();
        let trade_count = self.model.result.trades.len();
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_pos(egui::pos2(10.0, 44.0))
            .show(ctx, |ui| {
                let s = &mut self.settings;
                ui.strong("Panels");
                ui.add_enabled_ui(has_equity, |ui| {
                    ui.checkbox(&mut s.show_equity, "Equity curve");
                });
                ui.checkbox(&mut self.metrics_open, "Metrics");
                ui.checkbox(&mut self.trades_open, format!("Trade list ({trade_count})"));

                ui.separator();
                ui.strong("Chart");
                ui.checkbox(&mut s.show_bars, "Bars");
                ui.add_enabled_ui(s.show_bars, |ui| {
                    ui.checkbox(&mut s.show_footprint, "Footprint when zoomed in");
                });
                ui.checkbox(&mut s.show_trades, "Trades");
                ui.checkbox(&mut s.red_up, "Red up");

                ui.weak("Indicators (overlays, markers, volume profile) are listed
in the chart's top-left corner.");

                ui.separator();
                ui.strong("Order book");
                let book = has_heat && s.axis_mode == AxisMode::Time;
                if !has_heat {
                    ui.weak("No order-book data in this result.");
                } else if !book {
                    ui.weak("Turn on the time-proportional x axis to show the book.");
                }
                let mut time_axis = s.axis_mode == AxisMode::Time;
                if ui
                    .checkbox(&mut time_axis, "Time-proportional x axis")
                    .on_hover_text(
                        "Off: every bar has the same width. On: x is proportional to time, \
                         which the 1-second heatmap and bid/ask lines need.",
                    )
                    .changed()
                {
                    s.axis_mode = if time_axis { AxisMode::Time } else { AxisMode::Bars };
                }
                ui.add_enabled_ui(book, |ui| {
                    ui.checkbox(&mut s.show_heat, "Heatmap 1s");
                    ui.checkbox(&mut s.show_bbo, "Bid / Ask lines");
                    ui.add(
                        egui::Slider::new(&mut s.smoothing_secs, 0..=300)
                            .logarithmic(true)
                            .suffix(" s")
                            .text("line smoothing"),
                    )
                    .on_hover_text(
                        "Width of the zero-lag (centered) smoothing kernel applied to the \
                         best-bid and best-ask lines. 0 shows the raw 1-second quotes.",
                    );
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
                    ui.checkbox(&mut s.heat_smooth, "Smooth heatmap");
                    ui.add(egui::Slider::new(&mut s.heat_contrast, 0.5..=4.0).text("contrast"));
                });
            });
        self.settings_open = open;
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

    /// Apply settings that require rebuilding derived data.
    fn sync(&mut self) {
        if self.settings.smoothing_secs != self.applied_smoothing {
            self.applied_smoothing = self.settings.smoothing_secs;
            self.model.set_smoothing(self.applied_smoothing as usize);
        }
        if self.settings.axis_mode != self.model.xmap.mode() {
            // Keep the same stretch of time in view across the switch.
            let xmap = &self.model.xmap;
            let (t0, t1) = (xmap.to_ts(self.chart.x0), xmap.to_ts(self.chart.x1));
            self.model.set_axis_mode(self.settings.axis_mode);
            self.chart.auto_y = true;
            let xmap = &self.model.xmap;
            self.chart.set_total(xmap.total());
            self.chart.show_range(xmap.to_x(t0), xmap.to_x(t1));
        }
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        let ctx = ui.ctx().clone();
        if let Some(spec) = self.startup_bars.take() {
            self.bar_draft = spec;
            self.request_bars(spec, &ctx);
        }
        self.poll_bars();
        self.settings_window(&ctx);
        self.bar_dialog(&ctx);
        self.metrics_window(&ctx);
        self.trades_window(&ctx);
        self.sync();
        if self.replay.picking && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.replay.picking = false;
        }
        if let Some(source) = self.ticks.clone() {
            self.replay.shortcuts(&ctx, &mut self.model);
            self.replay.update(&mut self.model, &mut self.chart, &source, &ctx);
        }
        if self.settings.show_equity && !self.model.result.equity.is_empty() {
            egui::Panel::bottom("equity")
                .resizable(true)
                .default_size(150.0)
                .show(ui, |ui| chart::equity_panel(ui, &self.model, &self.chart));
        }
        let picked = egui::CentralPanel::default()
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
        if let Some(index) = picked {
            self.replay.start(&mut self.model, index);
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
    rows
}
