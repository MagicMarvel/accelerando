//! A minimal next-bar-fill broker simulator and the [`OrderCtx`] strategies use to express intent.
//!
//! Execution model: a strategy queues position changes on footprint `i`; the broker realizes
//! them on footprint `i+1`, then checks every open lot's stop/target against that footprint's
//! high/low intrabar.

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

#[derive(Clone, Copy, Debug, PartialEq)]
struct EntryOrder {
    dir: i32,
    qty: f64,
    stop_ticks: f64,
    target_ticks: f64,
    entry_min: Option<f64>,
    entry_max: Option<f64>,
    entry_limit: Option<f64>,
}

/// The position change a strategy wants after the next fill.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingKind {
    Flat,
    /// Legacy single-position behavior: if already purely in that direction, do nothing;
    /// otherwise close all lots and open one new lot.
    Replace(EntryOrder),
    /// Add a new lot without closing existing lots.
    Add(EntryOrder),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingIntent {
    kind: PendingKind,
    age: usize,
}

impl PendingIntent {
    fn new(kind: PendingKind) -> Self {
        Self { kind, age: 0 }
    }
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

/// The broker simulator: tracks open lots, fills next-bar orders, records trades and equity.
pub struct Broker {
    cfg: BrokerConfig,
    tick_size: f64,
    multiplier: f64,
    realized: f64,
    peak_equity: f64,
    positions: Vec<Position>,
    pending: Vec<PendingIntent>,
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
            positions: Vec::new(),
            pending: Vec::new(),
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

    /// Realize an exit of one lot at `px`, recording the trade.
    fn close_position_at(&mut self, idx: usize, px: f64, ts: i64, reason: TradeReason) {
        let pos = self.positions.remove(idx);
        self.record_close(pos, px, ts, reason);
    }

