//! A minimal next-bar-fill broker simulator and the [`OrderCtx`] strategies use to express intent.
//!
//! Execution model: a strategy sets a *desired* position on footprint `i`; the broker realizes
//! that transition at the **open** of footprint `i+1` (market order, tick slippage + commission),
//! then checks the open position's stop/target against `i+1`'s high/low intrabar.

use crate::footprint::Footprint;
use crate::result::{EquityPoint, Trade, TradeReason};

/// Static broker configuration (fees, slippage, starting equity).
#[derive(Clone, Copy, Debug)]
pub struct BrokerConfig {
    pub commission_per_contract: f64,
    pub slippage_ticks: f64,
    pub starting_equity: f64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            commission_per_contract: 0.0,
            slippage_ticks: 0.0,
            starting_equity: 100_000.0,
        }
    }
}

/// The position a strategy wants to hold after the next fill.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Desired {
    Flat,
    Long {
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
    },
    Short {
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
    },
}

#[derive(Clone, Copy, Debug)]
struct Position {
    dir: i32,
    qty: f64,
    entry_px: f64,
    entry_ts: i64,
    stop: Option<f64>,
    target: Option<f64>,
}

/// The broker simulator: tracks one position, fills next-bar, records trades and equity.
pub struct Broker {
    cfg: BrokerConfig,
    tick_size: f64,
    multiplier: f64,
    realized: f64,
    peak_equity: f64,
    position: Option<Position>,
    pending: Option<Desired>,
    pub trades: Vec<Trade>,
    pub equity: Vec<EquityPoint>,
}

impl Broker {
    pub fn new(cfg: BrokerConfig) -> Self {
        Self {
            cfg,
            tick_size: 0.25,
            multiplier: 1.0,
            realized: 0.0,
            peak_equity: cfg.starting_equity,
            position: None,
            pending: None,
            trades: Vec::new(),
            equity: Vec::new(),
        }
    }

    pub fn set_contract(&mut self, tick_size: f64, multiplier: f64) {
        if tick_size > 0.0 {
            self.tick_size = tick_size;
        }
        if multiplier > 0.0 {
            self.multiplier = multiplier;
        }
    }

    pub fn tick_size(&self) -> f64 {
        self.tick_size
    }
    pub fn multiplier(&self) -> f64 {
        self.multiplier
    }

    fn commission(&self, qty: f64) -> f64 {
        self.cfg.commission_per_contract * qty
    }

    /// Realize an exit of the current position at `px`, recording the trade.
    fn close_position(&mut self, px: f64, ts: i64, reason: TradeReason) {
        if let Some(pos) = self.position.take() {
            let gross = (px - pos.entry_px) * pos.dir as f64 * pos.qty * self.multiplier;
            let fees = self.commission(pos.qty); // exit-side commission
            let pnl = gross - fees;
            self.realized += pnl;
            self.trades.push(Trade {
                entry_ts_ns: pos.entry_ts,
                exit_ts_ns: ts,
                dir: pos.dir,
                qty: pos.qty,
                entry_px: pos.entry_px,
                exit_px: px,
                pnl,
                reason,
            });
        }
    }

    /// Open a fresh position at `px` (slippage already applied), charging entry commission.
    fn open_position(
        &mut self,
        dir: i32,
        qty: f64,
        px: f64,
        ts: i64,
        stop_ticks: f64,
        target_ticks: f64,
    ) {
        self.realized -= self.commission(qty); // entry-side commission
        let stop = if stop_ticks > 0.0 {
            Some(px - dir as f64 * stop_ticks * self.tick_size)
        } else {
            None
        };
        let target = if target_ticks > 0.0 {
            Some(px + dir as f64 * target_ticks * self.tick_size)
        } else {
            None
        };
        self.position = Some(Position {
            dir,
            qty,
            entry_px: px,
            entry_ts: ts,
            stop,
            target,
        });
    }

    /// Apply tick slippage to a fill, always against the taker.
    fn slip(&self, px: f64, dir: i32) -> f64 {
        px + dir as f64 * self.cfg.slippage_ticks * self.tick_size
    }

    /// Called when a new footprint arrives, before indicators/strategy run on it.
    pub fn on_new_footprint(&mut self, fp: &Footprint) {
        // 1) Realize the pending desired transition at this bar's open.
        if let Some(desired) = self.pending.take() {
            self.transition(desired, fp.open, fp.ts_first_ns);
        }
        // 2) Intrabar stop/target check against this bar's range.
        self.check_exits(fp);
        // 3) Mark-to-market equity at the close.
        let eq = self.mark_to_market(fp.close);
        self.peak_equity = self.peak_equity.max(eq);
        self.equity.push(EquityPoint {
            ts_ns: fp.ts_last_ns,
            equity: eq,
            drawdown: eq - self.peak_equity,
        });
    }

