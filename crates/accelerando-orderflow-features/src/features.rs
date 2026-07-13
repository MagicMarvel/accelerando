//! [`TradeFlowFeatures`] — one indicator computing every shared order-flow feature on each
//! completed footprint, published into `fp.values` under the `tf_` keys below.
//!
//! Causality contract: every value on footprint `t` is a function of footprints `..=t` (and the
//! raw trades inside them) only. Rolling z-score statistics, large-trade thresholds and the
//! recent-range window use *prior* bars exclusively, so the current bar is scored against
//! history it could not have influenced. Values that cannot be computed yet (not enough
//! history, first bar of a session) are simply **not inserted** — readers must treat a missing
//! key as "no feature", never as zero.

use std::collections::VecDeque;

use accelerando_core::market_time::eastern_day_minute;
use accelerando_core::{
    Configurable, EventInterest, Footprint, Indicator, OrderFlowEvent, ParamSpec, Params,
};

// ---- 2.1 base aggressive-trade features -------------------------------------------------------
pub const BUY_VOLUME: &str = "tf_buy_volume";
pub const SELL_VOLUME: &str = "tf_sell_volume";
pub const DELTA_RATIO: &str = "tf_delta_ratio";
pub const TRADE_RATE: &str = "tf_trade_rate";
pub const AVG_TRADE_SIZE: &str = "tf_avg_trade_size";
pub const LARGE_TRADE_VOLUME: &str = "tf_large_trade_volume";
pub const LARGE_TRADE_RATIO: &str = "tf_large_trade_ratio";
pub const LARGE_TRADE_THRESHOLD: &str = "tf_large_trade_threshold";

// ---- 2.2 price-response features --------------------------------------------------------------
pub const BAR_MOVE_TICKS: &str = "tf_bar_move_ticks";
pub const BAR_RANGE_TICKS: &str = "tf_bar_range_ticks";
pub const CLOSE_LOCATION: &str = "tf_close_location";
pub const DELTA_EFFICIENCY: &str = "tf_delta_efficiency";
pub const ABS_DELTA_EFFICIENCY: &str = "tf_abs_delta_efficiency";

pub const DELTA_Z: &str = "tf_delta_zscore";
pub const VOLUME_Z: &str = "tf_volume_zscore";
pub const TRADE_RATE_Z: &str = "tf_trade_rate_zscore";
pub const BAR_MOVE_Z: &str = "tf_bar_move_zscore";
pub const DELTA_EFF_Z: &str = "tf_delta_efficiency_zscore";
pub const ABS_DELTA_EFF_Z: &str = "tf_abs_delta_efficiency_zscore";

// ---- 2.3 CVD -----------------------------------------------------------------------------------
pub const CVD: &str = "tf_cvd";
pub const CVD_CHANGE_1: &str = "tf_cvd_change_1";
pub const CVD_CHANGE_3: &str = "tf_cvd_change_3";
pub const CVD_CHANGE_5: &str = "tf_cvd_change_5";
pub const CVD_SLOPE_5: &str = "tf_cvd_slope_5";
pub const CVD_SLOPE_10: &str = "tf_cvd_slope_10";

// ---- 2.4 location features ---------------------------------------------------------------------
pub const SESSION_VWAP: &str = "tf_session_vwap";
pub const DIST_VWAP_TICKS: &str = "tf_distance_to_vwap_ticks";
pub const DIST_RECENT_HIGH_TICKS: &str = "tf_distance_to_recent_high_ticks";
pub const DIST_RECENT_LOW_TICKS: &str = "tf_distance_to_recent_low_ticks";
pub const RECENT_RANGE_POS: &str = "tf_recent_range_position";
pub const DIST_SESSION_HIGH_TICKS: &str = "tf_distance_to_session_high_ticks";
pub const DIST_SESSION_LOW_TICKS: &str = "tf_distance_to_session_low_ticks";

// ---- 2.5 footprint diagonal imbalances ---------------------------------------------------------
pub const POS_IMBALANCE_COUNT: &str = "tf_positive_imbalance_count";
pub const NEG_IMBALANCE_COUNT: &str = "tf_negative_imbalance_count";
pub const MAX_POS_IMBALANCE_RATIO: &str = "tf_max_positive_imbalance_ratio";
pub const MAX_NEG_IMBALANCE_RATIO: &str = "tf_max_negative_imbalance_ratio";
pub const POS_STACK_MAX_LAYERS: &str = "tf_positive_stack_max_layers";
pub const NEG_STACK_MAX_LAYERS: &str = "tf_negative_stack_max_layers";

