use std::collections::BTreeMap;

use accelerando_core::{
    Configurable, EventInterest, Footprint, Indicator, OrderFlowEvent, Plot, Side,
};

pub const BUY_EDGE_PRICE: &str = "big_trade_buy_edge_price";
pub const BUY_EDGE_SIZE: &str = "big_trade_buy_edge_size";
pub const SELL_EDGE_PRICE: &str = "big_trade_sell_edge_price";
pub const SELL_EDGE_SIZE: &str = "big_trade_sell_edge_size";
pub const DELTA: &str = "big_trade_delta";
pub const TOTAL_BUY: &str = "big_trade_total_buy";
pub const TOTAL_SELL: &str = "big_trade_total_sell";
pub const COUNT: &str = "big_trade_count";
pub const THRESHOLD: &str = "big_trade_threshold";
pub const DEFAULT_MIN_TRADE_SIZE: f64 = 25.0;
pub const DEFAULT_CUMULATIVE_WINDOW_MS: i64 = 1;

#[derive(Clone, Copy, Debug, Default)]
struct BigLevel {
    buy: f64,
    sell: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BigTradeMode {
    Single,
    Cumulative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThresholdMode {
    Fixed,
    Adaptive,
}

#[derive(Clone, Copy)]
struct BigTrade {
    side: Side,
    price: f64,
    size: f64,
}

/// Consecutive prints reconstructed as one market execution.
pub struct CumulativeTrade {
    pub side: Side,
    start_ns: i64,
    pub last_ns: i64,
    first_price: f64,
    pub last_price: f64,
    notional: f64,
    pub total: f64,
}

impl CumulativeTrade {
    /// Midpoint of the swept price range. Prices move monotonically within a sweep.
    pub fn midpoint_price(&self) -> f64 {
        self.first_price.midpoint(self.last_price)
    }
}

/// Streaming sweep reconstruction shared by the indicator and the native viewer.
#[derive(Default)]
pub struct CumulativeTrades(Option<CumulativeTrade>);

impl CumulativeTrades {
    /// Returns a completed execution when this print ends the previous sweep.
    pub fn push(
        &mut self,
        ts_ns: i64,
        price: f64,
        size: f64,
        side: Side,
        window_ms: i64,
    ) -> Option<CumulativeTrade> {
        if !valid_print(price, size, side) {
            return self.take();
        }
        let window_ns = window_ms as u64 * 1_000_000;
        let completed = if self.0.as_ref().is_some_and(|pending| {
            pending.side != side
                || ts_ns.abs_diff(pending.start_ns) >= window_ns
                || (side == Side::Buy && price < pending.last_price)
                || (side == Side::Sell && price > pending.last_price)
        }) {
            self.take()
        } else {
            None
        };
        let pending = self.0.get_or_insert(CumulativeTrade {
            side,
            start_ns: ts_ns,
            last_ns: ts_ns,
            first_price: price,
            last_price: price,
            notional: 0.0,
            total: 0.0,
        });
        pending.last_ns = ts_ns;
        pending.last_price = price;
        pending.notional += price * size;
        pending.total += size;
        completed
    }

    pub fn take(&mut self) -> Option<CumulativeTrade> {
        self.0.take()
    }
}

fn valid_print(price: f64, size: f64, side: Side) -> bool {
    side != Side::Unknown && price.is_finite() && size.is_finite() && size > 0.0
}

/// ATAS-style large market executions from raw trade prints.
/// Cumulative mode groups consecutive prints within a footprint by aggressor, elapsed time from
/// the first print, and price progression (buys sweep up, sells sweep down).
/// The feed supplies one instrument stream without a market identifier.
#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct BigTrades {
    #[param(default = "cumulative", choice = ["single", "cumulative"])]
    mode: BigTradeMode,
    #[param(default = DEFAULT_CUMULATIVE_WINDOW_MS, min = 1, max = 1000)]
    cumulative_window_ms: i64,
    #[param(default = "last", choice = ["last", "start", "weighted_average"])]
    execution_price: String,
    #[param(default = "any", choice = ["any", "at_high", "at_low", "body", "upper_wick", "lower_wick", "at_high_or_low"])]
    price_location: String,
    #[param(default = "fixed", choice = ["fixed", "adaptive"])]
    threshold_mode: ThresholdMode,
    #[param(default = DEFAULT_MIN_TRADE_SIZE, min = 1.0, max = 5000.0)]
    min_trade_size: f64,
    #[param(default = 0.0, min = 0.0, max = 1000000.0)]
    max_volume: f64,
    // Upper bound for the adaptive threshold, independent of the volume filter.
    #[param(default = 500.0, min = 1.0, max = 20000.0)]
    max_trade_size: f64,
    #[param(default = 20, min = 3, max = 200)]
    lookback_bars: usize,
    #[param(default = 1.0, min = 0.0, max = 4.0)]
    sensitivity: f64,
    #[param(int, default = 16, min = 1, max = 120)]
    edge_band_ticks: f64,
    #[param(fixed, default = 0.25)]
    tick: f64,
    #[param(default = true)]
    plot_markers: bool,
    #[param(default = 8, min = 1, max = 50)]
    max_markers_per_side: usize,
    cumulative: CumulativeTrades,
    cached_threshold: f64,
    current_candidate_max: f64,
    prior_candidate_max: Vec<f64>,
    qualifying: Vec<BigTrade>,
    by_tick: BTreeMap<i64, BigLevel>,
    total_buy: f64,
    total_sell: f64,
}

impl BigTrades {
    fn init(&mut self) {
        self.min_trade_size = self.min_trade_size.max(1.0);
        self.cumulative_window_ms = self.cumulative_window_ms.clamp(1, 1000);
        self.max_trade_size = self.max_trade_size.max(self.min_trade_size);
        self.lookback_bars = self.lookback_bars.max(1);
        self.sensitivity = self.sensitivity.max(0.0);
        self.tick = self.tick.max(f64::EPSILON);
        self.max_markers_per_side = self.max_markers_per_side.max(1);
        self.cached_threshold = self.min_trade_size;
    }
}

impl std::str::FromStr for BigTradeMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "single" => Ok(Self::Single),
            "cumulative" => Ok(Self::Cumulative),
            other => Err(format!("unknown BigTradeMode `{other}`")),
        }
    }
}

