//! Serializable backtest output consumed by the CLI summary, the JSON result file and the web UI.

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

/// The full result of one backtest run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BacktestResult {
    pub metrics: Metrics,
    pub trades: Vec<Trade>,
    pub equity: Vec<EquityPoint>,
    /// All enriched footprints (for chart rendering). May be large.
    pub footprints: Vec<Footprint>,
    pub tick_size: f64,
    pub multiplier: f64,
}