    fn record_close(&mut self, pos: Position, px: f64, ts: i64, reason: TradeReason) {
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

    fn close_all_at_open(&mut self, open: f64, ts: i64, reason: TradeReason) {
        let positions = std::mem::take(&mut self.positions);
        for pos in positions {
            let px = self.slip(open, -pos.dir);
            self.record_close(pos, px, ts, reason);
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
        if qty <= 0.0 {
            return;
        }
        self.positions.push(Position {
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
        // 1) Realize pending position changes at this bar's open/intrabar limit touch.
        let pending = std::mem::take(&mut self.pending);
        let mut still_pending = Vec::new();
        for mut intent in pending {
            if self.transition(intent.kind, intent.age, fp) {
                intent.age += 1;
                still_pending.push(intent);
            }
        }
        self.pending = still_pending;
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

    fn transition(&mut self, kind: PendingKind, age: usize, fp: &Footprint) -> bool {
        let open = fp.open;
        let ts = fp.ts_first_ns;
        let (replace_existing, order) = match kind {
            PendingKind::Flat => {
                self.close_all_at_open(open, ts, TradeReason::Signal);
                return false;
            }
            PendingKind::Replace(order) => {
                if self.has_only_position_dir(order.dir) {
                    return false;
                }
                (true, order)
            }
            PendingKind::Add(order) => (false, order),
        };
        let Some(fill_px) = next_bar_entry_fill(fp, order.dir, order.entry_limit) else {
            return order.entry_limit.is_some() && age < LIMIT_ORDER_TTL_BARS;
        };
        if !price_in_band(fill_px, order.entry_min, order.entry_max) {
            return false;
        }
        // A limit order that fills at its limit price (rather than at the bar's open) was filled
        // somewhere inside the bar; flag it so the entry bar's target is not credited.
        let entered_intrabar = order.entry_limit.is_some() && fill_px != open;
        if replace_existing {
            self.close_all_at_open(open, ts, TradeReason::Signal);
        }
        self.open_position(
            order.dir,
            order.qty,
            self.slip(fill_px, order.dir),
            ts,
            order.stop_ticks,
            order.target_ticks,
            entered_intrabar,
        );
        false
    }

    fn check_exits(&mut self, fp: &Footprint) {
        for idx in (0..self.positions.len()).rev() {
            let pos = self.positions[idx];
            // On the bar an intrabar limit entry filled, the favorable extreme may predate the
            // fill, so the target is not eligible yet (avoids look-ahead). The adverse extreme is
            // reached after the fill, so the stop stays active. From the next bar on, both are
            // eligible normally.
            let on_entry_bar = pos.entry_ts == fp.ts_first_ns;
            let target_eligible = !(on_entry_bar && pos.entered_intrabar);
            // Conservative ordering: assume stop is touched before target within the bar.
            let exit = if pos.dir > 0 {
                if let Some(stop) = pos.stop {
                    if fp.low <= stop {
                        Some((stop, TradeReason::StopLoss))
                    } else {
                        None
                    }
                } else {
                    None
                }
                .or_else(|| {
                    if target_eligible {
                        pos.target
                            .filter(|target| fp.high >= *target)
                            .map(|target| (target, TradeReason::TakeProfit))
                    } else {
                        None
                    }
                })
            } else {
                if let Some(stop) = pos.stop {
                    if fp.high >= stop {
                        Some((stop, TradeReason::StopLoss))
                    } else {
                        None
                    }
                } else {
                    None
                }
                .or_else(|| {
                    if target_eligible {
                        pos.target
                            .filter(|target| fp.low <= *target)
                            .map(|target| (target, TradeReason::TakeProfit))
                    } else {
                        None
                    }
                })
            };
            if let Some((px, reason)) = exit {
                self.close_position_at(idx, px, fp.ts_last_ns, reason);
            }
        }
    }

    fn update_adverse_excursion(&mut self, fp: &Footprint) {
        for pos in &mut self.positions {
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
    }

    fn mark_to_market(&self, px: f64) -> f64 {
        let unrealized = self
            .positions
            .iter()
            .map(|p| (px - p.entry_px) * p.dir as f64 * p.qty * self.multiplier)
            .sum::<f64>();
        self.cfg.starting_equity + self.realized + unrealized
    }

    fn set_single_pending(&mut self, kind: PendingKind) {
        self.pending.clear();
        self.pending.push(PendingIntent::new(kind));
    }

    fn add_pending(&mut self, order: EntryOrder) {
        self.pending
            .push(PendingIntent::new(PendingKind::Add(order)));
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
    }

    fn current_dir(&self) -> i32 {
        let net = self.net_position_qty();
        if net > f64::EPSILON {
            1
        } else if net < -f64::EPSILON {
            -1
        } else {
            0
        }
    }

    fn net_position_qty(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.dir as f64 * p.qty)
            .sum::<f64>()
    }

    fn position_count(&self) -> usize {
        self.positions.len()
    }

    fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn has_only_position_dir(&self, dir: i32) -> bool {
        !self.positions.is_empty() && self.positions.iter().all(|p| p.dir == dir)
    }

    /// Close any open position at end of stream and append a final equity point.
    pub fn finalize(&mut self, last_close: f64, ts: i64) {
        if !self.positions.is_empty() {
            let positions = std::mem::take(&mut self.positions);
            for pos in positions {
                let px = self.slip(last_close, -pos.dir);
                self.record_close(pos, px, ts, TradeReason::EndOfData);
            }
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

    /// Net position direction: -1 net short, 0 flat/hedged, +1 net long.
    pub fn position(&self) -> i32 {
        self.broker.current_dir()
    }

    /// Net signed quantity across all open lots.
    pub fn net_position_qty(&self) -> f64 {
        self.broker.net_position_qty()
    }

    /// Number of currently open lots.
    pub fn position_count(&self) -> usize {
        self.broker.position_count()
    }

    /// Number of active pending orders.
    pub fn pending_count(&self) -> usize {
        self.broker.pending_count()
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
        self.broker
            .set_single_pending(PendingKind::Replace(entry_order(
                1,
                qty,
                stop_ticks,
                target_ticks,
                entry_min,
                entry_max,
                None,
            )));
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
        self.broker
            .set_single_pending(PendingKind::Replace(entry_order(
                -1,
                qty,
                stop_ticks,
                target_ticks,
                entry_min,
                entry_max,
                None,
            )));
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
        self.broker
            .set_single_pending(PendingKind::Replace(entry_order(
                1,
                qty,
                stop_ticks,
                target_ticks,
                entry_min,
                entry_max,
                Some(entry_limit),
            )));
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
        self.broker
            .set_single_pending(PendingKind::Replace(entry_order(
                -1,
                qty,
                stop_ticks,
                target_ticks,
                entry_min,
                entry_max,
                Some(entry_limit),
            )));
    }

    /// Add a new long lot next bar without closing existing lots.
    pub fn add_long(&mut self, qty: f64, stop_ticks: f64, target_ticks: f64) {
        self.add_long_if_open_between(qty, stop_ticks, target_ticks, None, None);
    }

    /// Add a new long lot only if the next bar opens inside the optional price band.
    pub fn add_long_if_open_between(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.add_pending(entry_order(
            1,
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            None,
        ));
    }

    /// Add a new short lot next bar without closing existing lots.
    pub fn add_short(&mut self, qty: f64, stop_ticks: f64, target_ticks: f64) {
        self.add_short_if_open_between(qty, stop_ticks, target_ticks, None, None);
    }

    /// Add a new short lot only if the next bar opens inside the optional price band.
    pub fn add_short_if_open_between(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.add_pending(entry_order(
            -1,
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            None,
        ));
    }

    /// Add a new long lot using a next-bar limit entry without closing existing lots.
    pub fn add_long_limit_next_bar(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_limit: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.add_pending(entry_order(
            1,
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            Some(entry_limit),
        ));
    }

    /// Add a new short lot using a next-bar limit entry without closing existing lots.
    pub fn add_short_limit_next_bar(
        &mut self,
        qty: f64,
        stop_ticks: f64,
        target_ticks: f64,
        entry_limit: f64,
        entry_min: Option<f64>,
        entry_max: Option<f64>,
    ) {
        self.broker.add_pending(entry_order(
            -1,
            qty,
            stop_ticks,
            target_ticks,
            entry_min,
            entry_max,
            Some(entry_limit),
        ));
    }

    /// Cancel resting pending orders without closing existing lots.
    pub fn cancel_pending_orders(&mut self) {
        self.broker.clear_pending();
    }

    /// Desire to be flat next bar.
    pub fn flatten(&mut self) {
        self.broker.set_single_pending(PendingKind::Flat);
    }
}

fn entry_order(
    dir: i32,
    qty: f64,
    stop_ticks: f64,
    target_ticks: f64,
    entry_min: Option<f64>,
    entry_max: Option<f64>,
    entry_limit: Option<f64>,
) -> EntryOrder {
    EntryOrder {
        dir,
        qty,
        stop_ticks,
        target_ticks,
        entry_min,
        entry_max,
        entry_limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::Level;
    use std::collections::BTreeMap;

    fn fp(open: f64, high: f64, low: f64, close: f64, ts: i64) -> Footprint {
        Footprint {
            ts_first_ns: ts,
            ts_last_ns: ts + 1,
            open,
            high,
            low,
            close,
            volume: 0.0,
            trades: 0,
            delta: 0.0,
            poc: close,
            ladder: Vec::<Level>::new(),
            values: BTreeMap::new(),
            tags: BTreeMap::new(),
            plots: Vec::new(),
        }
    }

    fn broker() -> Broker {
        let mut broker = Broker::new(BrokerConfig::default());
        broker.set_contract(1.0, 1.0);
        broker
    }

    #[test]
    fn additive_orders_open_multiple_lots_and_exit_independently() {
        let mut broker = broker();
        {
            let mut ctx = OrderCtx::new(&mut broker);
            ctx.add_long_limit_next_bar(1.0, 10.0, 2.0, 100.0, None, None);
            ctx.add_long_limit_next_bar(1.0, 10.0, 4.0, 99.0, None, None);
        }

        broker.on_new_footprint(&fp(100.0, 100.0, 99.0, 99.0, 1));
        assert_eq!(broker.position_count(), 2);
        assert_eq!(broker.current_dir(), 1);

        broker.on_new_footprint(&fp(99.0, 103.0, 99.0, 103.0, 2));
        assert_eq!(broker.position_count(), 0);
        assert_eq!(broker.trades.len(), 2);
        assert!(broker
            .trades
            .iter()
            .all(|trade| trade.reason == TradeReason::TakeProfit));
        let pnl = broker.trades.iter().map(|trade| trade.pnl).sum::<f64>();
        assert_eq!(pnl, 6.0);
    }

    #[test]
    fn legacy_replace_order_does_not_stack_same_direction_lots() {
        let mut broker = broker();
        {
            let mut ctx = OrderCtx::new(&mut broker);
            ctx.go_long(1.0, 10.0, 0.0);
        }
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        assert_eq!(broker.position_count(), 1);

        {
            let mut ctx = OrderCtx::new(&mut broker);
            ctx.go_long(1.0, 10.0, 0.0);
        }
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));
        assert_eq!(broker.position_count(), 1);
        assert!(broker.trades.is_empty());
    }

    #[test]
    fn flatten_closes_every_open_lot() {
        let mut broker = broker();
        {
            let mut ctx = OrderCtx::new(&mut broker);
            ctx.add_long(1.0, 10.0, 0.0);
            ctx.add_long(1.0, 10.0, 0.0);
        }
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        assert_eq!(broker.position_count(), 2);

        {
            let mut ctx = OrderCtx::new(&mut broker);
            ctx.flatten();
        }
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));
        assert_eq!(broker.position_count(), 0);
        assert_eq!(broker.trades.len(), 2);
        assert!(broker
            .trades
            .iter()
            .all(|trade| trade.reason == TradeReason::Signal));
    }
}
