//! A minimal next-bar-fill broker simulator and the typed strategy input/output API.
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

#[derive(Clone, Debug, PartialEq)]
struct EntryOrder {
    dir: i32,
    qty: f64,
    stop_ticks: f64,
    target_ticks: f64,
    /// Absolute stop price; wins over `stop_ticks` when set. Structural stops (a level plus a
    /// buffer) should use this so an open gap cannot silently move the protection.
    stop_px: Option<f64>,
    /// Absolute take-profit price; wins over `target_ticks` when set.
    target_px: Option<f64>,
    entry_min: Option<f64>,
    entry_max: Option<f64>,
    entry_limit: Option<f64>,
    /// Free-form setup tag carried onto the resulting [`Trade`] for later analysis.
    label: Option<String>,
}

/// The position change a strategy wants after the next fill.
#[derive(Clone, Debug, PartialEq)]
enum PendingKind {
    Flat,
    /// Legacy single-position behavior: if already purely in that direction, do nothing;
    /// otherwise close all lots and open one new lot.
    Replace(EntryOrder),
    /// Add a new lot without closing existing lots.
    Add(EntryOrder),
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug)]
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
    /// Completed bars this lot has been open; its fill bar counts as 1.
    bars_open: usize,
    label: Option<String>,
}

/// A snapshot of the open position visible to a strategy.
#[derive(Clone, Debug)]
pub struct PositionInfo {
    /// +1 net long, -1 net short.
    pub dir: i32,
    /// Total quantity across open lots in `dir`.
    pub qty: f64,
    /// Volume-weighted average entry price.
    pub entry_px: f64,
    /// Entry timestamp of the earliest open lot.
    pub entry_ts_ns: i64,
    /// Bars the earliest open lot has been held; its fill bar counts as 1.
    pub bars_held: usize,
    /// Stop of the earliest open lot.
    pub stop: Option<f64>,
    /// Target of the earliest open lot.
    pub target: Option<f64>,
}

/// Immutable broker state presented to a strategy for the current completed footprint.
#[derive(Clone, Debug)]
pub struct PortfolioSnapshot {
    pub position: i32,
    pub net_position_qty: f64,
    pub position_count: usize,
    pub pending_count: usize,
    pub open_position: Option<PositionInfo>,
}

/// Trading direction for an entry intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeSide {
    Long,
    Short,
}

impl TradeSide {
    fn dir(self) -> i32 {
        match self {
            Self::Long => 1,
            Self::Short => -1,
        }
    }
}

/// Entry price behavior. All entries are evaluated on a later footprint.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryExecution {
    Market,
    Limit {
        price: f64,
        fill_min: Option<f64>,
        fill_max: Option<f64>,
    },
}

/// Stop/target specification attached atomically to an entry intent.
#[derive(Clone, Debug, PartialEq)]
pub enum BracketSpec {
    Ticks {
        stop: f64,
        target: f64,
    },
    Prices {
        stop: Option<f64>,
        target: Option<f64>,
    },
}

/// A complete next-bar entry request, including its analysis tag.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryIntent {
    pub side: TradeSide,
    pub qty: f64,
    pub execution: EntryExecution,
    pub bracket: BracketSpec,
    pub tag: Option<String>,
}

impl EntryIntent {
    pub fn market(side: TradeSide, qty: f64) -> Self {
        Self {
            side,
            qty,
            execution: EntryExecution::Market,
            bracket: BracketSpec::Ticks {
                stop: 0.0,
                target: 0.0,
            },
            tag: None,
        }
    }

    pub fn limit(side: TradeSide, qty: f64, price: f64) -> Self {
        Self {
            execution: EntryExecution::Limit {
                price,
                fill_min: None,
                fill_max: None,
            },
            ..Self::market(side, qty)
        }
    }

    pub fn fill_between(mut self, min: Option<f64>, max: Option<f64>) -> Self {
        if let EntryExecution::Limit {
            fill_min, fill_max, ..
        } = &mut self.execution
        {
            *fill_min = min;
            *fill_max = max;
        }
        self
    }

    pub fn tick_bracket(mut self, stop: f64, target: f64) -> Self {
        self.bracket = BracketSpec::Ticks { stop, target };
        self
    }

    pub fn price_bracket(mut self, stop: Option<f64>, target: Option<f64>) -> Self {
        self.bracket = BracketSpec::Prices { stop, target };
        self
    }

