
//! ML dataset extraction: the **same** feature values the strategies trade from (published by
//! [`crate::TradeFlowFeatures`] into `fp.values`) are read back by one shared
//! [`FeatureExtractor`] and paired with forward-looking labels. There is deliberately no second
//! feature implementation for training — a column either comes from a footprint field or from a
//! `tf_` key, so training and inference cannot drift apart.
//!
//! Labels are the only place allowed to look forward: they are computed offline over the
//! completed footprint sequence, strictly *after* the causal features were attached.

use std::io::{self, Write};

use accelerando_core::market_time::{civil_from_days, eastern_day_minute};
use accelerando_core::Footprint;

use crate::features as tf;

/// One exported sample (spec §9): feature vector + optional forward label.
#[derive(Clone, Debug)]
pub struct MlDatasetRow {
    pub timestamp_ns: i64,
    pub session_date: String,
    pub feature_names: Vec<String>,
    pub feature_values: Vec<f64>,
    pub label: Option<f64>,
}

/// Column source: a raw footprint field or a published `tf_` value.
enum ColumnSource {
    Field(fn(&Footprint) -> f64),
    Value(&'static str),
    /// Direction-normalized column: `f(dir, fp)` where `dir` is the aggressor direction
    /// (+1 when the bar's delta is positive). Lets long and short samples train together.
    Signed(fn(i32, &Footprint) -> Option<f64>),
}

/// The canonical, ordered feature schema shared by export and any later inference path.
pub struct FeatureExtractor {
    columns: Vec<(&'static str, ColumnSource)>,
}

impl Default for FeatureExtractor {
    fn default() -> Self {
        Self::new()
    }
}

fn value(fp: &Footprint, key: &str) -> Option<f64> {
    fp.values.get(key).copied()
}

impl FeatureExtractor {
    pub fn new() -> Self {
        use ColumnSource::*;
        let columns: Vec<(&'static str, ColumnSource)> = vec![
            ("open", Field(|fp| fp.open)),
            ("high", Field(|fp| fp.high)),
            ("low", Field(|fp| fp.low)),
            ("close", Field(|fp| fp.close)),
            ("volume", Field(|fp| fp.volume)),
            ("trades", Field(|fp| fp.trades as f64)),
            ("delta", Field(|fp| fp.delta)),
            ("delta_abs", Field(|fp| fp.delta.abs())),
            ("buy_volume", Value(tf::BUY_VOLUME)),
            ("sell_volume", Value(tf::SELL_VOLUME)),
            ("delta_ratio", Value(tf::DELTA_RATIO)),
            ("trade_rate", Value(tf::TRADE_RATE)),
            ("average_trade_size", Value(tf::AVG_TRADE_SIZE)),
            ("large_trade_volume", Value(tf::LARGE_TRADE_VOLUME)),
            ("large_trade_ratio", Value(tf::LARGE_TRADE_RATIO)),
            ("delta_zscore", Value(tf::DELTA_Z)),
            ("volume_zscore", Value(tf::VOLUME_Z)),
            ("trade_rate_zscore", Value(tf::TRADE_RATE_Z)),
            ("bar_move_ticks", Value(tf::BAR_MOVE_TICKS)),
            ("bar_range_ticks", Value(tf::BAR_RANGE_TICKS)),
            ("close_location", Value(tf::CLOSE_LOCATION)),
            ("bar_move_zscore", Value(tf::BAR_MOVE_Z)),
            ("delta_efficiency", Value(tf::DELTA_EFFICIENCY)),
            ("delta_efficiency_zscore", Value(tf::DELTA_EFF_Z)),
            ("absolute_delta_efficiency", Value(tf::ABS_DELTA_EFFICIENCY)),
            (
                "absolute_delta_efficiency_zscore",
                Value(tf::ABS_DELTA_EFF_Z),
            ),
            ("cvd", Value(tf::CVD)),
            ("cvd_change_1", Value(tf::CVD_CHANGE_1)),
            ("cvd_change_3", Value(tf::CVD_CHANGE_3)),
            ("cvd_change_5", Value(tf::CVD_CHANGE_5)),
            ("cvd_slope_5", Value(tf::CVD_SLOPE_5)),
            ("cvd_slope_10", Value(tf::CVD_SLOPE_10)),
            ("distance_to_vwap_ticks", Value(tf::DIST_VWAP_TICKS)),
            (
                "distance_to_recent_high_ticks",
                Value(tf::DIST_RECENT_HIGH_TICKS),
            ),
            (
                "distance_to_recent_low_ticks",
                Value(tf::DIST_RECENT_LOW_TICKS),
            ),
            ("recent_range_position", Value(tf::RECENT_RANGE_POS)),
            (
                "distance_to_session_high_ticks",
                Value(tf::DIST_SESSION_HIGH_TICKS),
            ),
            (
                "distance_to_session_low_ticks",
                Value(tf::DIST_SESSION_LOW_TICKS),
            ),
            ("positive_imbalance_count", Value(tf::POS_IMBALANCE_COUNT)),
            ("negative_imbalance_count", Value(tf::NEG_IMBALANCE_COUNT)),
            (
                "max_positive_imbalance_ratio",
                Value(tf::MAX_POS_IMBALANCE_RATIO),
            ),
            (
                "max_negative_imbalance_ratio",
                Value(tf::MAX_NEG_IMBALANCE_RATIO),
            ),
            (
                "positive_stack_max_layers",
                Value(tf::POS_STACK_MAX_LAYERS),
            ),
            (
                "negative_stack_max_layers",
                Value(tf::NEG_STACK_MAX_LAYERS),
            ),
            ("atr", Value(tf::ATR)),
            ("realized_volatility", Value(tf::REALIZED_VOL)),
            ("time_of_day_minutes", Value(tf::TIME_OF_DAY_MINUTES)),
            // Direction-normalized ("aggressor direction is positive") variants, spec §9.1.
            (
                "signed_delta_zscore",
                Signed(|dir, fp| value(fp, tf::DELTA_Z).map(|v| dir as f64 * v)),
            ),
            (
                "signed_delta_ratio",
                Signed(|dir, fp| value(fp, tf::DELTA_RATIO).map(|v| dir as f64 * v)),
            ),
            (
                "signed_bar_move_ticks",
                Signed(|dir, fp| value(fp, tf::BAR_MOVE_TICKS).map(|v| dir as f64 * v)),
            ),
            (
                "signed_bar_move_zscore",
                Signed(|dir, fp| value(fp, tf::BAR_MOVE_Z).map(|v| dir as f64 * v)),
            ),
            (
                "signed_delta_efficiency",
                Signed(|dir, fp| value(fp, tf::DELTA_EFFICIENCY).map(|v| dir as f64 * v)),
            ),
            (
                "signed_close_location",
                Signed(|dir, fp| {
                    value(fp, tf::CLOSE_LOCATION).map(|v| if dir >= 0 { v } else { 1.0 - v })
                }),
            ),
            (
                "signed_range_position",
                Signed(|dir, fp| {
                    value(fp, tf::RECENT_RANGE_POS).map(|v| if dir >= 0 { v } else { 1.0 - v })
                }),
            ),
            (
                "signed_distance_to_range_edge_ticks",
                Signed(|dir, fp| {
                    if dir >= 0 {
                        value(fp, tf::DIST_RECENT_HIGH_TICKS)
                    } else {
                        value(fp, tf::DIST_RECENT_LOW_TICKS)
                    }
                }),
            ),
            (
                "signed_cvd_change_3",
                Signed(|dir, fp| value(fp, tf::CVD_CHANGE_3).map(|v| dir as f64 * v)),
            ),
            (
                "signed_distance_to_vwap_ticks",
                Signed(|dir, fp| value(fp, tf::DIST_VWAP_TICKS).map(|v| dir as f64 * v)),
            ),
        ];
        Self { columns }
    }