// ---- context -----------------------------------------------------------------------------------
pub const ATR: &str = "tf_atr";
pub const REALIZED_VOL: &str = "tf_realized_volatility";
pub const TIME_OF_DAY_MINUTES: &str = "tf_time_of_day_minutes";

const EPSILON: f64 = 1e-9;
/// Denominator floor for delta efficiency: one contract, so a 1-tick move on 1 delta scores 1.
const DELTA_EFF_FLOOR: f64 = 1.0;
/// Histogram bucket cap for the rolling large-trade percentile (sizes clamp into the last bucket).
const SIZE_HIST_BUCKETS: usize = 4096;

/// Rolling z-score over the previous `window` observations, **excluding** the value being
/// scored: the caller gets `z(x)` computed from prior data only, then `x` joins the window.
struct RollingZ {
    window: usize,
    min_history: usize,
    buf: VecDeque<f64>,
    sum: f64,
    sum_sq: f64,
}

impl RollingZ {
    fn new(window: usize, min_history: usize) -> Self {
        Self {
            window: window.max(2),
            min_history: min_history.max(2),
            buf: VecDeque::new(),
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    /// Score `x` against the current (past-only) window, then absorb it.
    fn score_and_push(&mut self, x: f64) -> Option<f64> {
        let z = self.score(x);
        self.buf.push_back(x);
        self.sum += x;
        self.sum_sq += x * x;
        while self.buf.len() > self.window {
            if let Some(old) = self.buf.pop_front() {
                self.sum -= old;
                self.sum_sq -= old * old;
            }
        }
        z
    }

    fn score(&self, x: f64) -> Option<f64> {
        let n = self.buf.len();
        if n < self.min_history {
            return None;
        }
        let mean = self.sum / n as f64;
        let var = ((self.sum_sq - self.sum * mean) / (n - 1) as f64).max(0.0);
        let sd = var.sqrt();
        if sd < EPSILON {
            return None; // degenerate history: a z-score would be meaningless noise
        }
        Some((x - mean) / sd)
    }
}

/// A raw trade waiting to be attributed to the footprint that closes over it.
#[derive(Clone, Copy)]
struct PendingTrade {
    ts_ns: i64,
    size: f64,
}

/// Per-session (US-Eastern trading day) accumulators, reset on day rollover.
#[derive(Default)]
struct SessionState {
    day: i64,
    vwap_pv: f64,
    vwap_vol: f64,
    high: Option<f64>,
    low: Option<f64>,
    cvd: f64,
    cvd_history: Vec<f64>,
}

pub struct TradeFlowFeatures {
    tick: f64,
    recent_range_window: usize,
    atr_period: usize,
    realized_vol_window: usize,
    large_trade_mode_fixed: bool,
    large_trade_min_size: f64,
    large_trade_percentile: f64,
    large_trade_window: usize,
    imbalance_ratio: f64,
    imbalance_min_volume: f64,

    // Raw-trade capture for large-trade stats.
    pending_trades: VecDeque<PendingTrade>,
    size_hist: Vec<u32>,
    size_hist_total: u64,
    size_window: VecDeque<u16>,

    // Rolling z-score state.
    z_delta: RollingZ,
    z_volume: RollingZ,
    z_trade_rate: RollingZ,
    z_bar_move: RollingZ,
    z_delta_eff: RollingZ,
    z_abs_delta_eff: RollingZ,

    // ATR / realized volatility.
    atr_prev_close: Option<f64>,
    atr_window: VecDeque<f64>,
    atr_sum: f64,
    rv_prev_close: Option<f64>,
    ret_window: VecDeque<f64>,

    session: SessionState,
}

impl Configurable for TradeFlowFeatures {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .fixed_float("tick", 0.25)
            .int("normalization_window", 100, 10, 2000, 10)
            .int("minimum_history", 30, 2, 500, 1)
            .int("recent_range_window", 20, 2, 500, 1)
            .int("atr_period", 20, 2, 200, 1)
            .int("realized_vol_window", 20, 2, 500, 1)
            .choice("large_trade_mode", "percentile", &["percentile", "fixed"])
            .float("large_trade_min_size", 10.0, 1.0, 5000.0)
            .float("large_trade_percentile", 0.95, 0.5, 0.999)
            .int("large_trade_window", 2000, 50, 100000, 50)
            .float("imbalance_ratio", 3.0, 1.1, 20.0)
            .float("imbalance_min_volume", 5.0, 0.0, 5000.0)
    }

    fn from_params(p: &Params) -> Self {
        let window = p.usize("normalization_window", 100).max(10);
        let min_history = p.usize("minimum_history", 30).max(2).min(window);
        Self {
            tick: p.float("tick", 0.25).max(f64::EPSILON),
            recent_range_window: p.usize("recent_range_window", 20).max(2),
            atr_period: p.usize("atr_period", 20).max(2),
            realized_vol_window: p.usize("realized_vol_window", 20).max(2),
            large_trade_mode_fixed: p.str("large_trade_mode", "percentile") == "fixed",
            large_trade_min_size: p.float("large_trade_min_size", 10.0).max(1.0),
            large_trade_percentile: p.float("large_trade_percentile", 0.95).clamp(0.5, 0.999),
            large_trade_window: p.usize("large_trade_window", 2000).max(50),
            imbalance_ratio: p.float("imbalance_ratio", 3.0).max(1.0),
            imbalance_min_volume: p.float("imbalance_min_volume", 5.0).max(0.0),
            pending_trades: VecDeque::new(),
            size_hist: vec![0; SIZE_HIST_BUCKETS],
            size_hist_total: 0,
            size_window: VecDeque::new(),
            z_delta: RollingZ::new(window, min_history),
            z_volume: RollingZ::new(window, min_history),
            z_trade_rate: RollingZ::new(window, min_history),
            z_bar_move: RollingZ::new(window, min_history),
            z_delta_eff: RollingZ::new(window, min_history),
            z_abs_delta_eff: RollingZ::new(window, min_history),
            atr_prev_close: None,
            atr_window: VecDeque::new(),
            atr_sum: 0.0,
            rv_prev_close: None,
            ret_window: VecDeque::new(),
            session: SessionState::default(),
        }
    }
}

impl Indicator for TradeFlowFeatures {
    fn name(&self) -> &str {
        "trade_flow_features"
    }