    fn transition(&mut self, desired: Desired, open: f64, ts: i64) {
        let (want_dir, qty, st, tt) = match desired {
            Desired::Flat => (0, 0.0, 0.0, 0.0),
            Desired::Long {
                qty,
                stop_ticks,
                target_ticks,
            } => (1, qty, stop_ticks, target_ticks),
            Desired::Short {
                qty,
                stop_ticks,
                target_ticks,
            } => (-1, qty, stop_ticks, target_ticks),
        };
        let cur_dir = self.position.map(|p| p.dir).unwrap_or(0);
        if cur_dir == want_dir {
            return; // already in the desired state
        }
        if self.position.is_some() {
            self.close_position(self.slip(open, -cur_dir), ts, TradeReason::Signal);
        }
        if want_dir != 0 {
            self.open_position(want_dir, qty, self.slip(open, want_dir), ts, st, tt);
        }
    }

    fn check_exits(&mut self, fp: &Footprint) {
        let Some(pos) = self.position else { return };
        // Conservative ordering: assume stop is touched before target within the bar.
        if pos.dir > 0 {
            if let Some(stop) = pos.stop {
                if fp.low <= stop {
                    self.close_position(stop, fp.ts_last_ns, TradeReason::StopLoss);
                    return;
                }
            }
            if let Some(target) = pos.target {
                if fp.high >= target {
                    self.close_position(target, fp.ts_last_ns, TradeReason::TakeProfit);
                }
            }
        } else {
            if let Some(stop) = pos.stop {
                if fp.high >= stop {
                    self.close_position(stop, fp.ts_last_ns, TradeReason::StopLoss);
                    return;
                }
            }
            if let Some(target) = pos.target {
                if fp.low <= target {
                    self.close_position(target, fp.ts_last_ns, TradeReason::TakeProfit);
                }
            }
        }
    }

    fn mark_to_market(&self, px: f64) -> f64 {
        let unrealized = self
            .position
            .map(|p| (px - p.entry_px) * p.dir as f64 * p.qty * self.multiplier)
            .unwrap_or(0.0);
        self.cfg.starting_equity + self.realized + unrealized
    }

    /// Queue the strategy's desired position for the next bar's open.
    fn set_pending(&mut self, desired: Desired) {
        self.pending = Some(desired);
    }

    fn current_dir(&self) -> i32 {
        self.position.map(|p| p.dir).unwrap_or(0)
    }

    /// Close any open position at end of stream and append a final equity point.
    pub fn finalize(&mut self, last_close: f64, ts: i64) {
        if self.position.is_some() {
            self.close_position(self.slip(last_close, -self.current_dir()), ts, TradeReason::EndOfData);
            let eq = self.mark_to_market(last_close);
            self.peak_equity = self.peak_equity.max(eq);
            self.equity.push(EquityPoint {
                ts_ns: ts,
                equity: eq,
                drawdown: eq - self.peak_equity,
            });
        }
    }

    pub fn starting_equity(&self) -> f64 {
        self.cfg.starting_equity
    }
}

/// Handed to the strategy each footprint; records the desired next-bar position.
pub struct OrderCtx<'b> {
    broker: &'b mut Broker,
}

impl<'b> OrderCtx<'b> {
    pub fn new(broker: &'b mut Broker) -> Self {
        Self { broker }
    }

    /// Current position direction: -1 short, 0 flat, +1 long.
    pub fn position(&self) -> i32 {
        self.broker.current_dir()
    }

    /// Desire a long position of `qty` next bar, with stop/target distances in ticks (0 = none).
    pub fn go_long(&mut self, qty: f64, stop_ticks: f64, target_ticks: f64) {
        self.broker.set_pending(Desired::Long {
            qty,
            stop_ticks,
            target_ticks,
        });
    }

    /// Desire a short position of `qty` next bar.
    pub fn go_short(&mut self, qty: f64, stop_ticks: f64, target_ticks: f64) {
        self.broker.set_pending(Desired::Short {
            qty,
            stop_ticks,
            target_ticks,
        });
    }

    /// Desire to be flat next bar.
    pub fn flatten(&mut self) {
        self.broker.set_pending(Desired::Flat);
    }
}