    pub fn names(&self) -> Vec<String> {
        self.columns.iter().map(|(n, _)| n.to_string()).collect()
    }

    /// Extract the full feature vector for one enriched footprint. Missing values come back as
    /// `NaN` — never silently zero. `dir` is the aggressor direction used for the signed
    /// columns (conventionally `sign(delta)` of the signal bar).
    pub fn extract(&self, fp: &Footprint, dir: i32) -> Vec<f64> {
        self.columns
            .iter()
            .map(|(_, src)| match src {
                ColumnSource::Field(f) => f(fp),
                ColumnSource::Value(key) => value(fp, key).unwrap_or(f64::NAN),
                ColumnSource::Signed(f) => f(dir, fp).unwrap_or(f64::NAN),
            })
            .collect()
    }
}

// ---- labels ------------------------------------------------------------------------------------

/// Continuation label (spec §9.1) for the strong-flow bar at `i`, direction `dir`
/// (+1 = aggressive buying): within `horizon_bars`, does price touch
/// `close + dir·target_ticks` before `close - dir·stop_ticks`? A bar touching both counts as
/// stop-first (conservative). Neither touched → `None`.
#[derive(Clone, Copy, Debug)]
pub struct ContinuationLabelConfig {
    pub horizon_bars: usize,
    pub target_ticks: f64,
    pub stop_ticks: f64,
    /// Candidate filter: `|delta_zscore| >=` this marks a strong-flow bar.
    pub candidate_delta_z: f64,
}

impl Default for ContinuationLabelConfig {
    fn default() -> Self {
        Self {
            horizon_bars: 5,
            target_ticks: 8.0,
            stop_ticks: 5.0,
            candidate_delta_z: 1.5,
        }
    }
}

pub fn continuation_label(
    fps: &[Footprint],
    i: usize,
    dir: i32,
    tick: f64,
    cfg: &ContinuationLabelConfig,
) -> Option<f64> {
    let anchor = fps[i].close;
    let d = dir as f64;
    let target = anchor + d * cfg.target_ticks * tick;
    let stop = anchor - d * cfg.stop_ticks * tick;
    for fp in fps.iter().skip(i + 1).take(cfg.horizon_bars) {
        let hit_stop = if dir > 0 { fp.low <= stop } else { fp.high >= stop };
        let hit_target = if dir > 0 {
            fp.high >= target
        } else {
            fp.low <= target
        };
        if hit_stop {
            return Some(0.0); // conservative: stop resolves first on an ambiguous bar
        }
        if hit_target {
            return Some(1.0);
        }
    }
    None
}

/// Failure-reversal label (spec §9.2) for the strong-flow bar at `i`, aggressor direction `dir`.
///
/// - `mfe` = best move in the aggressor direction over the next `response_window_bars`.
/// - label 1: `mfe <= response_mfe_atr_fraction × atr` **and** the reverse target
///   (`close - dir·target_ticks`) is touched before the continuation level
///   (`close + dir·continuation_ticks`) within `horizon_bars`.
/// - label 0: the continuation level is touched first (ties count as continuation —
///   conservative for the reversal thesis).
/// - `None`: anything else.
#[derive(Clone, Copy, Debug)]
pub struct FailureLabelConfig {
    pub response_window_bars: usize,
    pub horizon_bars: usize,
    pub target_ticks: f64,
    pub continuation_ticks: f64,
    pub response_mfe_atr_fraction: f64,
    pub candidate_delta_z: f64,
}

impl Default for FailureLabelConfig {
    fn default() -> Self {
        Self {
            response_window_bars: 3,
            horizon_bars: 10,
            target_ticks: 8.0,
            continuation_ticks: 8.0,
            response_mfe_atr_fraction: 0.20,
            candidate_delta_z: 1.5,
        }
    }
}

pub fn failure_label(
    fps: &[Footprint],
    i: usize,
    dir: i32,
    tick: f64,
    atr: f64,
    cfg: &FailureLabelConfig,
) -> Option<f64> {
    let anchor = fps[i].close;
    let d = dir as f64;
    let mut mfe = 0.0f64;
    for fp in fps.iter().skip(i + 1).take(cfg.response_window_bars) {
        let favorable = if dir > 0 {
            fp.high - anchor
        } else {
            anchor - fp.low
        };
        mfe = mfe.max(favorable);
    }
    let mfe_ok = mfe <= cfg.response_mfe_atr_fraction * atr;

    let reverse_target = anchor - d * cfg.target_ticks * tick;
    let continuation = anchor + d * cfg.continuation_ticks * tick;
    for fp in fps.iter().skip(i + 1).take(cfg.horizon_bars) {
        let hit_continuation = if dir > 0 {
            fp.high >= continuation
        } else {
            fp.low <= continuation
        };
        let hit_reverse = if dir > 0 {
            fp.low <= reverse_target
        } else {
            fp.high >= reverse_target
        };
        if hit_continuation {
            return Some(0.0); // ties resolve as continuation: conservative for the fade
        }
        if hit_reverse {
            return if mfe_ok { Some(1.0) } else { None };
        }
    }
    None
}

// ---- CSV export ---------------------------------------------------------------------------------

/// Which candidate bars a dataset keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatasetKind {
    /// Strong-flow bars labeled with [`continuation_label`].
    Continuation,
    /// Strong-flow bars labeled with [`failure_label`].
    FailureReversal,
}

#[derive(Clone, Debug)]
pub struct DatasetConfig {
    pub symbol: String,
    pub contract: String,
    /// Drop rows whose label is `None` (spec default). When `false`, unlabeled rows are kept
    /// with an empty label field.
    pub drop_unlabeled: bool,
    pub continuation: ContinuationLabelConfig,
    pub failure: FailureLabelConfig,
}

impl Default for DatasetConfig {
    fn default() -> Self {
        Self {
            symbol: "ES".to_string(),
            contract: String::new(),
            drop_unlabeled: true,
            continuation: ContinuationLabelConfig::default(),
            failure: FailureLabelConfig::default(),
        }
    }
}

fn session_date(ts_ns: i64) -> String {
    let (day, _) = eastern_day_minute(ts_ns);
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Build labeled rows for every strong-flow candidate bar in `fps`.
pub fn build_dataset(
    fps: &[Footprint],
    tick: f64,
    kind: DatasetKind,
    cfg: &DatasetConfig,
) -> Vec<(usize, MlDatasetRow)> {
    let extractor = FeatureExtractor::new();
    let names = extractor.names();
    let candidate_z = match kind {
        DatasetKind::Continuation => cfg.continuation.candidate_delta_z,
        DatasetKind::FailureReversal => cfg.failure.candidate_delta_z,
    };
    let mut rows = Vec::new();
    for (i, fp) in fps.iter().enumerate() {
        let Some(dz) = value(fp, tf::DELTA_Z) else {
            continue;
        };
        if dz.abs() < candidate_z {
            continue;
        }
        let dir = if dz >= 0.0 { 1 } else { -1 };
        let label = match kind {
            DatasetKind::Continuation => continuation_label(fps, i, dir, tick, &cfg.continuation),
            DatasetKind::FailureReversal => {
                let atr = value(fp, tf::ATR).unwrap_or(0.0);
                if atr <= 0.0 {
                    continue;
                }
                failure_label(fps, i, dir, tick, atr, &cfg.failure)
            }
        };
        if label.is_none() && cfg.drop_unlabeled {
            continue;
        }
        rows.push((
            i,
            MlDatasetRow {
                timestamp_ns: fp.ts_last_ns,
                session_date: session_date(fp.ts_last_ns),
                feature_names: names.clone(),
                feature_values: extractor.extract(fp, dir),
                label,
            },
        ));
    }
    rows
}

/// Write one dataset as CSV (spec §10 header shape). Missing values export as empty fields, not
/// zeros. Returns the number of data rows written.
pub fn export_csv<W: Write>(
    out: &mut W,
    fps: &[Footprint],
    tick: f64,
    kind: DatasetKind,
    cfg: &DatasetConfig,
) -> io::Result<usize> {
    let extractor = FeatureExtractor::new();
    let label_column = match kind {
        DatasetKind::Continuation => "continuation_label",
        DatasetKind::FailureReversal => "failure_reversal_label",
    };
    write!(out, "timestamp_ns,session_date,symbol,contract,bar_index")?;
    for name in extractor.names() {
        write!(out, ",{name}")?;
    }
    writeln!(out, ",{label_column}")?;

    let rows = build_dataset(fps, tick, kind, cfg);
    for (bar_index, row) in &rows {
        write!(
            out,
            "{},{},{},{},{}",
            row.timestamp_ns, row.session_date, cfg.symbol, cfg.contract, bar_index
        )?;
        for v in &row.feature_values {
            if v.is_finite() {
                write!(out, ",{v}")?;
            } else {
                write!(out, ",")?;
            }
        }
        match row.label {
            Some(l) => writeln!(out, ",{l}")?,
            None => writeln!(out, ",")?,
        }
    }
    Ok(rows.len())
}
