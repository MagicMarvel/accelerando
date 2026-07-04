//! Offline JPEG footprint snapshots for trade review and AI-vision workflows.

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use ab_glyph::{FontArc, PxScale};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use imageproc::drawing::{
    draw_filled_rect_mut, draw_hollow_rect_mut, draw_line_segment_mut, draw_polygon_mut,
    draw_text_mut,
};
use imageproc::point::Point;
use imageproc::rect::Rect;
use serde::{Deserialize, Serialize};

use crate::footprint::{Footprint, Level, Plot};
use crate::result::{BacktestResult, Trade, TradeReason};

const MARGIN_L: f32 = 62.0;
const MARGIN_R: f32 = 14.0;
const MARGIN_T: f32 = 58.0;
const MARGIN_B: f32 = 28.0;

/// Which trade outcomes are eligible for sampling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TradeOutcomeFilter {
    #[default]
    All,
    Losers,
    Winners,
}

impl TradeOutcomeFilter {
    pub fn matches(self, pnl: f64) -> bool {
        match self {
            TradeOutcomeFilter::All => true,
            TradeOutcomeFilter::Losers => pnl < 0.0,
            TradeOutcomeFilter::Winners => pnl >= 0.0,
        }
    }
}

/// Controls trade snapshot sampling and JPEG rendering.
#[derive(Clone, Debug)]
pub struct TradeImageOptions {
    /// Bars before the trade entry footprint.
    pub pre_bars: usize,
    /// Bars after the trade exit footprint.
    pub post_bars: usize,
    /// Deterministic sampling seed.
    pub seed: u64,
    /// Restrict sampling to winning or losing trades.
    pub outcome_filter: TradeOutcomeFilter,
    /// Horizontal pixels per footprint column.
    pub column_width: u32,
    /// Target plot height used to scale price rows.
    pub target_plot_height: u32,
    /// Lower bound for one tick row in pixels.
    pub min_row_height: f32,
    /// Upper bound for one tick row in pixels.
    pub max_row_height: f32,
    /// JPEG quality, clamped to 1..=100.
    pub jpeg_quality: u8,
    /// Optional font path. If absent, common monospace system fonts are tried.
    pub font_path: Option<PathBuf>,
}

