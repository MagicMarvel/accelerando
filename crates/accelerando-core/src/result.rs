//! Serializable backtest output consumed by downstream apps, JSON artifacts and the web UI.

use serde::{Deserialize, Serialize};

use crate::footprint::Footprint;
use crate::metrics::Metrics;

/// Why a position was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeReason {
    Signal,
    StopLoss,
    TakeProfit,
    EndOfData,
}

/// A completed round-trip trade.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub entry_ts_ns: i64,
    pub exit_ts_ns: i64,
    /// +1 long, -1 short.
    pub dir: i32,
    pub qty: f64,
    pub entry_px: f64,
    pub exit_px: f64,
    /// Stop-loss price the position carried while open (`None` if no stop was set).
    #[serde(default)]
    pub stop: Option<f64>,
    /// Take-profit price the position carried while open (`None` if no target was set).
    #[serde(default)]
    pub target: Option<f64>,
    /// Maximum adverse excursion while the trade was open, in account currency.
    #[serde(default)]
    pub max_adverse_excursion: f64,
    /// Maximum adverse excursion while the trade was open, in ticks.
    #[serde(default)]
    pub max_adverse_ticks: f64,
    /// Realized PnL in account currency, net of commission.
    pub pnl: f64,
    pub reason: TradeReason,
}

/// A point on the equity curve, sampled at each footprint close (mark-to-market).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts_ns: i64,
    pub equity: f64,
    /// Drawdown from the running peak, in account currency (<= 0).
    pub drawdown: f64,
}

/// Resting liquidity at one price level for a Bookmap-style heatmap snapshot.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct LiquidityLevel {
    pub price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

/// A sampled order-book depth state. Levels are sparse and sorted by price.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiquiditySnapshot {
    pub ts_ns: i64,
    pub levels: Vec<LiquidityLevel>,
}

/// Bookmap-style resting-liquidity history, sampled on footprint closes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiquidityHeatmap {
    pub snapshots: Vec<LiquiditySnapshot>,
}

/// The full result of one backtest run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BacktestResult {
    pub metrics: Metrics,
    pub trades: Vec<Trade>,
    pub equity: Vec<EquityPoint>,
    /// All enriched footprints (for chart rendering). May be large.
    pub footprints: Vec<Footprint>,
    /// Resting order-book liquidity snapshots for Bookmap-style heatmap rendering.
    #[serde(default)]
    pub liquidity_heatmap: LiquidityHeatmap,
    pub tick_size: f64,
    pub multiplier: f64,
}

/// Lightweight metadata and metrics for one run in a multi-run experiment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentRunSummary {
    pub id: String,
    pub label: String,
    pub strategy: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub metrics: Metrics,
}

/// A complete multi-run experiment result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentResult {
    pub runs: Vec<ExperimentRun>,
}

/// One complete experiment run, including chartable backtest data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentRun {
    pub summary: ExperimentRunSummary,
    pub result: BacktestResult,
}
