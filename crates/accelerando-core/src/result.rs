//! Serializable backtest output consumed by downstream apps, JSON artifacts and the web UI.

use std::collections::BTreeMap;

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
    /// Free-form setup tag attached atomically through `EntryIntent::tagged` (features,
    /// location type, ...), for per-setup analysis of the trade list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A per-run line overlay sampled once per footprint (e.g. a VWAP and its bands).
/// `points` are `(footprint index, value)` pairs, sorted by index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Series {
    pub id: String,
    #[serde(default)]
    pub color: String,
    pub points: Vec<(u32, f64)>,
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

/// Bookmap-style resting-liquidity history on a fixed time grid.
///
/// The engine snapshots the book once per `interval_ns` bucket in which it changed (1 second, see
/// `engine::LIQUIDITY_HEATMAP_INTERVAL_NS`). Each snapshot is stamped with its bucket start and
/// stays valid until the next snapshot; an empty snapshot marks a cleared book.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiquidityHeatmap {
    /// Sampling bucket width in nanoseconds (`0` in results written before fixed-grid sampling).
    #[serde(default)]
    pub interval_ns: i64,
    pub snapshots: Vec<LiquiditySnapshot>,
}

/// Named diagnostic tallies a strategy keeps while it runs ("setups seen", "rejected: risk too
/// wide", ...). Return them from `Strategy::counters`; runners print them under the result table
/// and the viewers show them next to the metrics.
///
/// ```ignore
/// self.counters.inc("departures");
/// self.counters.add("risk_ticks_sum", risk);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Counters(pub BTreeMap<String, f64>);

impl Counters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one to `name`.
    pub fn inc(&mut self, name: &str) {
        self.add(name, 1.0);
    }

    /// Add `amount` to `name`.
    pub fn add(&mut self, name: &str, amount: f64) {
        *self.0.entry(name.to_string()).or_insert(0.0) += amount;
    }

    /// Overwrite `name` (gauges such as "max open setups").
    pub fn set(&mut self, name: &str, value: f64) {
        self.0.insert(name.to_string(), value);
    }

    pub fn get(&self, name: &str) -> f64 {
        self.0.get(name).copied().unwrap_or(0.0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, f64)> {
        self.0.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// `name value | name value` on one line; integers print without decimals.
    pub fn summary(&self) -> String {
        self.iter()
            .map(|(k, v)| {
                if v.fract() == 0.0 && v.abs() < 1e15 {
                    format!("{k} {}", v as i64)
                } else {
                    format!("{k} {v:.2}")
                }
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
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
    /// Per-run line overlays collected via `VisualOutput::series`.
    #[serde(default)]
    pub series: Vec<Series>,
    /// The strategy's own diagnostic tallies (`Strategy::counters`).
    #[serde(default, skip_serializing_if = "Counters::is_empty")]
    pub counters: Counters,
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