    fn event_interest(&self) -> EventInterest {
        EventInterest::TRADE
    }

    fn on_event(&mut self, ev: &OrderFlowEvent) {
        if let OrderFlowEvent::Trade { ts_ns, size, .. } = *ev {
            self.pending_trades.push_back(PendingTrade {
                ts_ns,
                size: size.max(0.0),
            });
        }
    }

    fn on_footprint(&mut self, fp: &mut Footprint, history: &[Footprint]) {
        let (day, minute) = eastern_day_minute(fp.ts_last_ns);
        if day != self.session.day {
            self.session = SessionState {
                day,
                ..SessionState::default()
            };
        }

        self.base_features(fp);
        self.large_trade_features(fp);
        let (move_ticks, delta_eff, abs_delta_eff) = self.response_features(fp);
        self.zscore_features(fp, move_ticks, delta_eff, abs_delta_eff);
        self.cvd_features(fp);
        self.location_features(fp, history);
        self.imbalance_features(fp);

        let atr = self.update_atr(fp);
        fp.values.insert(ATR.to_string(), atr);
        if let Some(rv) = self.update_realized_vol(fp) {
            fp.values.insert(REALIZED_VOL.to_string(), rv);
        }
        fp.values
            .insert(TIME_OF_DAY_MINUTES.to_string(), minute as f64);

        // Fold this bar into the session trackers *after* all features were computed, so
        // session high/low distances never see the current bar.
        self.fold_session_bar(fp);
    }
}

impl TradeFlowFeatures {
    fn base_features(&self, fp: &mut Footprint) {
        // buy - sell = delta, buy + sell = volume.
        let buy = ((fp.volume + fp.delta) / 2.0).max(0.0);
        let sell = ((fp.volume - fp.delta) / 2.0).max(0.0);
        fp.values.insert(BUY_VOLUME.to_string(), buy);
        fp.values.insert(SELL_VOLUME.to_string(), sell);
        fp.values.insert(
            DELTA_RATIO.to_string(),
            fp.delta / fp.volume.max(EPSILON),
        );
        let duration_s =
            ((fp.ts_last_ns - fp.ts_first_ns) as f64 / 1e9).max(1e-3);
        fp.values
            .insert(TRADE_RATE.to_string(), fp.trades as f64 / duration_s);
        fp.values.insert(
            AVG_TRADE_SIZE.to_string(),
            fp.volume / (fp.trades.max(1) as f64),
        );
    }