impl std::str::FromStr for ThresholdMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "fixed" => Ok(Self::Fixed),
            "adaptive" => Ok(Self::Adaptive),
            other => Err(format!("unknown ThresholdMode `{other}`")),
        }
    }
}

impl Indicator for BigTrades {
    fn event_interest(&self) -> EventInterest {
        EventInterest::CONTRACT.union(EventInterest::TRADE)
    }

    fn name(&self) -> &str {
        "big_trades"
    }

    fn on_event(&mut self, ev: &OrderFlowEvent) {
        match *ev {
            OrderFlowEvent::Contract { tick_size, .. } => {
                self.clear_bar();
                self.qualifying.clear();
                self.prior_candidate_max.clear();
                self.cached_threshold = self.min_trade_size;
                if tick_size.is_finite() && tick_size > 0.0 {
                    self.tick = tick_size;
                }
            }
            OrderFlowEvent::Trade {
                price,
                size,
                aggressor,
                ts_ns,
            } => self.on_trade(ts_ns, price, size, aggressor),
            _ => {}
        }
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        if let Some(trade) = self.cumulative.take() {
            self.consider_cumulative(trade);
        }

        let mut qualifying = std::mem::take(&mut self.qualifying);
        qualifying.retain(|trade| self.matches_price_location(fp, trade.price));
        for trade in &qualifying {
            self.record_big_trade(self.price_key(trade.price), trade.size, trade.side);
        }
        let threshold = self.cached_threshold;
        let buy_edge = self.best_buy_edge(fp);
        let sell_edge = self.best_sell_edge(fp);

        fp.values.insert(THRESHOLD.to_string(), threshold);
        fp.values.insert(TOTAL_BUY.to_string(), self.total_buy);
        fp.values.insert(TOTAL_SELL.to_string(), self.total_sell);
        fp.values
            .insert(DELTA.to_string(), self.total_buy - self.total_sell);
        fp.values.insert(COUNT.to_string(), qualifying.len() as f64);
        fp.values.insert(
            BUY_EDGE_PRICE.to_string(),
            buy_edge.map(|v| v.0).unwrap_or(0.0),
        );
        fp.values.insert(
            BUY_EDGE_SIZE.to_string(),
            buy_edge.map(|v| v.1).unwrap_or(0.0),
        );
        fp.values.insert(
            SELL_EDGE_PRICE.to_string(),
            sell_edge.map(|v| v.0).unwrap_or(0.0),
        );
        fp.values.insert(
            SELL_EDGE_SIZE.to_string(),
            sell_edge.map(|v| v.1).unwrap_or(0.0),
        );

        if self.plot_markers {
            self.plot_big_trades(fp, &mut qualifying);
        }

        qualifying.clear();
        self.qualifying = qualifying;
        self.push_prior_candidate();
        self.clear_bar();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::Params;

    fn print(indicator: &mut BigTrades, ts_ns: i64, price: f64, size: f64, aggressor: Side) {
        indicator.on_event(&OrderFlowEvent::Trade {
            ts_ns,
            price,
            size,
            aggressor,
        });
    }

    #[test]
    fn cumulative_groups_by_first_print_time_side_and_sweep_direction() {
        let mut indicator = BigTrades::from_params(&Params::default());
        for (time, price, size, side) in [
            (900_000, 100.0, 12.0, Side::Buy),
            (1_400_000, 100.25, 13.0, Side::Buy), // Crosses a millisecond boundary.
            (1_900_000, 100.50, 14.0, Side::Buy), // Exactly 1 ms from the start.
            (1_950_000, 100.25, 14.0, Side::Buy), // A buy price reversal splits the group.
            (2_000_000, 100.25, 14.0, Side::Sell),
            (2_100_000, 100.0, 12.0, Side::Sell),
        ] {
            print(&mut indicator, time, price, size, side);
        }
        let mut bar = Footprint::seed(900_000, 100.0);
        indicator.on_footprint(&mut bar, &[]);
        assert_eq!(bar.value(COUNT), Some(2.0));
        assert_eq!(bar.value(TOTAL_BUY), Some(25.0));
        assert_eq!(bar.value(TOTAL_SELL), Some(26.0));
        assert!(matches!(&bar.plots[0], Plot::Marker { price, text, .. }
            if *price == 100.25 && text == "25"));
        assert!(matches!(&bar.plots[1], Plot::Marker { price, text, .. }
            if *price == 100.0 && text == "26"));
    }

    #[test]
    fn uses_selected_execution_price_for_markers_and_price_location() {
        let params = Params::default()
            .with("execution_price", "weighted_average")
            .with("price_location", "body");
        let mut indicator = BigTrades::from_params(&params);
        print(&mut indicator, 0, 100.0, 10.0, Side::Buy);
        print(&mut indicator, 1, 101.0, 30.0, Side::Buy);
        print(&mut indicator, 2, 101.0, 30.0, Side::Sell);
        let mut bar = Footprint::seed(0, 100.0);
        bar.high = 101.0;
        bar.close = 100.75;
        indicator.on_footprint(&mut bar, &[]);
        assert_eq!(bar.value(COUNT), Some(1.0));
        assert_eq!(bar.value(DELTA), Some(40.0));
        assert!(matches!(&bar.plots[0], Plot::Marker { price, .. } if *price == 100.75));
    }

    #[test]
    fn single_mode_filters_each_print_with_inclusive_volume_bounds() {
        let params = Params::default()
            .with("mode", "single")
            .with("max_volume", 30.0);
        let mut indicator = BigTrades::from_params(&params);
        for (size, side) in [
            (15.0, Side::Buy),
            (15.0, Side::Buy),
            (25.0, Side::Buy),
            (30.0, Side::Buy),
            (31.0, Side::Buy),
            (29.0, Side::Unknown),
        ] {
            print(&mut indicator, 0, 100.0, size, side);
        }
        let mut bar = Footprint::seed(0, 100.0);
        indicator.on_footprint(&mut bar, &[]);
        assert_eq!(bar.value(COUNT), Some(2.0));
        assert_eq!(bar.value(TOTAL_BUY), Some(55.0));
        assert_eq!(bar.plots.len(), 2);
    }
}

impl BigTrades {
    fn on_trade(&mut self, ts_ns: i64, price: f64, size: f64, side: Side) {
        match self.mode {
            BigTradeMode::Single if valid_print(price, size, side) => {
                self.consider(BigTrade { side, price, size });
            }
            BigTradeMode::Cumulative => {
                if let Some(trade) =
                    self.cumulative
                        .push(ts_ns, price, size, side, self.cumulative_window_ms)
                {
                    self.consider_cumulative(trade);
                }
            }
            _ => {}
        }
    }

