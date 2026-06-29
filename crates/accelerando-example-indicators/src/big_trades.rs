use std::collections::BTreeMap;

use accelerando_core::{
    Configurable, EventInterest, Footprint, Indicator, OrderFlowEvent, ParamSpec, Params, Plot,
    Side,
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

#[derive(Clone, Debug)]
struct PendingCumulative {
    side: Side,
    by_tick: BTreeMap<i64, f64>,
    total: f64,
}

/// ATAS-style big trade indicator.
///
/// In `single` mode it keeps trades at or above `min_trade_size`. In `cumulative` mode it groups
/// consecutive same-side trades and keeps the whole group when its total reaches the threshold.
/// Both modes aggregate accepted volume by price inside the current footprint.
pub struct BigTrades {
    mode: BigTradeMode,
    threshold_mode: ThresholdMode,
    min_trade_size: f64,
    max_trade_size: f64,
    lookback_bars: usize,
    sensitivity: f64,
    edge_band_ticks: f64,
    tick: f64,
    plot_markers: bool,
    max_markers_per_side: usize,
    pending: Option<PendingCumulative>,
    cached_threshold: f64,
    current_candidate_max: f64,
    prior_candidate_max: Vec<f64>,
    by_tick: BTreeMap<i64, BigLevel>,
    total_buy: f64,
    total_sell: f64,
    count: u64,
}

impl Configurable for BigTrades {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .choice("mode", "single", &["single", "cumulative"])
            .choice("threshold_mode", "fixed", &["fixed", "adaptive"])
            .float("min_trade_size", 25.0, 1.0, 5000.0)
            .float("max_trade_size", 500.0, 1.0, 20000.0)
            .int("lookback_bars", 20, 3, 200, 1)
            .float("sensitivity", 1.0, 0.0, 4.0)
            .int("edge_band_ticks", 16, 1, 120, 1)
            .fixed_float("tick", 0.25)
            .int("plot_markers", 1, 0, 1, 1)
            .int("max_markers_per_side", 8, 1, 50, 1)
    }

    fn from_params(p: &Params) -> Self {
        let mode = match p.str("mode", "single").as_str() {
            "cumulative" => BigTradeMode::Cumulative,
            _ => BigTradeMode::Single,
        };
        let min_trade_size = p.float("min_trade_size", 25.0).max(1.0);
        Self {
            mode,
            threshold_mode: match p.str("threshold_mode", "fixed").as_str() {
                "adaptive" => ThresholdMode::Adaptive,
                _ => ThresholdMode::Fixed,
            },
            min_trade_size,
            max_trade_size: p.float("max_trade_size", 500.0).max(min_trade_size),
            lookback_bars: p.usize("lookback_bars", 20).max(1),
            sensitivity: p.float("sensitivity", 1.0).max(0.0),
            edge_band_ticks: p.int("edge_band_ticks", 16) as f64,
            tick: p.float("tick", 0.25).max(f64::EPSILON),
            plot_markers: p.int("plot_markers", 1) != 0,
            max_markers_per_side: p.usize("max_markers_per_side", 8).max(1),
            pending: None,
            cached_threshold: min_trade_size,
            current_candidate_max: 0.0,
            prior_candidate_max: Vec::new(),
            by_tick: BTreeMap::new(),
            total_buy: 0.0,
            total_sell: 0.0,
            count: 0,
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
                if tick_size > 0.0 {
                    self.tick = tick_size;
                }
            }
            OrderFlowEvent::Trade {
                price,
                size,
                aggressor,
                ..
            } => self.on_trade(price, size, aggressor),
            _ => {}
        }
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        self.flush_pending_cumulative();

        let threshold = self.cached_threshold;
        let buy_edge = self.best_buy_edge(fp);
        let sell_edge = self.best_sell_edge(fp);

        fp.values.insert(THRESHOLD.to_string(), threshold);
        fp.values.insert(TOTAL_BUY.to_string(), self.total_buy);
        fp.values.insert(TOTAL_SELL.to_string(), self.total_sell);
        fp.values
            .insert(DELTA.to_string(), self.total_buy - self.total_sell);
        fp.values.insert(COUNT.to_string(), self.count as f64);
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
            self.plot_big_trades(fp);
        }

        self.push_prior_candidate();
        self.clear_bar();
    }
}

impl BigTrades {
    fn on_trade(&mut self, price: f64, size: f64, side: Side) {
        match self.mode {
            BigTradeMode::Single => {
                self.current_candidate_max = self.current_candidate_max.max(size);
                if size >= self.cached_threshold {
                    self.record_big_trade(self.price_key(price), size, side);
                    self.count += 1;
                }
            }
            BigTradeMode::Cumulative => self.on_cumulative_trade(price, size, side),
        }
    }

    fn on_cumulative_trade(&mut self, price: f64, size: f64, side: Side) {
        let key = self.price_key(price);
        let side_changed = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.side != side);
        if side_changed {
            self.flush_pending_cumulative();
        }

        let pending = self.pending.get_or_insert_with(|| PendingCumulative {
            side,
            by_tick: BTreeMap::new(),
            total: 0.0,
        });
        *pending.by_tick.entry(key).or_default() += size;
        pending.total += size;
    }

    fn flush_pending_cumulative(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.current_candidate_max = self.current_candidate_max.max(pending.total);
        if pending.total < self.cached_threshold {
            return;
        }
        for (key, size) in pending.by_tick {
            self.record_big_trade(key, size, pending.side);
        }
        self.count += 1;
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
        let max_keep = self.lookback_bars.saturating_mul(4).max(self.lookback_bars + 1);
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

    fn plot_big_trades(&self, fp: &mut Footprint) {
        let mut buys: Vec<(i64, f64)> = self
            .by_tick
            .iter()
            .filter_map(|(key, level)| (level.buy > 0.0).then_some((*key, level.buy)))
            .collect();
        buys.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (key, size) in buys.into_iter().take(self.max_markers_per_side) {
            fp.plots.push(Plot::Marker {
                price: self.price_from_key(key),
                shape: "circle".to_string(),
                color: "#6d6dff".to_string(),
                text: format!("{:.0}", size),
                text_dx: Some(0.0),
                text_dy: Some(0.0),
                group: Some("big_trades".to_string()),
            });
        }

        let mut sells: Vec<(i64, f64)> = self
            .by_tick
            .iter()
            .filter_map(|(key, level)| (level.sell > 0.0).then_some((*key, level.sell)))
            .collect();
        sells.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (key, size) in sells.into_iter().take(self.max_markers_per_side) {
            fp.plots.push(Plot::Marker {
                price: self.price_from_key(key),
                shape: "circle".to_string(),
                color: "#ff6d6d".to_string(),
                text: format!("{:.0}", size),
                text_dx: Some(0.0),
                text_dy: Some(0.0),
                group: Some("big_trades".to_string()),
            });
        }
    }

    fn clear_bar(&mut self) {
        self.pending = None;
        self.current_candidate_max = 0.0;
        self.by_tick.clear();
        self.total_buy = 0.0;
        self.total_sell = 0.0;
        self.count = 0;
    }
}
