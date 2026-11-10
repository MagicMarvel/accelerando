//! Helpers for unit-testing strategies and indicators on synthetic bars.
//!
//! ```ignore
//! use accelerando_core::testkit::{bar, eastern_ts, drive};
//!
//! let t0 = eastern_ts(2026, 1, 5, 9, 45);
//! let bars = vec![
//!     bar(t0).ohlc(100.0, 101.0, 99.5, 100.5).volume(500.0).build(),
//!     bar(t0).minute(1).ohlc(100.5, 102.0, 100.0, 101.75).value("my_feature", 2.5).build(),
//! ];
//! let mut strategy = MyStrategy::build(&Params::default().with("stop_ticks", 8));
//! let run = drive(&mut strategy, &bars);
//! assert_eq!(run.trades.len(), 1);
//! ```
//!
//! [`drive`] steps the strategy exactly like `run_prepared_backtest` (broker fills first, then
//! `on_trade_closed`, then `on_footprint`) but keeps everything in memory for assertions.

use crate::broker::{Broker, BrokerConfig, StrategyOutput};
use crate::engine::{run_prepared_backtest, PreparedBacktestData};
use crate::footprint::{Footprint, Plot};
use crate::market_time::{days_from_civil, in_us_dst, NS_PER_SEC};
use crate::result::{BacktestResult, Trade};
use crate::traits::Strategy;

/// Default tick size used by the helpers (ES/NQ style).
pub const TICK: f64 = 0.25;
/// Default point value used by the helpers (ES).
pub const MULTIPLIER: f64 = 50.0;

/// Nanosecond timestamp for an US-Eastern wall-clock time (DST aware).
pub fn eastern_ts(year: i32, month: i32, day: i32, hour: i64, minute: i64) -> i64 {
    let offset_hours = if in_us_dst(year, month, day) { 4 } else { 5 };
    (days_from_civil(year, month, day) * 86_400 + (hour + offset_hours) * 3600 + minute * 60)
        * NS_PER_SEC
}

/// Builder for one synthetic footprint. Starts as a flat 1-minute bar at price 100.
#[derive(Clone, Debug)]
pub struct BarBuilder {
    fp: Footprint,
    secs: i64,
}

/// Start a bar opening at `ts_ns`.
pub fn bar(ts_ns: i64) -> BarBuilder {
    let mut fp = Footprint::seed(ts_ns, 100.0);
    fp.volume = 100.0;
    fp.trades = 50;
    BarBuilder { fp, secs: 60 }
}

impl BarBuilder {
    /// Shift the bar start by `n` bar lengths (handy in loops: `bar(t0).minute(i)`).
    pub fn minute(mut self, n: i64) -> Self {
        self.fp.ts_first_ns += n * 60 * NS_PER_SEC;
        self
    }

    /// Bar length in seconds (default 60).
    pub fn secs(mut self, secs: i64) -> Self {
        self.secs = secs.max(1);
        self
    }

    pub fn ohlc(mut self, open: f64, high: f64, low: f64, close: f64) -> Self {
        self.fp.open = open;
        self.fp.high = high.max(open).max(close);
        self.fp.low = low.min(open).min(close);
        self.fp.close = close;
        self
    }

    /// Flat bar at `price`.
    pub fn price(self, price: f64) -> Self {
        self.ohlc(price, price, price, price)
    }

    pub fn volume(mut self, volume: f64) -> Self {
        self.fp.volume = volume;
        self
    }

    pub fn delta(mut self, delta: f64) -> Self {
        self.fp.delta = delta;
        self
    }

    /// Add traded volume at one price level (also updates volume/delta/POC).
    pub fn level(mut self, price: f64, buy: f64, sell: f64) -> Self {
        let ts = self.fp.ts_first_ns;
        let (o, h, l, c) = (self.fp.open, self.fp.high, self.fp.low, self.fp.close);
        let had_ladder = !self.fp.ladder.is_empty();
        let base_volume = if had_ladder { self.fp.volume } else { 0.0 };
        let base_delta = if had_ladder { self.fp.delta } else { 0.0 };
        self.fp.volume = base_volume;
        self.fp.delta = base_delta;
        if buy > 0.0 {
            self.fp.add_trade(ts, price, buy, true, TICK);
        }
        if sell > 0.0 {
            self.fp.add_trade(ts, price, sell, false, TICK);
        }
        self.fp.open = o;
        self.fp.high = h.max(price);
        self.fp.low = l.min(price);
        self.fp.close = c;
        self.fp.finalize_ladder();
        self
    }

    /// Inject an indicator value (what an `Indicator` would publish).
    pub fn value(mut self, key: &str, value: f64) -> Self {
        self.fp.values.insert(key.to_string(), value);
        self
    }