impl Default for TradeImageOptions {
    fn default() -> Self {
        Self {
            pre_bars: 10,
            post_bars: 6,
            seed: 42,
            outcome_filter: TradeOutcomeFilter::default(),
            column_width: 60,
            target_plot_height: 1300,
            min_row_height: 8.0,
            max_row_height: 16.0,
            jpeg_quality: 92,
            font_path: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeImageManifest {
    pub run: String,
    pub strategy: String,
    pub seed: u64,
    pub window: TradeImageWindow,
    pub run_metrics: TradeImageRunMetrics,
    pub samples: Vec<TradeImageSample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeImageWindow {
    pub pre: usize,
    pub post: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeImageRunMetrics {
    pub net_pnl: f64,
    pub trades: usize,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub sharpe: f64,
    pub max_drawdown: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeImageSample {
    pub file: String,
    pub trade_index: usize,
    pub dir: i32,
    pub outcome: String,
    pub pnl: f64,
    pub entry_px: f64,
    pub exit_px: f64,
    pub stop: Option<f64>,
    pub target: Option<f64>,
    pub reason: String,
    pub max_adverse_ticks: f64,
    pub entry_ts_ns: i64,
    pub exit_ts_ns: i64,
    pub bar_start: usize,
    pub bar_end: usize,
    pub image_width: u32,
    pub image_height: u32,
    /// (close - open, aggressor delta) of the bars immediately before the entry bar, oldest
    /// first. Lets reviewers correlate outcomes with the retest approach without re-rendering.
    #[serde(default)]
    pub approach_bars: Vec<(f64, f64)>,
}

#[derive(Debug)]
pub enum TradeImageError {
    Io(std::io::Error),
    Image(image::ImageError),
    Json(serde_json::Error),
    Font(String),
    NoTrades,
    NoFootprints,
}

impl fmt::Display for TradeImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TradeImageError::Io(e) => write!(f, "{e}"),
            TradeImageError::Image(e) => write!(f, "{e}"),
            TradeImageError::Json(e) => write!(f, "{e}"),
            TradeImageError::Font(msg) => write!(f, "{msg}"),
            TradeImageError::NoTrades => write!(f, "backtest produced no trades"),
            TradeImageError::NoFootprints => write!(f, "backtest kept no footprints"),
        }
    }
}

impl Error for TradeImageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            TradeImageError::Io(e) => Some(e),
            TradeImageError::Image(e) => Some(e),
            TradeImageError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TradeImageError {
    fn from(value: std::io::Error) -> Self {
        TradeImageError::Io(value)
    }
}

impl From<image::ImageError> for TradeImageError {
    fn from(value: image::ImageError) -> Self {
        TradeImageError::Image(value)
    }
}

impl From<serde_json::Error> for TradeImageError {
    fn from(value: serde_json::Error) -> Self {
        TradeImageError::Json(value)
    }
}

/// Sample trades from a result, render each surrounding footprint window to JPEG, and write
/// `index.json` beside the images.
pub fn sample_trade_footprint_jpegs(
    run_label: &str,
    strategy: &str,
    result: &BacktestResult,
    count: usize,
    out_dir: impl AsRef<Path>,
    options: TradeImageOptions,
) -> Result<TradeImageManifest, TradeImageError> {
    if result.trades.is_empty() {
        return Err(TradeImageError::NoTrades);
    }
    if result.footprints.is_empty() {
        return Err(TradeImageError::NoFootprints);
    }

    std::fs::create_dir_all(out_dir.as_ref())?;
    let font = load_font(options.font_path.as_deref())?;

    let mut idx: Vec<usize> = (0..result.trades.len())
        .filter(|&i| options.outcome_filter.matches(result.trades[i].pnl))
        .collect();
    if idx.is_empty() {
        return Err(TradeImageError::NoTrades);
    }
    let mut rng = Rng::new(options.seed);
    rng.shuffle(&mut idx);
    idx.truncate(count.min(result.trades.len()));
    idx.sort_unstable();

    let mut samples = Vec::with_capacity(idx.len());
    for (seq, &trade_index) in idx.iter().enumerate() {
        let trade = &result.trades[trade_index];
        let entry_i = nearest_footprint(&result.footprints, trade.entry_ts_ns);
        let exit_i = nearest_footprint(&result.footprints, trade.exit_ts_ns);
        let start = entry_i.saturating_sub(options.pre_bars);
        let end = (entry_i.max(exit_i) + options.post_bars + 1).min(result.footprints.len());
        let bars = &result.footprints[start..end];
        let win = RenderWindow {
            label: run_label,
            bars,
            start_index: start,
            tick_size: result.tick_size,
            trade,
            entry_bar: entry_i - start,
            exit_bar: exit_i
                .saturating_sub(start)
                .min(bars.len().saturating_sub(1)),
        };
        let image = render_window(&win, &font, &options);
        let width = image.width();
        let height = image.height();
        let outcome = if trade.pnl >= 0.0 { "win" } else { "loss" };
        let file = format!(
            "{}_{:02}_t{:03}_{}_{:+.0}.jpg",
            safe_file_label(run_label),
            seq,
            trade_index,
            outcome,
            trade.pnl
        );
        let path = out_dir.as_ref().join(&file);
        write_jpeg(&path, image, options.jpeg_quality)?;

        let approach_bars = result.footprints[entry_i.saturating_sub(3)..entry_i]
            .iter()
            .map(|fp| (fp.close - fp.open, fp.delta))
            .collect();
        samples.push(TradeImageSample {
            file,
            trade_index,
            dir: trade.dir,
            outcome: outcome.to_string(),
            pnl: trade.pnl,
            entry_px: trade.entry_px,
            exit_px: trade.exit_px,
            stop: trade.stop,
            target: trade.target,
            reason: format!("{:?}", trade.reason),
            max_adverse_ticks: trade.max_adverse_ticks,
            entry_ts_ns: trade.entry_ts_ns,
            exit_ts_ns: trade.exit_ts_ns,
            bar_start: start,
            bar_end: end.saturating_sub(1),
            image_width: width,
            image_height: height,
            approach_bars,
        });
    }

    let m = &result.metrics;
    let manifest = TradeImageManifest {
        run: run_label.to_string(),
        strategy: strategy.to_string(),
        seed: options.seed,
        window: TradeImageWindow {
            pre: options.pre_bars,
            post: options.post_bars,
        },
        run_metrics: TradeImageRunMetrics {
            net_pnl: m.net_pnl,
            trades: m.trades,
            win_rate: m.win_rate,
            profit_factor: m.profit_factor,
            sharpe: m.sharpe,
            max_drawdown: m.max_drawdown,
        },
        samples,
    };

    let index_path = out_dir.as_ref().join("index.json");
    std::fs::write(index_path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(manifest)
}

struct RenderWindow<'a> {
    label: &'a str,
    bars: &'a [Footprint],
    start_index: usize,
    tick_size: f64,
    trade: &'a Trade,
    entry_bar: usize,
    exit_bar: usize,
}

#[derive(Clone, Copy)]
struct Layout {
    tick: f64,
    pmin: f64,
    pmax: f64,
    nrows: i64,
    row_h: f32,
    col_w: f32,
    plot_w: f32,
    plot_h: f32,
    width: u32,
    height: u32,
    show_ladder: bool,
}

impl Layout {
    fn x_of(&self, bar: usize) -> f32 {
        MARGIN_L + bar as f32 * self.col_w
    }

    fn y_of(&self, price: f64) -> f32 {
        MARGIN_T + ((self.pmax - price) / self.tick) as f32 * self.row_h
    }
}

fn render_window(win: &RenderWindow<'_>, font: &FontArc, options: &TradeImageOptions) -> RgbImage {
    let layout = layout(win, options);
    let mut img = RgbImage::from_pixel(layout.width, layout.height, rgb(251, 252, 254));

    header(&mut img, win, &layout, font);
    grid(&mut img, &layout, font);

    for (bar, fp) in win.bars.iter().enumerate() {
        for plot in &fp.plots {
            if let Plot::PriceBox {
                low,
                high,
                span,
                color,
                text,
                ..
            } = plot
            {
                price_box(
                    &mut img, &layout, font, bar, *low, *high, *span, color, text,
                );
            }
        }
    }

    for (bar, fp) in win.bars.iter().enumerate() {
        let x0 = layout.x_of(bar);
        draw_line_segment_mut(
            &mut img,
            (x0, MARGIN_T),
            (x0, MARGIN_T + layout.plot_h),
            rgb(238, 241, 246),
        );
        candle(&mut img, &layout, fp, x0);
        if layout.show_ladder {
            ladder(&mut img, &layout, font, fp, x0 + layout.col_w * 0.5);
        }
        poc_ring(&mut img, &layout, fp, x0);

        let label_every = ((28.0 / layout.row_h).ceil() as usize).max(3);
        if bar % label_every == 0 {
            text_center(
                &mut img,
                font,
                x0 + layout.col_w * 0.5,
                MARGIN_T + layout.plot_h + 10.0,
                8.5,
                rgb(110, 120, 138),
                &hhmm(fp.ts_first_ns),
            );
        }
    }

    for (bar, fp) in win.bars.iter().enumerate() {
        for plot in &fp.plots {
            if let Plot::Marker {
                price, color, text, ..
            } = plot
            {
                marker(&mut img, &layout, font, bar, *price, color, text);
            }
        }
    }

    trade_overlay(&mut img, win, &layout, font);
    img
}

fn layout(win: &RenderWindow<'_>, options: &TradeImageOptions) -> Layout {
    let tick = if win.tick_size > 0.0 {
        win.tick_size
    } else {
        0.25
    };
    let mut pmin = f64::INFINITY;
    let mut pmax = f64::NEG_INFINITY;
    let mut grow = |p: f64| {
        if p.is_finite() {
            pmin = pmin.min(p);
            pmax = pmax.max(p);
        }
    };

    for fp in win.bars {
        grow(fp.high);
        grow(fp.low);
        for lv in &fp.ladder {
            grow(lv.price);
        }
        for plot in &fp.plots {
            if let Plot::PriceBox { low, high, .. } = plot {
                grow(*low);
                grow(*high);
            }
        }
    }
    grow(win.trade.entry_px);
    grow(win.trade.exit_px);
    if let Some(stop) = win.trade.stop {
        grow(stop);
    }
    if let Some(target) = win.trade.target {
        grow(target);
    }

    if !pmin.is_finite() || !pmax.is_finite() {
        pmin = win.trade.entry_px - tick;
        pmax = win.trade.entry_px + tick;
    }
    pmin -= tick;
    pmax += tick;

    let nrows = (((pmax - pmin) / tick).round() as i64).max(1) + 1;
    let row_h = (options.target_plot_height as f32 / nrows as f32)
        .clamp(options.min_row_height, options.max_row_height);
    let col_w = options.column_width.max(30) as f32;
    let plot_w = win.bars.len().max(1) as f32 * col_w;
    let plot_h = nrows as f32 * row_h;
    let width = (MARGIN_L + plot_w + MARGIN_R).ceil().max(1.0) as u32;
    let height = (MARGIN_T + plot_h + MARGIN_B).ceil().max(1.0) as u32;

    Layout {
        tick,
        pmin,
        pmax,
        nrows,
        row_h,
        col_w,
        plot_w,
        plot_h,
        width,
        height,
        show_ladder: row_h >= 8.0,
    }
}

fn header(img: &mut RgbImage, win: &RenderWindow<'_>, layout: &Layout, font: &FontArc) {
    let t = win.trade;
    let dir = if t.dir > 0 { "LONG" } else { "SHORT" };
    let dir_color = if t.dir > 0 {
        rgb(22, 163, 74)
    } else {
        rgb(220, 38, 38)
    };
    let outcome = if t.pnl >= 0.0 { "WIN" } else { "LOSS" };
    let out_color = if t.pnl >= 0.0 {
        rgb(22, 163, 74)
    } else {
        rgb(220, 38, 38)
    };

    draw_text(img, font, 10.0, 8.0, 14.0, rgb(31, 41, 55), win.label);
    draw_text(img, font, 10.0, 31.0, 11.0, dir_color, dir);
    draw_text(
        img,
        font,
        56.0,
        31.0,
        11.0,
        rgb(75, 85, 99),
        &format!(
            "entry {:.2}  exit {:.2}  stop {}  tgt {}  {}",
            t.entry_px,
            t.exit_px,
            opt_price(t.stop),
            opt_price(t.target),
            reason(t.reason)
        ),
    );
    let right = layout.width as f32 - 10.0;
    text_right(
        img,
        font,
        right,
        8.0,
        13.0,
        out_color,
        &format!("{outcome} {:+.0}", t.pnl),
    );
    text_right(
        img,
        font,
        right,
        31.0,
        10.0,
        rgb(107, 114, 128),
        &format!(
            "bars {}..{}  MAE {:.1}t",
            win.start_index,
            win.start_index + win.bars.len().saturating_sub(1),
            t.max_adverse_ticks
        ),
    );
}

fn grid(img: &mut RgbImage, layout: &Layout, font: &FontArc) {
    let label_every = ((28.0 / layout.row_h).ceil() as i64).max(1);
    let base = (layout.pmin / layout.tick).round() as i64;
    for r in 0..layout.nrows {
        let price = (base + r) as f64 * layout.tick;
        let y = layout.y_of(price);
        if (base + r) % label_every == 0 {
            draw_line_segment_mut(
                img,
                (MARGIN_L, y),
                (MARGIN_L + layout.plot_w, y),
                rgb(230, 235, 242),
            );
            text_right(
                img,
                font,
                MARGIN_L - 4.0,
                y - 6.0,
                9.0,
                rgb(138, 148, 166),
                &format!("{price:.2}"),
            );
        }
    }
}

fn candle(img: &mut RgbImage, layout: &Layout, fp: &Footprint, x0: f32) {
    let color = if fp.close >= fp.open {
        rgb(22, 163, 74)
    } else {
        rgb(220, 38, 38)
    };
    let cx = x0 + 4.0;
    let yh = layout.y_of(fp.high);
    let yl = layout.y_of(fp.low);
    let yo = layout.y_of(fp.open);
    let yc = layout.y_of(fp.close);
    draw_line_segment_mut(img, (cx, yh), (cx, yl), color);
    let top = yo.min(yc);
    let bot = yo.max(yc);
    let h = (bot - top).max(layout.row_h * 0.5);
    fill_rect(img, cx - 2.0, top, 4.0, h, color);
}

fn ladder(img: &mut RgbImage, layout: &Layout, font: &FontArc, fp: &Footprint, cx: f32) {
    let size = (layout.row_h - 2.0).clamp(5.5, 11.0);
    for lv in &fp.ladder {
        let y = layout.y_of(lv.price) - size * 0.55;
        let (sell_color, buy_color) = imbalance_colors(lv);
        text_right(
            img,
            font,
            cx - 3.0,
            y,
            size,
            sell_color,
            &fmt_vol(lv.sell_vol),
        );
        draw_text(
            img,
            font,
            cx + 3.0,
            y,
            size,
            buy_color,
            &fmt_vol(lv.buy_vol),
        );
    }
}

fn poc_ring(img: &mut RgbImage, layout: &Layout, fp: &Footprint, x0: f32) {
    let y = layout.y_of(fp.poc) - layout.row_h * 0.5;
    hollow_rect(
        img,
        x0 + 6.0,
        y,
        layout.col_w - 12.0,
        layout.row_h.max(2.0),
        rgb(245, 158, 11),
    );
}

#[allow(clippy::too_many_arguments)]
fn price_box(
    img: &mut RgbImage,
    layout: &Layout,
    font: &FontArc,
    end_bar: usize,
    low: f64,
    high: f64,
    span: usize,
    color: &str,
    text: &str,
) {
    let c = parse_color(color, rgb(37, 99, 235));
    let start_bar = end_bar.saturating_sub(span.saturating_sub(1));
    let x = layout.x_of(start_bar);
    let x_end = (MARGIN_L + layout.plot_w).min(layout.x_of(end_bar) + layout.col_w);
    let w = (x_end - x).max(layout.col_w);
    let yt = layout.y_of(high);
    let yb = layout.y_of(low);
    let y = yt.min(yb);
    let h = (yb - yt).abs().max(2.0);
    blend_rect(img, x, y, w, h, c, 0.10);
    hollow_rect(img, x, y, w, h, c);
    if !text.is_empty() {
        draw_text(img, font, x + 3.0, y + 2.0, 9.0, c, text);
    }
}

fn marker(
    img: &mut RgbImage,
    layout: &Layout,
    font: &FontArc,
    bar: usize,
    price: f64,
    color: &str,
    text: &str,
) {
    let c = parse_color(color, rgb(37, 99, 235));
    let cx = layout.x_of(bar) + layout.col_w * 0.5;
    let cy = layout.y_of(price);
    let r = 4;
    draw_polygon_mut(
        img,
        &[
            Point::new(cx.round() as i32, cy.round() as i32 - r),
            Point::new(cx.round() as i32 + r, cy.round() as i32),
            Point::new(cx.round() as i32, cy.round() as i32 + r),
            Point::new(cx.round() as i32 - r, cy.round() as i32),
        ],
        c,
    );
    if !text.is_empty() {
        draw_text(img, font, cx + 7.0, cy - 9.0, 8.0, c, text);
    }
}

fn trade_overlay(img: &mut RgbImage, win: &RenderWindow<'_>, layout: &Layout, font: &FontArc) {
    let t = win.trade;
    let x_left = MARGIN_L;
    let x_right = MARGIN_L + layout.plot_w;

    if let Some(stop) = t.stop {
        let y = layout.y_of(stop);
        dashed_hline(img, x_left, x_right, y, rgb(220, 38, 38));
        text_right(
            img,
            font,
            x_right - 2.0,
            y - 12.0,
            9.0,
            rgb(220, 38, 38),
            &format!("stop {stop:.2}"),
        );
    }
    if let Some(target) = t.target {
        let y = layout.y_of(target);
        dashed_hline(img, x_left, x_right, y, rgb(22, 163, 74));
        text_right(
            img,
            font,
            x_right - 2.0,
            y + 2.0,
            9.0,
            rgb(22, 163, 74),
            &format!("tgt {target:.2}"),
        );
    }

    let ex = layout.x_of(win.entry_bar) + layout.col_w * 0.5;
    let ey = layout.y_of(t.entry_px);
    let xx = layout.x_of(win.exit_bar) + layout.col_w * 0.5;
    let xy = layout.y_of(t.exit_px);
    draw_line_segment_mut(img, (ex, ey), (xx, xy), rgb(37, 99, 235));
    entry_triangle(img, ex, ey, t.dir > 0, rgb(37, 99, 235));
    draw_text(
        img,
        font,
        ex + 8.0,
        ey - 4.0,
        9.0,
        rgb(37, 99, 235),
        "entry",
    );

    fill_rect(img, xx - 4.0, xy - 4.0, 8.0, 8.0, rgb(17, 24, 39));
    draw_text(img, font, xx + 6.0, xy - 5.0, 9.0, rgb(17, 24, 39), "exit");
}

fn entry_triangle(img: &mut RgbImage, cx: f32, cy: f32, up: bool, color: Rgb<u8>) {
    let r = 6;
    let points = if up {
        [
            Point::new(cx.round() as i32, cy.round() as i32 - r),
            Point::new(cx.round() as i32 + r, cy.round() as i32 + r),
            Point::new(cx.round() as i32 - r, cy.round() as i32 + r),
        ]
    } else {
        [
            Point::new(cx.round() as i32, cy.round() as i32 + r),
            Point::new(cx.round() as i32 + r, cy.round() as i32 - r),
            Point::new(cx.round() as i32 - r, cy.round() as i32 - r),
        ]
    };
    draw_polygon_mut(img, &points, color);
}

fn write_jpeg(path: &Path, image: RgbImage, quality: u8) -> Result<(), TradeImageError> {
    let file = File::create(path)?;
    let mut encoder = JpegEncoder::new_with_quality(file, quality.clamp(1, 100));
    encoder.encode_image(&DynamicImage::ImageRgb8(image))?;
    Ok(())
}

fn load_font(path: Option<&Path>) -> Result<FontArc, TradeImageError> {
    let mut paths = Vec::new();
    if let Some(path) = path {
        paths.push(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("ACCELERANDO_FONT") {
        paths.push(PathBuf::from(path));
    }
    paths.extend([
        PathBuf::from(r"C:\Windows\Fonts\consola.ttf"),
        PathBuf::from(r"C:\Windows\Fonts\Consola.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf"),
        PathBuf::from("/Library/Fonts/Menlo.ttc"),
    ]);

    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(font) = FontArc::try_from_vec(bytes) {
            return Ok(font);
        }
    }

    Err(TradeImageError::Font(
        "could not load a monospace font; set ACCELERANDO_FONT or TradeImageOptions::font_path"
            .to_string(),
    ))
}

/// The footprint whose [first,last] span contains `ts_ns`, else the nearest by first timestamp.
pub fn nearest_footprint(footprints: &[Footprint], ts_ns: i64) -> usize {
    let mut best = 0usize;
    let mut best_d = i64::MAX;
    for (i, fp) in footprints.iter().enumerate() {
        if fp.ts_first_ns <= ts_ns && ts_ns <= fp.ts_last_ns {
            return i;
        }
        let d = (fp.ts_first_ns - ts_ns).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

fn imbalance_colors(lv: &Level) -> (Rgb<u8>, Rgb<u8>) {
    let buy_dom = lv.buy_vol >= lv.sell_vol * 3.0 && lv.buy_vol > 0.0;
    let sell_dom = lv.sell_vol >= lv.buy_vol * 3.0 && lv.sell_vol > 0.0;
    let sell = if sell_dom {
        rgb(185, 28, 28)
    } else {
        rgb(200, 107, 107)
    };
    let buy = if buy_dom {
        rgb(21, 128, 61)
    } else {
        rgb(111, 174, 134)
    };
    (sell, buy)
}

fn fmt_vol(v: f64) -> String {
    let v = v.round();
    if v >= 10_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}k", v / 1000.0)
    } else {
        format!("{v:.0}")
    }
}

fn hhmm(ts_ns: i64) -> String {
    if ts_ns <= 0 {
        return String::new();
    }
    let secs = ts_ns / 1_000_000_000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    format!("{h:02}:{m:02}")
}

fn reason(r: TradeReason) -> &'static str {
    match r {
        TradeReason::Signal => "signal",
        TradeReason::StopLoss => "stop",
        TradeReason::TakeProfit => "target",
        TradeReason::EndOfData => "eod",
    }
}

fn opt_price(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "-".to_string())
}

fn safe_file_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "run".to_string()
    } else {
        out
    }
}

