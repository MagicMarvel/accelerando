use std::collections::BTreeMap;

use accelerando_core::{
    Configurable, Footprint, Indicator, OrderFlowEvent, ParamSpec, Params, Plot, Side,
};

pub const BUY_EDGE_PRICE: &str = "big_trade_buy_edge_price";
pub const BUY_EDGE_SIZE: &str = "big_trade_buy_edge_size";
pub const SELL_EDGE_PRICE: &str = "big_trade_sell_edge_price";
pub const SELL_EDGE_SIZE: &str = "big_trade_sell_edge_size";
pub const DELTA: &str = "big_trade_delta";
pub const TOTAL_BUY: &str = "big_trade_total_buy";
pub const TOTAL_SELL: &str = "big_trade_total_sell";
pub const COUNT: &str = "big_trade_count";

#[derive(Clone, Copy, Debug, Default)]
struct BigLevel {
    buy: f64,
    sell: f64,
}

/// ATAS-style big trade indicator.
///
/// It watches raw trade events, keeps only trades at or above `min_trade_size`, aggregates them by
/// price inside the current footprint, exposes summary values for strategies, and optionally plots
/// the big-trade nodes directly on the footprint chart.
pub struct BigTrades {
    min_trade_size: f64,
    edge_band_ticks: f64,
    tick: f64,
    plot_markers: bool,
    max_markers_per_side: usize,
    by_tick: BTreeMap<i64, BigLevel>,
    total_buy: f64,
    total_sell: f64,
    count: u64,
}

impl Configurable for BigTrades {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .float("min_trade_size", 25.0, 1.0, 5000.0)
            .int("edge_band_ticks", 16, 1, 120, 1)
            .fixed_float("tick", 0.25)
            .int("plot_markers", 1, 0, 1, 1)
            .int("max_markers_per_side", 8, 1, 50, 1)
    }

    fn from_params(p: &Params) -> Self {
        Self {
            min_trade_size: p.float("min_trade_size", 25.0).max(1.0),
            edge_band_ticks: p.int("edge_band_ticks", 16) as f64,
            tick: p.float("tick", 0.25).max(f64::EPSILON),
            plot_markers: p.int("plot_markers", 1) != 0,
            max_markers_per_side: p.usize("max_markers_per_side", 8).max(1),
            by_tick: BTreeMap::new(),
            total_buy: 0.0,
            total_sell: 0.0,
            count: 0,
        }
    }
}

impl Indicator for BigTrades {
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
            } if size >= self.min_trade_size => {
                let level = self.by_tick.entry(self.price_key(price)).or_default();
                match aggressor {
                    Side::Buy => {
                        level.buy += size;
                        self.total_buy += size;
                    }
                    Side::Sell => {
                        level.sell += size;
                        self.total_sell += size;
                    }
                }
                self.count += 1;
            }
            _ => {}
        }
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        let buy_edge = self.best_buy_edge(fp);
        let sell_edge = self.best_sell_edge(fp);

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

        self.clear_bar();
    }
}

impl BigTrades {
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
                color: "#2563eb".to_string(),
                text: format!("BT buy {:.0}", size),
                text_dx: Some(8.0),
                text_dy: Some(-10.0),
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
                color: "#d97706".to_string(),
                text: format!("BT sell {:.0}", size),
                text_dx: Some(8.0),
                text_dy: Some(10.0),
            });
        }
    }

    fn clear_bar(&mut self) {
        self.by_tick.clear();
        self.total_buy = 0.0;
        self.total_sell = 0.0;
        self.count = 0;
    }
}