    /// Large-trade volume for this bar, thresholded against the size distribution of *prior*
    /// bars' trades. Pending raw trades are attributed to this bar by timestamp; with a time
    /// aggregator that attribution is exact (the bar-closing event belongs to the next bar).
    fn large_trade_features(&mut self, fp: &mut Footprint) {
        let threshold = self.large_trade_threshold();
        let mut bar_sizes: Vec<f64> = Vec::new();
        while let Some(front) = self.pending_trades.front() {
            if front.ts_ns > fp.ts_last_ns {
                break;
            }
            bar_sizes.push(front.size);
            self.pending_trades.pop_front();
        }
        let large_vol: f64 = bar_sizes
            .iter()
            .filter(|&&s| s >= threshold)
            .sum();
        fp.values
            .insert(LARGE_TRADE_VOLUME.to_string(), large_vol);
        fp.values.insert(
            LARGE_TRADE_RATIO.to_string(),
            large_vol / fp.volume.max(EPSILON),
        );
        fp.values
            .insert(LARGE_TRADE_THRESHOLD.to_string(), threshold);
        // Absorb this bar's sizes into the rolling distribution last (past-only threshold).
        for size in bar_sizes {
            let bucket = (size.round().max(0.0) as usize).min(SIZE_HIST_BUCKETS - 1) as u16;
            self.size_window.push_back(bucket);
            self.size_hist[bucket as usize] += 1;
            self.size_hist_total += 1;
            while self.size_window.len() > self.large_trade_window {
                if let Some(old) = self.size_window.pop_front() {
                    self.size_hist[old as usize] -= 1;
                    self.size_hist_total -= 1;
                }
            }
        }
    }

    fn large_trade_threshold(&self) -> f64 {
        if self.large_trade_mode_fixed || self.size_hist_total == 0 {
            return self.large_trade_min_size;
        }
        let rank = (self.size_hist_total as f64 * self.large_trade_percentile).floor() as u64;
        let mut seen = 0u64;
        for (bucket, &count) in self.size_hist.iter().enumerate() {
            seen += u64::from(count);
            if seen > rank {
                return (bucket as f64).max(self.large_trade_min_size);
            }
        }
        self.large_trade_min_size
    }

    fn response_features(&self, fp: &mut Footprint) -> (f64, f64, f64) {
        let move_ticks = (fp.close - fp.open) / self.tick;
        let range_ticks = (fp.high - fp.low) / self.tick;
        let close_location = (fp.close - fp.low) / (fp.high - fp.low).max(self.tick);
        let eff_denominator = fp.delta.abs().max(DELTA_EFF_FLOOR);
        let delta_eff = move_ticks / eff_denominator;
        let abs_delta_eff = move_ticks.abs() / eff_denominator;
        fp.values.insert(BAR_MOVE_TICKS.to_string(), move_ticks);
        fp.values.insert(BAR_RANGE_TICKS.to_string(), range_ticks);
        fp.values
            .insert(CLOSE_LOCATION.to_string(), close_location);
        fp.values.insert(DELTA_EFFICIENCY.to_string(), delta_eff);
        fp.values
            .insert(ABS_DELTA_EFFICIENCY.to_string(), abs_delta_eff);
        (move_ticks, delta_eff, abs_delta_eff)
    }

    fn zscore_features(
        &mut self,
        fp: &mut Footprint,
        move_ticks: f64,
        delta_eff: f64,
        abs_delta_eff: f64,
    ) {
        let trade_rate = fp.values[TRADE_RATE];
        let put = |key: &str, z: Option<f64>, values: &mut std::collections::BTreeMap<String, f64>| {
            if let Some(z) = z {
                values.insert(key.to_string(), z);
            }
        };
        put(DELTA_Z, self.z_delta.score_and_push(fp.delta), &mut fp.values);
        put(VOLUME_Z, self.z_volume.score_and_push(fp.volume), &mut fp.values);
        put(
            TRADE_RATE_Z,
            self.z_trade_rate.score_and_push(trade_rate),
            &mut fp.values,
        );
        put(BAR_MOVE_Z, self.z_bar_move.score_and_push(move_ticks), &mut fp.values);
        put(
            DELTA_EFF_Z,
            self.z_delta_eff.score_and_push(delta_eff),
            &mut fp.values,
        );
        put(
            ABS_DELTA_EFF_Z,
            self.z_abs_delta_eff.score_and_push(abs_delta_eff),
            &mut fp.values,
        );
    }