fn draw_text(
    img: &mut RgbImage,
    font: &FontArc,
    x: f32,
    y: f32,
    size: f32,
    color: Rgb<u8>,
    text: &str,
) {
    draw_text_mut(
        img,
        color,
        x.round() as i32,
        y.round() as i32,
        PxScale::from(size),
        font,
        text,
    );
}

fn text_right(
    img: &mut RgbImage,
    font: &FontArc,
    x: f32,
    y: f32,
    size: f32,
    color: Rgb<u8>,
    text: &str,
) {
    draw_text(
        img,
        font,
        x - approx_text_width(text, size),
        y,
        size,
        color,
        text,
    );
}

fn text_center(
    img: &mut RgbImage,
    font: &FontArc,
    x: f32,
    y: f32,
    size: f32,
    color: Rgb<u8>,
    text: &str,
) {
    draw_text(
        img,
        font,
        x - approx_text_width(text, size) * 0.5,
        y,
        size,
        color,
        text,
    );
}

fn approx_text_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.62
}

fn fill_rect(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, color: Rgb<u8>) {
    draw_filled_rect_mut(img, rect(x, y, w, h), color);
}

fn hollow_rect(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, color: Rgb<u8>) {
    draw_hollow_rect_mut(img, rect(x, y, w, h), color);
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::at(x.round() as i32, y.round() as i32)
        .of_size(w.max(1.0).round() as u32, h.max(1.0).round() as u32)
}