    pub fn tagged(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    fn into_order(self) -> EntryOrder {
        let (entry_limit, entry_min, entry_max) = match self.execution {
            EntryExecution::Market => (None, None, None),
            EntryExecution::Limit {
                price,
                fill_min,
                fill_max,
            } => (Some(price), fill_min, fill_max),
        };
        let (stop_ticks, target_ticks, stop_px, target_px) = match self.bracket {
            BracketSpec::Ticks { stop, target } => (stop, target, None, None),
            BracketSpec::Prices { stop, target } => (0.0, 0.0, stop, target),
        };
        EntryOrder {
            dir: self.side.dir(),
            qty: self.qty,
            stop_ticks,
            target_ticks,
            stop_px,
            target_px,
            entry_min,
            entry_max,
            entry_limit,
            label: self.tag,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum OrderAction {
    Replace(EntryIntent),
    Add(EntryIntent),
    CancelPending,
    ExitPosition,
}

/// Typed order command buffer produced by a strategy and applied by the engine afterward.
#[derive(Default)]
pub struct OrderOutput {
    actions: Vec<OrderAction>,
}

impl OrderOutput {
    pub fn replace(&mut self, intent: EntryIntent) {
        self.actions.push(OrderAction::Replace(intent));
    }

    pub fn add(&mut self, intent: EntryIntent) {
        self.actions.push(OrderAction::Add(intent));
    }

    pub fn cancel_pending(&mut self) {
        self.actions.push(OrderAction::CancelPending);
    }

    pub fn exit_position(&mut self) {
        self.actions.push(OrderAction::ExitPosition);
    }
}

/// Chart-only output. It can be disabled for headless parameter searches.
pub struct VisualOutput {
    enabled: bool,
    plots: Vec<Plot>,
    series: Vec<(String, f64, Option<String>)>,
}

impl VisualOutput {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            plots: Vec::new(),
            series: Vec::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn push(&mut self, plot: Plot) {
        if self.enabled {
            self.plots.push(plot);
        }
    }

    pub fn series(&mut self, id: &str, value: f64) {
        self.series.push((id.to_string(), value, None));
    }

    pub fn series_colored(&mut self, id: &str, value: f64, color: &str) {
        self.series
            .push((id.to_string(), value, Some(color.to_string())));
    }
}

/// All commands emitted by one strategy callback.
pub struct StrategyOutput {
    pub orders: OrderOutput,
    pub visuals: VisualOutput,
}

impl StrategyOutput {
    pub fn new(visuals_enabled: bool) -> Self {
        Self {
            orders: OrderOutput::default(),
            visuals: VisualOutput::new(visuals_enabled),
        }
    }

    fn into_parts(
        self,
    ) -> (
        Vec<OrderAction>,
        Vec<Plot>,
        Vec<(String, f64, Option<String>)>,
    ) {
        (self.orders.actions, self.visuals.plots, self.visuals.series)
    }
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
            label: pos.label,
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
    /// Absolute bracket prices on the order win over tick distances.
    fn open_position(&mut self, order: &EntryOrder, px: f64, ts: i64, entered_intrabar: bool) {
        if order.qty <= 0.0 {
            return;
        }
        self.realized -= self.commission(order.qty); // entry-side commission
        let dir = order.dir;
        let stop = order.stop_px.or(if order.stop_ticks > 0.0 {
            Some(px - dir as f64 * order.stop_ticks * self.tick_size)
        } else {
            None
        });
        let target = order.target_px.or(if order.target_ticks > 0.0 {
            Some(px + dir as f64 * order.target_ticks * self.tick_size)
        } else {
            None
        });
        self.positions.push(Position {
            dir,
            qty: order.qty,
            entry_px: px,
            entry_ts: ts,
            stop,
            target,
            max_adverse_excursion: 0.0,
            max_adverse_ticks: 0.0,
            entered_intrabar,
            bars_open: 0,
            label: order.label.clone(),
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
            if self.transition(intent.kind.clone(), intent.age, fp) {
                intent.age += 1;
                still_pending.push(intent);
            }
        }
        self.pending = still_pending;
        // 2) Track holding time and adverse movement, then check stops/targets against this bar.
        for pos in &mut self.positions {
            pos.bars_open += 1;
        }
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
        self.open_position(&order, self.slip(fill_px, order.dir), ts, entered_intrabar);
        false
    }

    fn check_exits(&mut self, fp: &Footprint) {
        for idx in (0..self.positions.len()).rev() {
            let pos = self.positions[idx].clone();
            // On the bar an intrabar limit entry filled, the favorable extreme may predate the
            // fill, so the target is not eligible yet (avoids look-ahead). The adverse extreme is
            // reached after the fill, so the stop stays active. From the next bar on, both are
            // eligible normally.
            let on_entry_bar = pos.entry_ts == fp.ts_first_ns;
            let target_eligible = !(on_entry_bar && pos.entered_intrabar);
            // Conservative ordering: assume the stop is touched before the target within a bar.
            // A bar that OPENS through the stop fills at that open (gap-through) — a resting
            // stop cannot fill at a price the market never traded. Targets stay at the target
            // price, which is the conservative side for a resting limit.
            let exit = if pos.dir > 0 {
                if let Some(stop) = pos.stop {
                    if fp.low <= stop {
                        Some((fp.open.min(stop), TradeReason::StopLoss))
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
                        Some((fp.open.max(stop), TradeReason::StopLoss))
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

    /// Snapshot of the open position (lots in the current net direction), if any.
    pub fn open_position_info(&self) -> Option<PositionInfo> {
        let dir = self.current_dir();
        if dir == 0 {
            return None;
        }
        let lots: Vec<&Position> = self.positions.iter().filter(|p| p.dir == dir).collect();
        let qty: f64 = lots.iter().map(|p| p.qty).sum();
        if qty <= 0.0 {
            return None;
        }
        let entry_px = lots.iter().map(|p| p.entry_px * p.qty).sum::<f64>() / qty;
        let oldest = lots
            .iter()
            .max_by_key(|p| p.bars_open)
            .expect("non-empty lots");
        Some(PositionInfo {
            dir,
            qty,
            entry_px,
            entry_ts_ns: oldest.entry_ts,
            bars_held: oldest.bars_open,
            stop: oldest.stop,
            target: oldest.target,
        })
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

    pub fn portfolio_snapshot(&self) -> PortfolioSnapshot {
        PortfolioSnapshot {
            position: self.current_dir(),
            net_position_qty: self.net_position_qty(),
            position_count: self.position_count(),
            pending_count: self.pending_count(),
            open_position: self.open_position_info(),
        }
    }

    pub fn apply_strategy_output(
        &mut self,
        output: StrategyOutput,
    ) -> (Vec<Plot>, Vec<(String, f64, Option<String>)>) {
        let (actions, plots, series) = output.into_parts();
        for action in actions {
            match action {
                OrderAction::Replace(intent) => {
                    self.set_single_pending(PendingKind::Replace(intent.into_order()))
                }
                OrderAction::Add(intent) => self.add_pending(intent.into_order()),
                OrderAction::CancelPending => self.clear_pending(),
                OrderAction::ExitPosition => self.set_single_pending(PendingKind::Flat),
            }
        }
        (plots, series)
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
        let mut output = StrategyOutput::new(false);
        output
            .orders
            .add(EntryIntent::limit(TradeSide::Long, 1.0, 100.0).tick_bracket(10.0, 2.0));
        output
            .orders
            .add(EntryIntent::limit(TradeSide::Long, 1.0, 99.0).tick_bracket(10.0, 4.0));
        broker.apply_strategy_output(output);

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
    fn replace_order_does_not_stack_same_direction_lots() {
        let mut broker = broker();
        let mut output = StrategyOutput::new(false);
        output
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        assert_eq!(broker.position_count(), 1);

        let mut output = StrategyOutput::new(false);
        output
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));
        assert_eq!(broker.position_count(), 1);
        assert!(broker.trades.is_empty());
    }

    #[test]
    fn flatten_closes_every_open_lot() {
        let mut broker = broker();
        let mut output = StrategyOutput::new(false);
        output
            .orders
            .add(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        output
            .orders
            .add(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        assert_eq!(broker.position_count(), 2);

        let mut output = StrategyOutput::new(false);
        output.orders.exit_position();
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));
        assert_eq!(broker.position_count(), 0);
        assert_eq!(broker.trades.len(), 2);
        assert!(broker
            .trades
            .iter()
            .all(|trade| trade.reason == TradeReason::Signal));
    }

    #[test]
    fn cancel_pending_does_not_exit_an_open_position() {
        let mut broker = broker();
        let mut output = StrategyOutput::new(false);
        output
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));

        let mut output = StrategyOutput::new(false);
        output.orders.cancel_pending();
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));

        assert_eq!(broker.position_count(), 1);
        assert!(broker.trades.is_empty());
    }

    #[test]
    fn entry_tag_is_bound_to_the_resulting_trade() {
        let mut broker = broker();
        let mut output = StrategyOutput::new(false);
        output.orders.replace(
            EntryIntent::market(TradeSide::Long, 1.0)
                .tick_bracket(10.0, 0.0)
                .tagged("breakout"),
        );
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));

        let mut output = StrategyOutput::new(false);
        output.orders.exit_position();
        broker.apply_strategy_output(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));

        assert_eq!(broker.trades.len(), 1);
        assert_eq!(broker.trades[0].label.as_deref(), Some("breakout"));
    }
}