    fn consider_cumulative(&mut self, pending: CumulativeTrade) {
        let price = match self.execution_price.as_str() {
            "start" => pending.first_price,
            "weighted_average" => pending.notional / pending.total,
            _ => pending.last_price,
        };
        self.consider(BigTrade {
            side: pending.side,
            price,
            size: pending.total,
        });
    }

    fn consider(&mut self, trade: BigTrade) {
        self.current_candidate_max = self.current_candidate_max.max(trade.size);
        if trade.size >= self.cached_threshold
            && (self.max_volume == 0.0 || trade.size <= self.max_volume)
        {
            self.qualifying.push(trade);
        }
    }

    fn matches_price_location(&self, fp: &Footprint, price: f64) -> bool {
        let body_high = fp.open.max(fp.close);
        let body_low = fp.open.min(fp.close);
        // Allow only floating-point rounding error when comparing a VWAP to a bar boundary.
        let at =
            |level: f64| (price - level).abs() <= f64::EPSILON * price.abs().max(level.abs()) * 4.0;
        match self.price_location.as_str() {
            "at_high" => at(fp.high),
            "at_low" => at(fp.low),
            "body" => (price >= body_low || at(body_low)) && (price <= body_high || at(body_high)),
            "upper_wick" => price >= body_high || at(body_high),
            "lower_wick" => price <= body_low || at(body_low),
            "at_high_or_low" => at(fp.high) || at(fp.low),
            _ => true,
        }
    }