    fn cvd_features(&mut self, fp: &mut Footprint) {
        self.session.cvd += fp.delta;
        let cvd = self.session.cvd;
        fp.values.insert(CVD.to_string(), cvd);
        let hist = &self.session.cvd_history; // same ET day only; cleared on rollover
        let change = |k: usize, key: &str, values: &mut std::collections::BTreeMap<String, f64>| {
            if hist.len() >= k {
                values.insert(key.to_string(), cvd - hist[hist.len() - k]);
            }
        };
        change(1, CVD_CHANGE_1, &mut fp.values);
        change(3, CVD_CHANGE_3, &mut fp.values);
        change(5, CVD_CHANGE_5, &mut fp.values);
        if hist.len() >= 5 {
            fp.values
                .insert(CVD_SLOPE_5.to_string(), (cvd - hist[hist.len() - 5]) / 5.0);
        }
        if hist.len() >= 10 {
            fp.values.insert(
                CVD_SLOPE_10.to_string(),
                (cvd - hist[hist.len() - 10]) / 10.0,
            );
        }
        self.session.cvd_history.push(cvd);
    }

    fn location_features(&self, fp: &mut Footprint, history: &[Footprint]) {
        // Recent range over the previous N *completed* bars — the current bar is excluded so a
        // wide bar cannot place itself at its own extreme.
        let window = history.len().min(self.recent_range_window);
        if window > 0 {
            let recent = &history[history.len() - window..];
            let recent_high = recent.iter().map(|b| b.high).fold(f64::MIN, f64::max);
            let recent_low = recent.iter().map(|b| b.low).fold(f64::MAX, f64::min);
            fp.values.insert(
                DIST_RECENT_HIGH_TICKS.to_string(),
                (recent_high - fp.close) / self.tick,
            );
            fp.values.insert(
                DIST_RECENT_LOW_TICKS.to_string(),
                (fp.close - recent_low) / self.tick,
            );
            fp.values.insert(
                RECENT_RANGE_POS.to_string(),
                (fp.close - recent_low) / (recent_high - recent_low).max(self.tick),
            );
        }

        // Session extremes from prior bars of the same ET day.
        if let (Some(high), Some(low)) = (self.session.high, self.session.low) {
            fp.values.insert(
                DIST_SESSION_HIGH_TICKS.to_string(),
                (high - fp.close) / self.tick,
            );
            fp.values.insert(
                DIST_SESSION_LOW_TICKS.to_string(),
                (fp.close - low) / self.tick,
            );
        }

        // Session VWAP includes the current bar (standard anchored VWAP), from exact
        // per-price ladder volume.
        let (pv, vol) = ladder_price_volume(fp);
        let vwap_pv = self.session.vwap_pv + pv;
        let vwap_vol = self.session.vwap_vol + vol;
        if vwap_vol > EPSILON {
            let vwap = vwap_pv / vwap_vol;
            fp.values.insert(SESSION_VWAP.to_string(), vwap);
            fp.values.insert(
                DIST_VWAP_TICKS.to_string(),
                (fp.close - vwap) / self.tick,
            );
        }
    }

    fn fold_session_bar(&mut self, fp: &Footprint) {
        let (pv, vol) = ladder_price_volume(fp);
        self.session.vwap_pv += pv;
        self.session.vwap_vol += vol;
        self.session.high = Some(self.session.high.map_or(fp.high, |h| h.max(fp.high)));
        self.session.low = Some(self.session.low.map_or(fp.low, |l| l.min(fp.low)));
    }