    pub fn tag(mut self, key: &str, value: &str) -> Self {
        self.fp.tags.insert(key.to_string(), value.to_string());
        self
    }

    pub fn build(mut self) -> Footprint {
        self.fp.ts_last_ns = self.fp.ts_first_ns + self.secs * NS_PER_SEC - 1;
        self.fp
    }
}

/// Consecutive bars from a list of closes (each bar opens at the previous close).
pub fn bars_from_closes(start_ts: i64, secs: i64, closes: &[f64]) -> Vec<Footprint> {
    let mut prev = closes.first().copied().unwrap_or(100.0);
    closes
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let fp = bar(start_ts + i as i64 * secs * NS_PER_SEC)
                .secs(secs)
                .ohlc(prev, prev.max(c), prev.min(c), c)
                .build();
            prev = c;
            fp
        })
        .collect()
}

/// Everything [`drive`] observed.
#[derive(Debug, Default)]
pub struct Driven {
    pub trades: Vec<Trade>,
    /// Plots emitted per bar (index = bar index).
    pub plots: Vec<Vec<Plot>>,
    /// Net signed position after each bar.
    pub positions: Vec<i32>,
}

impl Driven {
    /// Net position after the last bar.
    pub fn final_position(&self) -> i32 {
        self.positions.last().copied().unwrap_or(0)
    }
}

/// Step `strategy` over `bars` with a default broker (no costs, ES contract).
pub fn drive<S: Strategy + ?Sized>(strategy: &mut S, bars: &[Footprint]) -> Driven {
    drive_with(strategy, bars, BrokerConfig::default())
}

/// [`drive`] with explicit commission/slippage.
pub fn drive_with<S: Strategy + ?Sized>(
    strategy: &mut S,
    bars: &[Footprint],
    cfg: BrokerConfig,
) -> Driven {
    let mut broker = Broker::new(cfg);
    broker.set_contract(TICK, MULTIPLIER);
    let mut out = Driven::default();
    for fp in bars {
        let before = broker.trades.len();
        broker.on_new_footprint(fp);
        for i in before..broker.trades.len() {
            let trade = broker.trades[i].clone();
            strategy.on_trade_closed(&trade);
        }
        let portfolio = broker.portfolio_snapshot();
        let mut output = StrategyOutput::new(true);
        strategy.on_footprint(fp, &portfolio, &mut output);
        let (plots, _) = broker.apply_strategy_output(output);
        out.plots.push(plots);
        out.positions.push(broker.portfolio_snapshot().position);
    }
    out.trades = broker.trades.clone();
    out
}

/// Full engine run (metrics, equity, end-of-data flattening) over synthetic bars.
pub fn backtest(strategy: Box<dyn Strategy>, bars: Vec<Footprint>, cfg: BrokerConfig) -> BacktestResult {
    let data = PreparedBacktestData::from_footprints(bars, TICK, MULTIPLIER);
    run_prepared_backtest(&data, strategy, cfg, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{EntryIntent, PortfolioSnapshot, TradeSide};

    struct BuyFirstBar {
        done: bool,
    }

    impl Strategy for BuyFirstBar {
        fn on_footprint(&mut self, _fp: &Footprint, _p: &PortfolioSnapshot, out: &mut StrategyOutput) {
            if !self.done {
                self.done = true;
                out.orders
                    .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(4.0, 4.0));
            }
        }
    }

    #[test]
    fn drive_fills_next_bar_and_hits_target() {
        let t0 = eastern_ts(2026, 1, 5, 10, 0);
        let bars = bars_from_closes(t0, 60, &[100.0, 100.0, 101.5, 101.5]);
        let mut s = BuyFirstBar { done: false };
        let run = drive(&mut s, &bars);
        assert_eq!(run.trades.len(), 1);
        assert_eq!(run.trades[0].entry_px, 100.0);
        assert_eq!(run.trades[0].exit_px, 101.0);
        assert_eq!(run.final_position(), 0);
    }

    #[test]
    fn eastern_ts_handles_dst() {
        // 2026-07-06 09:30 EDT == 13:30 UTC
        let ts = eastern_ts(2026, 7, 6, 9, 30);
        assert_eq!(crate::market_time::eastern_day_minute(ts).1, 9 * 60 + 30);
        let ts = eastern_ts(2026, 1, 5, 9, 30);
        assert_eq!(crate::market_time::eastern_day_minute(ts).1, 9 * 60 + 30);
    }

    #[test]
    fn level_builds_a_ladder() {
        let fp = bar(0).price(100.0).level(100.0, 3.0, 1.0).level(100.25, 0.0, 2.0).build();
        assert_eq!(fp.volume, 6.0);
        assert_eq!(fp.delta, 0.0);
        assert_eq!(fp.ladder.len(), 2);
        assert_eq!(fp.high, 100.25);
    }
}