    fn threshold(&self) -> f64 {
        if self.threshold_mode == ThresholdMode::Fixed || self.prior_candidate_max.is_empty() {
            return self.min_trade_size;
        }
        let mut n = 0usize;
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        for v in self
            .prior_candidate_max
            .iter()
            .rev()
            .take(self.lookback_bars)
            .copied()
            .filter(|v| v.is_finite() && *v > 0.0)
        {
            n += 1;
            sum += v;
            sum_sq += v * v;
        }
        if n == 0 {
            return self.min_trade_size;
        }
        let mean = sum / n as f64;
        let var = (sum_sq / n as f64 - mean * mean).max(0.0);
        (mean + self.sensitivity * var.sqrt()).clamp(self.min_trade_size, self.max_trade_size)
    }

    fn push_prior_candidate(&mut self) {
        if self.current_candidate_max > 0.0 {
            self.prior_candidate_max.push(self.current_candidate_max);
        }
        let max_keep = self
            .lookback_bars
            .saturating_mul(4)
            .max(self.lookback_bars + 1);
        if self.prior_candidate_max.len() > max_keep {
            let remove = self.prior_candidate_max.len() - max_keep;
            self.prior_candidate_max.drain(0..remove);
        }
        self.cached_threshold = self.threshold();
    }

    fn record_big_trade(&mut self, key: i64, size: f64, side: Side) {
        let level = self.by_tick.entry(key).or_default();
        match side {
            Side::Buy => {
                level.buy += size;
                self.total_buy += size;
            }
            Side::Unknown => return,
            Side::Sell => {
                level.sell += size;
                self.total_sell += size;
            }
        }
    }

    fn price_key(&self, price: f64) -> i64 {
        (price / self.tick).round() as i64
    }

    fn price_from_key(&self, key: i64) -> f64 {
        key as f64 * self.tick
    }

    fn best_buy_edge(&self, fp: &Footprint) -> Option<(f64, f64)> {
        let min_price = fp.high - self.edge_band_ticks * self.tick;
        self.by_tick
            .iter()
            .map(|(key, level)| (self.price_from_key(*key), level.buy))
            .filter(|(price, size)| *price >= min_price && *size > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    fn best_sell_edge(&self, fp: &Footprint) -> Option<(f64, f64)> {
        let max_price = fp.low + self.edge_band_ticks * self.tick;
        self.by_tick
            .iter()
            .map(|(key, level)| (self.price_from_key(*key), level.sell))
            .filter(|(price, size)| *price <= max_price && *size > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    fn plot_big_trades(&self, fp: &mut Footprint, trades: &mut [BigTrade]) {
        trades.sort_by(|a, b| b.size.total_cmp(&a.size));
        for (side, color) in [(Side::Buy, "#6d6dff"), (Side::Sell, "#ff6d6d")] {
            for trade in trades
                .iter()
                .filter(|t| t.side == side)
                .take(self.max_markers_per_side)
            {
                fp.plots.push(Plot::Marker {
                    price: trade.price,
                    shape: "circle".to_string(),
                    color: color.to_string(),
                    text: trade.size.to_string(),
                    text_dx: Some(0.0),
                    text_dy: Some(0.0),
                    group: Some("big_trades".to_string()),
                });
            }
        }
    }

    fn clear_bar(&mut self) {
        self.cumulative.take();
        self.current_candidate_max = 0.0;
        self.by_tick.clear();
        self.total_buy = 0.0;
        self.total_sell = 0.0;
    }
}
