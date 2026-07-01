//! A minimal next-bar-fill broker simulator and the [`OrderCtx`] strategies use to express intent.
//!
//! Execution model: a strategy sets a *desired* position on footprint `i`; the broker realizes
//! that transition at the **open** of footprint `i+1` (market order, tick slippage + commission),
//! then checks the open position's stop/target against `i+1`'s high/low intrabar.

use crate::footprint::{Footprint, Plot};
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
        entry_min: Option<f64>,
        entry_max: Option<f64>,
        entry_limit: Option<f64>,
    },
    Short {
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
        entry_limit: Option<f64>,
    },
}

const LIMIT_ORDER_TTL_BARS: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Position {
    dir: i32,
    qty: f64,
    entry_px: f64,
    entry_ts: i64,
    stop: Option<f64>,
    target: Option<f64>,
    max_adverse_excursion: f64,
    max_adverse_ticks: f64,
    /// True when the entry was a limit filled *inside* the bar (not at the open). On such a bar the
    /// profit target must not be credited, because the favorable extreme may have occurred before
    /// the fill — counting it would be look-ahead. The adverse extreme is reached after the fill, so
    /// the stop stays active.
    entered_intrabar: bool,
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
    pending_age: usize,
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
            pending_age: 0,
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
                stop: pos.stop,
                target: pos.target,
                max_adverse_excursion: pos.max_adverse_excursion,
                max_adverse_ticks: pos.max_adverse_ticks,
                pnl,
                reason,
            });
        }
    }

    /// Open a fresh position at `px` (slippage already applied), charging entry commission.
    #[allow(clippy::too_many_arguments)]
    fn open_position(
        &mut self,
        dir: i32,
        qty: f64,
        px: f64,
        ts: i64,
        stop_ticks: f64,
        target_ticks: f64,
        entered_intrabar: bool,
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
            max_adverse_excursion: 0.0,
            max_adverse_ticks: 0.0,
            entered_intrabar,
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
            if self.transition(desired, fp) {
                self.pending = Some(desired);
                self.pending_age += 1;
            } else {
                self.pending_age = 0;
            }
        }
        // 2) Track adverse movement after entry, then check stops/targets against this bar.
        self.update_adverse_excursion(fp);
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

    fn transition(&mut self, desired: Desired, fp: &Footprint) -> bool {
        let open = fp.open;
        let ts = fp.ts_first_ns;
        let (want_dir, qty, st, tt, entry_min, entry_max, entry_limit) = match desired {
            Desired::Flat => (0, 0.0, 0.0, 0.0, None, None, None),
            Desired::Long {
                qty,
                stop_ticks,
                target_ticks,
                entry_min,
                entry_max,
                entry_limit,
            } => (1, qty, stop_ticks, target_ticks, entry_min, entry_max, entry_limit),
            Desired::Short {
                qty,
                stop_ticks,
                target_ticks,
                entry_min,
                entry_max,
                entry_limit,
            } => (-1, qty, stop_ticks, target_ticks, entry_min, entry_max, entry_limit),
        };
        let cur_dir = self.position.map(|p| p.dir).unwrap_or(0);
        if cur_dir == want_dir {
            return false;
        }
        if want_dir == 0 {
            if self.position.is_some() {
                self.close_position(self.slip(open, -cur_dir), ts, TradeReason::Signal);
            }
            return false;
        }
        let Some(fill_px) = next_bar_entry_fill(fp, want_dir, entry_limit) else {
            return entry_limit.is_some() && self.pending_age < LIMIT_ORDER_TTL_BARS;
        };
        if !price_in_band(fill_px, entry_min, entry_max) {
            return false;
        }
        // A limit order that fills at its limit price (rather than at the bar's open) was filled
        // somewhere inside the bar; flag it so the entry bar's target is not credited.
        let entered_intrabar = entry_limit.is_some() && fill_px != open;
        if self.position.is_some() {
            self.close_position(self.slip(open, -cur_dir), ts, TradeReason::Signal);
        }
        self.open_position(
            want_dir,
            qty,
            self.slip(fill_px, want_dir),
            ts,
            st,
            tt,
            entered_intrabar,
        );
        false
    }

    fn check_exits(&mut self, fp: &Footprint) {
        let Some(pos) = self.position else { return };
        // On the bar an intrabar limit entry filled, the favorable extreme may predate the fill, so
        // the target is not eligible yet (avoids look-ahead). The adverse extreme is reached after
        // the fill, so the stop stays active. From the next bar on, both are eligible normally.
        let on_entry_bar = pos.entry_ts == fp.ts_first_ns;
        let target_eligible = !(on_entry_bar && pos.entered_intrabar);
        // Conservative ordering: assume stop is touched before target within the bar.
        if pos.dir > 0 {
            if let Some(stop) = pos.stop {
                if fp.low <= stop {
                    self.close_position(stop, fp.ts_last_ns, TradeReason::StopLoss);
                    return;
                }
            }
            if target_eligible {
                if let Some(target) = pos.target {
                    if fp.high >= target {
                        self.close_position(target, fp.ts_last_ns, TradeReason::TakeProfit);
                    }
                }
            }
        } else {
            if let Some(stop) = pos.stop {
                if fp.high >= stop {
                    self.close_position(stop, fp.ts_last_ns, TradeReason::StopLoss);
                    return;
                }
            }
            if target_eligible {
                if let Some(target) = pos.target {
                    if fp.low <= target {
                        self.close_position(target, fp.ts_last_ns, TradeReason::TakeProfit);
                    }
                }
            }
        }
    }

    fn update_adverse_excursion(&mut self, fp: &Footprint) {
        let Some(pos) = self.position.as_mut() else {
            return;
        };
        let adverse_px = if pos.dir > 0 {
            let worst = pos.stop.filter(|stop| fp.low <= *stop).unwrap_or(fp.low);
            (pos.entry_px - worst).max(0.0)
        } else {
            let worst = pos.stop.filter(|stop| fp.high >= *stop).unwrap_or(fp.high);
            (worst - pos.entry_px).max(0.0)
        };
        let adverse_ticks = if self.tick_size > 0.0 {
            adverse_px / self.tick_size
        } else {
            0.0
        };
        let adverse_currency = adverse_px * pos.qty * self.multiplier;
        pos.max_adverse_ticks = pos.max_adverse_ticks.max(adverse_ticks);
        pos.max_adverse_excursion = pos.max_adverse_excursion.max(adverse_currency);
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
        self.pending_age = 0;
    }

    fn current_dir(&self) -> i32 {
        self.position.map(|p| p.dir).unwrap_or(0)
    }

    /// Close any open position at end of stream and append a final equity point.
    pub fn finalize(&mut self, last_close: f64, ts: i64) {
        if self.position.is_some() {
            self.close_position(
                self.slip(last_close, -self.current_dir()),
                ts,
                TradeReason::EndOfData,
            );
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

fn next_bar_entry_fill(fp: &Footprint, dir: i32, limit: Option<f64>) -> Option<f64> {
    let Some(limit) = limit else {
        return Some(fp.open);
    };
    if dir > 0 {
        if fp.open <= limit {
            Some(fp.open)
        } else if fp.low <= limit {
            Some(limit)
        } else {
            None
        }
    } else if fp.open >= limit {
        Some(fp.open)
    } else if fp.high >= limit {
        Some(limit)
    } else {
        None
    }
}

fn price_in_band(px: f64, min: Option<f64>, max: Option<f64>) -> bool {
    if let Some(min) = min {
        if px < min {
            return false;
        }
    }
    if let Some(max) = max {
        if px > max {
            return false;
        }
    }
    true
}

/// Handed to the strategy each footprint; records the desired next-bar position and any
/// chart overlays the strategy wants drawn.
pub struct OrderCtx<'b> {
    broker: &'b mut Broker,
    plots: Vec<Plot>,
}

impl<'b> OrderCtx<'b> {
    pub fn new(broker: &'b mut Broker) -> Self {
        Self {
            broker,
            plots: Vec::new(),
        }
    }

    /// Append a chart overlay to this footprint (lines, markers, bands).
    pub fn plot(&mut self, p: Plot) {
        self.plots.push(p);
    }

    /// Drain the collected plots (called by the engine after the strategy returns).
    pub fn take_plots(self) -> Vec<Plot> {
        self.plots
    }

    /// Current position direction: -1 short, 0 flat, +1 long.
    pub fn position(&self) -> i32 {
        self.broker.current_dir()
    }

    /// Desire a long position of `qty` next bar, with stop/target distances in ticks (0 = none).
    pub fn go_long(&mut self, qty: f64, stop_ticks: f64, target_ticks: f64) {
        self.go_long_if_open_between(qty, stop_ticks, target_ticks, None, None);
    }

    /// Desire a long position only if the next bar opens inside the optional price band.
    pub fn go_long_if_open_between(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.set_pending(Desired::Long {
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            entry_limit: None,
        });
    }

    /// Desire a short position of `qty` next bar.
    pub fn go_short(&mut self, qty: f64, stop_ticks: f64, target_ticks: f64) {
        self.go_short_if_open_between(qty, stop_ticks, target_ticks, None, None);
    }

    /// Desire a short position only if the next bar opens inside the optional price band.
    pub fn go_short_if_open_between(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.set_pending(Desired::Short {
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            entry_limit: None,
        });
    }

    /// Desire a long position using a next-bar limit entry inside an optional price band.
    pub fn go_long_limit_next_bar(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_limit: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.set_pending(Desired::Long {
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            entry_limit: Some(entry_limit),
        });
    }

    /// Desire a short position using a next-bar limit entry inside an optional price band.
    pub fn go_short_limit_next_bar(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_limit: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.set_pending(Desired::Short {
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            entry_limit: Some(entry_limit),
        });
    }

    /// Desire to be flat next bar.
    pub fn flatten(&mut self) {
        self.broker.set_pending(Desired::Flat);
    }
}