fn blend_rect(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, color: Rgb<u8>, alpha: f32) {
    let alpha = alpha.clamp(0.0, 1.0);
    let x0 = x.floor().max(0.0) as u32;
    let y0 = y.floor().max(0.0) as u32;
    let x1 = (x + w).ceil().min(img.width() as f32).max(0.0) as u32;
    let y1 = (y + h).ceil().min(img.height() as f32).max(0.0) as u32;
    for yy in y0..y1 {
        for xx in x0..x1 {
            let px = img.get_pixel_mut(xx, yy);
            px.0 = [
                blend_channel(px[0], color[0], alpha),
                blend_channel(px[1], color[1], alpha),
                blend_channel(px[2], color[2], alpha),
            ];
        }
    }
}

fn blend_channel(base: u8, over: u8, alpha: f32) -> u8 {
    (base as f32 * (1.0 - alpha) + over as f32 * alpha)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn dashed_hline(img: &mut RgbImage, x1: f32, x2: f32, y: f32, color: Rgb<u8>) {
    let mut x = x1;
    while x < x2 {
        let end = (x + 7.0).min(x2);
        draw_line_segment_mut(img, (x, y), (end, y), color);
        x += 12.0;
    }
}

fn parse_color(value: &str, fallback: Rgb<u8>) -> Rgb<u8> {
    let s = value.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return fallback;
    }
    let Ok(v) = u32::from_str_radix(hex, 16) else {
        return fallback;
    };
    rgb(
        ((v >> 16) & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        (v & 0xff) as u8,
    )
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb<u8> {
    Rgb([r, g, b])
}