    /// ATAS-style diagonal imbalances from the bar's own volume ladder (no book data):
    /// buy imbalance at `p` when `ask(p) >= ratio × bid(p - tick)`, sell when
    /// `bid(p) >= ratio × ask(p + tick)`; both require the aggressive side to clear
    /// `imbalance_min_volume`.
    fn imbalance_features(&self, fp: &mut Footprint) {
        let keyed: Vec<(i64, f64, f64)> = fp
            .ladder
            .iter()
            .map(|lv| ((lv.price / self.tick).round() as i64, lv.buy_vol, lv.sell_vol))
            .collect();
        let mut pos_keys: Vec<i64> = Vec::new();
        let mut neg_keys: Vec<i64> = Vec::new();
        let mut max_pos_ratio = 0.0f64;
        let mut max_neg_ratio = 0.0f64;
        for (i, &(key, buy, sell)) in keyed.iter().enumerate() {
            if i > 0 && keyed[i - 1].0 == key - 1 {
                let bid_below = keyed[i - 1].2;
                if bid_below > 0.0
                    && buy >= self.imbalance_ratio * bid_below
                    && buy >= self.imbalance_min_volume
                {
                    pos_keys.push(key);
                    max_pos_ratio = max_pos_ratio.max(buy / bid_below);
                }
            }
            if i + 1 < keyed.len() && keyed[i + 1].0 == key + 1 {
                let ask_above = keyed[i + 1].1;
                if ask_above > 0.0
                    && sell >= self.imbalance_ratio * ask_above
                    && sell >= self.imbalance_min_volume
                {
                    neg_keys.push(key);
                    max_neg_ratio = max_neg_ratio.max(sell / ask_above);
                }
            }
        }
        fp.values
            .insert(POS_IMBALANCE_COUNT.to_string(), pos_keys.len() as f64);
        fp.values
            .insert(NEG_IMBALANCE_COUNT.to_string(), neg_keys.len() as f64);
        fp.values
            .insert(MAX_POS_IMBALANCE_RATIO.to_string(), max_pos_ratio);
        fp.values
            .insert(MAX_NEG_IMBALANCE_RATIO.to_string(), max_neg_ratio);
        fp.values.insert(
            POS_STACK_MAX_LAYERS.to_string(),
            longest_adjacent_run(&pos_keys) as f64,
        );
        fp.values.insert(
            NEG_STACK_MAX_LAYERS.to_string(),
            longest_adjacent_run(&neg_keys) as f64,
        );
    }

    fn update_atr(&mut self, fp: &Footprint) -> f64 {
        let tr = match self.atr_prev_close {
            Some(prev) => (fp.high - fp.low)
                .max((fp.high - prev).abs())
                .max((fp.low - prev).abs()),
            None => fp.high - fp.low,
        };
        self.atr_prev_close = Some(fp.close);
        self.atr_window.push_back(tr);
        self.atr_sum += tr;
        while self.atr_window.len() > self.atr_period {
            if let Some(old) = self.atr_window.pop_front() {
                self.atr_sum -= old;
            }
        }
        (self.atr_sum / self.atr_window.len() as f64).max(self.tick)
    }

    /// Std-dev of close-to-close moves (in ticks) over the realized-vol window.
    fn update_realized_vol(&mut self, fp: &Footprint) -> Option<f64> {
        if let Some(prev) = self.rv_prev_close {
            self.ret_window.push_back((fp.close - prev) / self.tick);
            while self.ret_window.len() > self.realized_vol_window {
                self.ret_window.pop_front();
            }
        }
        self.rv_prev_close = Some(fp.close);
        let n = self.ret_window.len();
        if n < 2 {
            return None;
        }
        let mean = self.ret_window.iter().sum::<f64>() / n as f64;
        let var = self
            .ret_window
            .iter()
            .map(|r| (r - mean).powi(2))
            .sum::<f64>()
            / (n - 1) as f64;
        Some(var.sqrt())
    }
}

/// Sum of `price × volume` and total volume across the ladder (exact per-price volume).
fn ladder_price_volume(fp: &Footprint) -> (f64, f64) {
    if fp.ladder.is_empty() {
        // Trades-only fallback: approximate with the typical price.
        let typical = (fp.high + fp.low + fp.close) / 3.0;
        return (typical * fp.volume, fp.volume);
    }
    let mut pv = 0.0;
    let mut vol = 0.0;
    for lv in &fp.ladder {
        let v = lv.buy_vol + lv.sell_vol;
        pv += lv.price * v;
        vol += v;
    }
    (pv, vol)
}

/// Longest run of adjacent (1-tick-apart) keys in an ascending list.
fn longest_adjacent_run(keys: &[i64]) -> usize {
    let mut best = 0usize;
    let mut run = 0usize;
    let mut prev: Option<i64> = None;
    for &k in keys {
        run = match prev {
            Some(p) if k == p + 1 => run + 1,
            _ => 1,
        };
        best = best.max(run);
        prev = Some(k);
    }
    best
}
