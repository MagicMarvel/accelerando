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
    entry_trigger: Option<f64>,
    /// Optional wall-clock validity from the signal footprint close to the fill footprint open.
    max_fill_delay_ns: Option<i64>,
    /// Free-form setup tag carried onto the resulting [`Trade`] for later analysis.
    label: Option<String>,
    reward_risk: Option<f64>,
    add_risk_budget: Option<f64>,
    max_hold_bars: Option<usize>,
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
    StopPair {
        buy: EntryOrder,
        sell: EntryOrder,
        max_gap_ns: i64,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct PendingIntent {
    kind: PendingKind,
    age: usize,
    submitted_ts_ns: Option<i64>,
}

impl PendingIntent {
    fn new(kind: PendingKind, submitted_ts_ns: Option<i64>) -> Self {
        Self {
            kind,
            age: 0,
            submitted_ts_ns,
        }
    }
}

const LIMIT_ORDER_TTL_BARS: usize = 8;

#[derive(Clone, Debug)]
struct Position {
    id: u64,
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
    stop_entered_intrabar: bool,
    /// Completed bars this lot has been open; its fill bar counts as 1.
    bars_open: usize,
    max_hold_bars: Option<usize>,
    label: Option<String>,
}

impl Position {
    fn tighten_stop(&mut self, price: f64) {
        if price.is_finite()
            && self
                .stop
                .is_none_or(|stop| (price - stop) * self.dir as f64 > 0.0)
        {
            self.stop = Some(price);
        }
    }
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

/// A single open lot and its current protective stop.
#[derive(Clone, Copy, Debug)]
pub struct OpenLotInfo {
    pub id: u64,
    pub dir: i32,
    pub entry_px: f64,
    pub entry_ts_ns: i64,
    pub stop: Option<f64>,
}

/// Immutable broker state presented to a strategy for the current completed footprint.
#[derive(Clone, Debug)]
pub struct PortfolioSnapshot {
    pub position: i32,
    pub net_position_qty: f64,
    pub position_count: usize,
    pub pending_count: usize,
    pub open_position: Option<PositionInfo>,
    pub long_stop: Option<f64>,
    pub short_stop: Option<f64>,
    pub open_lots: Vec<OpenLotInfo>,
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
    StopMarket {
        trigger: f64,
    },
    Limit {
        price: f64,
        fill_min: Option<f64>,
        fill_max: Option<f64>,
    },
}

/// Stop/target specification attached atomically to an entry intent.
#[derive(Clone, Debug, PartialEq)]
pub enum BracketSpec {
    /// Absolute structural stop; target is computed from the slipped fill and rounded outward.
    RiskReward {
        stop: f64,
        reward_risk: f64,
    },
    /// Size a same-direction add using gross cash risk at its actual fill.
    RiskSizedAdd {
        stop: f64,
        reward_risk: Option<f64>,
        risk_budget: f64,
    },
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
    pub max_fill_delay_ns: Option<i64>,
    pub max_hold_bars: Option<usize>,
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
            max_fill_delay_ns: None,
            max_hold_bars: None,
        }
    }

    /// A stop entry becomes a market order once the trigger is reached.
    pub fn stop_market(side: TradeSide, qty: f64, trigger: f64) -> Self {
        Self {
            execution: EntryExecution::StopMarket { trigger },
            ..Self::market(side, qty)
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

    pub fn risk_reward_bracket(mut self, stop: f64, reward_risk: f64) -> Self {
        self.bracket = BracketSpec::RiskReward { stop, reward_risk };
        self
    }

    /// Keep the requested quantity when opening a position, or risk the specified
    /// cash amount when adding to a same-direction position. Quantity may be fractional.
    pub fn risk_sized_add_bracket(
        mut self,
        stop: f64,
        reward_risk: Option<f64>,
        risk_budget: f64,
    ) -> Self {
        self.bracket = BracketSpec::RiskSizedAdd {
            stop,
            reward_risk,
            risk_budget,
        };
        self
    }

    pub fn tagged(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Expire this entry instead of filling it after a weekend, holiday, or other stale data gap.
    pub fn expire_after_ns(mut self, max_fill_delay_ns: i64) -> Self {
        self.max_fill_delay_ns = Some(max_fill_delay_ns.max(0));
        self
    }

    /// Close this lot at the first open after it has held this many bars, including the fill bar.
    /// Available in footprint backtests.
    pub fn hold_for_bars(mut self, bars: usize) -> Self {
        self.max_hold_bars = Some(bars.max(1));
        self
    }

    fn into_order(self) -> EntryOrder {
        let (entry_limit, entry_min, entry_max, entry_trigger) = match self.execution {
            EntryExecution::Market => (None, None, None, None),
            EntryExecution::StopMarket { trigger } => (None, None, None, Some(trigger)),
            EntryExecution::Limit {
                price,
                fill_min,
                fill_max,
            } => (Some(price), fill_min, fill_max, None),
        };
        let (reward_risk, add_risk_budget) = match self.bracket {
            BracketSpec::RiskReward { reward_risk, .. } => (Some(reward_risk), None),
            BracketSpec::RiskSizedAdd {
                reward_risk,
                risk_budget,
                ..
            } => (reward_risk, Some(risk_budget)),
            _ => (None, None),
        };
        let (stop_ticks, target_ticks, stop_px, target_px) = match self.bracket {
            BracketSpec::RiskReward { stop, .. } | BracketSpec::RiskSizedAdd { stop, .. } => {
                (0.0, 0.0, Some(stop), None)
            }
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
            entry_trigger,
            max_fill_delay_ns: self.max_fill_delay_ns,
            label: self.tag,
            reward_risk,
            add_risk_budget,
            max_hold_bars: self.max_hold_bars,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum OrderAction {
    Replace(EntryIntent),
    Add(EntryIntent),
    CancelPending,
    ExitPosition,
    TightenStop(f64, Option<TradeSide>, Option<i64>),
    TightenLotStop(u64, f64),
    ReplaceStopPair {
        buy: EntryIntent,
        sell: EntryIntent,
        max_gap_ns: i64,
    },
}

/// Typed order command buffer produced by a strategy and applied by the engine afterward.
#[derive(Default)]
pub(crate) struct OrderOutput {
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

    pub fn tighten_stop(&mut self, price: f64) {
        self.actions
            .push(OrderAction::TightenStop(price, None, None));
    }

    pub fn tighten_stops_for_side(&mut self, side: TradeSide, price: f64) {
        self.actions
            .push(OrderAction::TightenStop(price, Some(side), None));
    }

    pub fn tighten_stops_for_side_opened_before(
        &mut self,
        side: TradeSide,
        price: f64,
        before_ns: i64,
    ) {
        self.actions
            .push(OrderAction::TightenStop(price, Some(side), Some(before_ns)));
    }

    pub fn tighten_lot_stop(&mut self, lot_id: u64, price: f64) {
        self.actions
            .push(OrderAction::TightenLotStop(lot_id, price));
    }

    pub fn replace_stop_pair(&mut self, buy: EntryIntent, sell: EntryIntent, max_gap_ns: i64) {
        self.actions.push(OrderAction::ReplaceStopPair {
            buy,
            sell,
            max_gap_ns,
        });
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
pub(crate) struct StrategyCommands {
    pub orders: OrderOutput,
    pub visuals: VisualOutput,
}

impl StrategyCommands {
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
    next_lot_id: u64,
    pending: Vec<PendingIntent>,
    last_footprint_ts_ns: Option<i64>,
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
            next_lot_id: 0,
            pending: Vec::new(),
            last_footprint_ts_ns: None,
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
        let exit_fee = self.commission(pos.qty);
        // Entry commission already hit `realized` when the lot opened; the trade record still
        // reports the full round-trip net so per-trade stats (win rate, PF, avg) match equity.
        let entry_fee = self.commission(pos.qty);
        self.realized += gross - exit_fee;
        let pnl = gross - exit_fee - entry_fee;
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
        let risk_target = if let Some(rr) = order.reward_risk {
            let Some(stop) = order.stop_px else {
                return;
            };
            let risk = (px - stop) * order.dir as f64;
            if !risk.is_finite()
                || risk + self.tick_size * 1e-9 < self.tick_size
                || !rr.is_finite()
                || rr <= 0.0
            {
                return;
            }
            let raw = (px + order.dir as f64 * rr * risk) / self.tick_size;
            Some(
                if order.dir > 0 {
                    (raw - 1e-9).ceil()
                } else {
                    (raw + 1e-9).floor()
                } * self.tick_size,
            )
        } else {
            None
        };
        let dir = order.dir;
        let qty = if let Some(budget) = order
            .add_risk_budget
            .filter(|_| self.positions.iter().any(|position| position.dir == dir))
        {
            let Some(stop) = order.stop_px else {
                return;
            };
            let risk = (px - stop) * f64::from(dir);
            if !risk.is_finite() || risk + self.tick_size * 1e-9 < self.tick_size {
                return;
            }
            budget / (risk * self.multiplier())
        } else {
            order.qty
        };
        if !qty.is_finite() || qty <= 0.0 {
            return;
        }
        self.realized -= self.commission(qty); // entry-side commission
        let stop = order.stop_px.or(if order.stop_ticks > 0.0 {
            Some(px - dir as f64 * order.stop_ticks * self.tick_size)
        } else {
            None
        });
        let target = risk_target
            .or(order.target_px)
            .or(if order.target_ticks > 0.0 {
                Some(px + dir as f64 * order.target_ticks * self.tick_size)
            } else {
                None
            });
        let id = self.next_lot_id;
        self.next_lot_id += 1;
        self.positions.push(Position {
            id,
            dir,
            qty,
            entry_px: px,
            entry_ts: ts,
            stop,
            target,
            max_adverse_excursion: 0.0,
            max_adverse_ticks: 0.0,
            entered_intrabar,
            stop_entered_intrabar: entered_intrabar && order.entry_trigger.is_some(),
            bars_open: 0,
            max_hold_bars: order.max_hold_bars,
            label: order.label.clone(),
        });
    }

    /// Apply tick slippage to a fill, always against the taker.
    fn slip(&self, px: f64, dir: i32) -> f64 {
        px + dir as f64 * self.cfg.slippage_ticks * self.tick_size
    }

    /// Called when a new footprint arrives, before indicators/strategy run on it.
    pub fn on_new_footprint(&mut self, fp: &Footprint) {
        for idx in (0..self.positions.len()).rev() {
            let lot = &self.positions[idx];
            if lot.max_hold_bars.is_some_and(|bars| lot.bars_open >= bars) {
                let price = self.slip(fp.open, -lot.dir);
                self.close_position_at(idx, price, fp.ts_first_ns, TradeReason::Signal);
            }
        }
        // 1) Realize pending position changes at this bar's open/intrabar limit touch.
        let pending = std::mem::take(&mut self.pending);
        let mut still_pending = Vec::new();
        for mut intent in pending {
            if self.transition(intent.kind.clone(), intent.age, intent.submitted_ts_ns, fp) {
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
        self.last_footprint_ts_ns = Some(fp.ts_last_ns);
    }

    fn transition(
        &mut self,
        kind: PendingKind,
        age: usize,
        submitted_ts_ns: Option<i64>,
        fp: &Footprint,
    ) -> bool {
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
            PendingKind::StopPair {
                buy,
                sell,
                max_gap_ns,
            } => {
                if self
                    .last_footprint_ts_ns
                    .is_some_and(|last| ts.saturating_sub(last) > max_gap_ns)
                {
                    return false;
                }
                let (Some(up), Some(down)) = (buy.entry_trigger, sell.entry_trigger) else {
                    return false;
                };
                let order = match stop_pair_trigger(fp, up, down) {
                    Some(TradeSide::Long) => buy,
                    Some(TradeSide::Short) => sell,
                    None => return true,
                };
                let raw = stop_market_fill(fp, order.dir, order.entry_trigger.unwrap())
                    .expect("selected stop was touched");
                self.open_position(&order, self.slip(raw, order.dir), ts, raw != open);
                return false;
            }
        };
        if let (Some(max_delay), Some(submitted)) = (order.max_fill_delay_ns, submitted_ts_ns) {
            if fp.ts_first_ns.saturating_sub(submitted) > max_delay {
                return false;
            }
        }
        let fill_px = match order.entry_trigger {
            Some(trigger) => stop_market_fill(fp, order.dir, trigger),
            None => next_bar_entry_fill(fp, order.dir, order.entry_limit),
        };
        let Some(fill_px) = fill_px else {
            return order.entry_trigger.is_some()
                || (order.entry_limit.is_some() && age < LIMIT_ORDER_TTL_BARS);
        };
        if !price_in_band(fill_px, order.entry_min, order.entry_max) {
            return false;
        }
        // A limit order that fills at its limit price (rather than at the bar's open) was filled
        // somewhere inside the bar; flag it so the entry bar's target is not credited.
        let entered_intrabar =
            (order.entry_limit.is_some() || order.entry_trigger.is_some()) && fill_px != open;
        if replace_existing {
            self.close_all_at_open(open, ts, TradeReason::Signal);
        }
        let entry_px = if order.entry_limit.is_some() {
            fill_px
        } else {
            self.slip(fill_px, order.dir)
        };
        self.open_position(&order, entry_px, ts, entered_intrabar);
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
            // An adverse extreme may precede an intrabar stop entry. Only the close is
            // known to be at or after the trigger, so use it for protection on that bar.
            let (stop_low, stop_high, stop_open) = if on_entry_bar && pos.stop_entered_intrabar {
                (fp.close, fp.close, fp.close)
            } else {
                (fp.low, fp.high, fp.open)
            };
            // Conservative ordering: assume the stop is touched before the target within a bar.
            // A bar that OPENS through the stop triggers at that open (gap-through) — a resting
            // stop cannot fill at a price the market never traded. A triggered stop becomes a
            // market order, so it pays `slippage_ticks` against the position (same as the
            // event-driven path and every other taker fill). Targets are resting limits: they fill
            // at the target price with no slippage.
            let exit = if pos.dir > 0 {
                if let Some(stop) = pos.stop {
                    if stop_low <= stop {
                        Some((self.slip(stop_open.min(stop), -1), TradeReason::StopLoss))
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
                    if stop_high >= stop {
                        Some((self.slip(stop_open.max(stop), 1), TradeReason::StopLoss))
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
            let (low, high) = if pos.stop_entered_intrabar && pos.entry_ts == fp.ts_first_ns {
                (fp.close, fp.close)
            } else {
                (fp.low, fp.high)
            };
            let adverse_px = if pos.dir > 0 {
                let worst = pos.stop.filter(|stop| low <= *stop).unwrap_or(low);
                (pos.entry_px - worst).max(0.0)
            } else {
                let worst = pos.stop.filter(|stop| high >= *stop).unwrap_or(high);
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
        self.pending
            .push(PendingIntent::new(kind, self.last_footprint_ts_ns));
    }

    fn add_pending(&mut self, order: EntryOrder) {
        self.pending.push(PendingIntent::new(
            PendingKind::Add(order),
            self.last_footprint_ts_ns,
        ));
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
            open_lots: self
                .positions
                .iter()
                .map(|p| OpenLotInfo {
                    id: p.id,
                    dir: p.dir,
                    entry_px: p.entry_px,
                    entry_ts_ns: p.entry_ts,
                    stop: p.stop,
                })
                .collect(),
            long_stop: self
                .positions
                .iter()
                .filter(|p| p.dir > 0)
                .filter_map(|p| p.stop)
                .reduce(f64::min),
            short_stop: self
                .positions
                .iter()
                .filter(|p| p.dir < 0)
                .filter_map(|p| p.stop)
                .reduce(f64::max),
        }
    }

    /// Apply a completed bar callback's queued commands.
    pub fn apply_context(
        &mut self,
        ctx: crate::StrategyContext<'_>,
    ) -> (Vec<Plot>, Vec<(String, f64, Option<String>)>) {
        self.apply_commands(ctx.into_commands())
    }
    /// Apply event callback commands at the supplied execution opportunity.
    pub fn apply_event_context(
        &mut self,
        ctx: crate::StrategyContext<'_>,
        ts: i64,
        price: f64,
    ) -> Result<(Vec<Plot>, Vec<(String, f64, Option<String>)>), String> {
        self.apply_event_commands(ctx.into_commands(), ts, price)
    }

    pub(crate) fn apply_commands(
        &mut self,
        output: StrategyCommands,
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
                OrderAction::TightenStop(price, side, before) => {
                    self.tighten_stop(price, side, before)
                }
                OrderAction::TightenLotStop(lot_id, price) => {
                    if let Some(position) = self.positions.iter_mut().find(|p| p.id == lot_id) {
                        position.tighten_stop(price);
                    }
                }
                OrderAction::ReplaceStopPair {
                    buy,
                    sell,
                    max_gap_ns,
                } => {
                    self.set_single_pending(PendingKind::StopPair {
                        buy: buy.into_order(),
                        sell: sell.into_order(),
                        max_gap_ns,
                    });
                }
            }
        }
        (plots, series)
    }

    fn tighten_stop(&mut self, price: f64, side: Option<TradeSide>, before: Option<i64>) {
        if !price.is_finite() {
            return;
        }
        for position in &mut self.positions {
            if side.is_some_and(|side| side.dir() != position.dir)
                || before.is_some_and(|before| position.entry_ts >= before)
            {
                continue;
            }
            position.tighten_stop(price);
        }
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

    /// Check only positions that existed before this print. Do not call on_new_footprint
    /// for these same prints: that would fill orders twice and use future OHLC extremes.
    pub fn on_trade_event(&mut self, ts: i64, price: f64) {
        let fp = Footprint::seed(ts, price);
        self.update_adverse_excursion(&fp);
        for idx in (0..self.positions.len()).rev() {
            let pos = &self.positions[idx];
            let dir = pos.dir;
            let exit = if pos.stop.is_some_and(|s| (price - s) * dir as f64 <= 0.0) {
                Some((self.slip(price, -dir), TradeReason::StopLoss))
            } else if pos.target.is_some_and(|t| (price - t) * dir as f64 >= 0.0) {
                Some((pos.target.unwrap(), TradeReason::TakeProfit))
            } else {
                None
            };
            if let Some((px, reason)) = exit {
                self.close_position_at(idx, px, ts, reason);
            }
        }
        self.last_footprint_ts_ns = Some(ts);
    }

    /// Apply market-only commands after an event callback at the current print plus slippage.
    /// Strategies own signal delays; exits for a new fill are first checked on the next event.
    pub(crate) fn apply_event_commands(
        &mut self,
        output: StrategyCommands,
        ts: i64,
        price: f64,
    ) -> Result<(Vec<Plot>, Vec<(String, f64, Option<String>)>), String> {
        let (actions, plots, series) = output.into_parts();
        for action in actions {
            let independent_add = matches!(&action, OrderAction::Add(_));
            match action {
                OrderAction::Replace(intent) | OrderAction::Add(intent) => {
                    if intent.execution != EntryExecution::Market {
                        return Err("event execution currently supports market entries only".into());
                    }
                    if intent.max_hold_bars.is_some() {
                        return Err("bar-count holding periods require footprint execution".into());
                    }
                    if !independent_add && !self.positions.is_empty() {
                        return Err("event execution supports one position at a time".into());
                    }
                    if !intent.qty.is_finite() || intent.qty <= 0.0 {
                        return Err("invalid event entry quantity".into());
                    }
                    let order = intent.into_order();
                    let before = self.positions.len();
                    self.open_position(&order, self.slip(price, order.dir), ts, false);
                    if self.positions.len() == before {
                        return Err("invalid event entry bracket".into());
                    }
                }
                OrderAction::CancelPending => self.clear_pending(),
                OrderAction::ExitPosition => self.close_all_at_open(price, ts, TradeReason::Signal),
                OrderAction::TightenStop(price, side, before) => {
                    self.tighten_stop(price, side, before)
                }
                OrderAction::TightenLotStop(lot_id, price) => {
                    if let Some(position) = self.positions.iter_mut().find(|p| p.id == lot_id) {
                        position.tighten_stop(price);
                    }
                }
                OrderAction::ReplaceStopPair { .. } => {
                    return Err("stop pairs require footprint execution".into());
                }
            }
        }
        Ok((plots, series))
    }

    /// Sample event-engine account equity at a completed display bar.
    pub fn sample_event_equity(&mut self, ts: i64, price: f64) {
        let equity = self.mark_to_market(price);
        self.peak_equity = self.peak_equity.max(equity);
        self.equity.push(EquityPoint {
            ts_ns: ts,
            equity,
            drawdown: equity - self.peak_equity,
        });
    }

    pub fn starting_equity(&self) -> f64 {
        self.cfg.starting_equity
    }
}

/// Resolve the first side of an OCO stop pair from a footprint. If both are inside
/// the bar and the order of prints is unknown, use the trigger nearest the open.
pub fn stop_pair_trigger(fp: &Footprint, upper: f64, lower: f64) -> Option<TradeSide> {
    match (
        reaches_stop(fp.high, upper, 1),
        reaches_stop(fp.low, lower, -1),
    ) {
        (true, true) if fp.open - lower < upper - fp.open => Some(TradeSide::Short),
        (true, _) => Some(TradeSide::Long),
        (_, true) => Some(TradeSide::Short),
        _ => None,
    }
}

fn reaches_stop(price: f64, trigger: f64, dir: i32) -> bool {
    // Decimal market prices can land a few ULPs off their exact tick after decoding.
    let tolerance = 4.0 * f64::EPSILON * price.abs().max(trigger.abs());
    price.is_finite() && trigger.is_finite() && (price - trigger) * dir as f64 >= -tolerance
}

fn stop_market_fill(fp: &Footprint, dir: i32, trigger: f64) -> Option<f64> {
    if dir > 0 {
        reaches_stop(fp.high, trigger, dir).then_some(fp.open.max(trigger))
    } else {
        reaches_stop(fp.low, trigger, dir).then_some(fp.open.min(trigger))
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
    #[test]
    fn event_adds_open_independent_lots_in_both_directions_and_exit_separately() {
        let mut b = Broker::new(BrokerConfig {
            slippage_ticks: 0.0,
            commission_per_contract: 2.0,
            ..BrokerConfig::default()
        });
        b.set_contract(1.0, 10.0);
        let mut output = StrategyCommands::new(false);
        output.orders.add(
            EntryIntent::market(TradeSide::Long, 1.0)
                .risk_reward_bracket(98.0, 1.0)
                .tagged("A"),
        );
        output.orders.add(
            EntryIntent::market(TradeSide::Long, 2.0)
                .risk_reward_bracket(99.0, 1.0)
                .tagged("B"),
        );
        output.orders.add(
            EntryIntent::market(TradeSide::Short, 1.0)
                .risk_reward_bracket(104.0, 1.0)
                .tagged("C"),
        );
        b.apply_event_commands(output, 10, 100.0).unwrap();
        assert_eq!(b.portfolio_snapshot().position_count, 3);
        assert!(b.trades.is_empty());
        let mut replacement = StrategyCommands::new(false);
        replacement
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).risk_reward_bracket(98.0, 1.0));
        assert!(b.apply_event_commands(replacement, 10, 100.0).is_err());
        assert_eq!(b.portfolio_snapshot().position_count, 3);
        b.on_trade_event(11, 101.0);
        assert_eq!(b.portfolio_snapshot().position_count, 2);
        assert_eq!(b.trades[0].label.as_deref(), Some("B"));
        assert_eq!(b.trades[0].pnl, 12.0);
        b.on_trade_event(12, 102.0);
        assert_eq!(b.portfolio_snapshot().position_count, 1);
        assert_eq!(b.trades[1].label.as_deref(), Some("A"));
        assert_eq!(b.trades[1].pnl, 16.0);
        b.on_trade_event(13, 104.0);
        assert_eq!(b.portfolio_snapshot().position_count, 0);
        assert_eq!(b.trades[2].label.as_deref(), Some("C"));
        assert_eq!(b.trades[2].pnl, -44.0);
        assert_eq!(b.trades[2].reason, TradeReason::StopLoss);
        assert!(b.trades.iter().all(|t| t.entry_ts_ns == 10));
        // Entry fees hit account cash on entry; Trade.pnl carries both sides, so the trade log
        // sums to the equity change.
        assert_eq!(b.trades.iter().map(|t| t.pnl).sum::<f64>(), -16.0);
        b.sample_event_equity(13, 104.0);
        assert_eq!(b.equity.last().unwrap().equity, b.starting_equity() - 16.0);
    }

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
    fn expiring_market_entry_does_not_fill_after_a_stale_time_gap() {
        let mut broker = broker();
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 100));

        let mut output = StrategyCommands::new(false);
        output.orders.replace(
            EntryIntent::market(TradeSide::Long, 1.0)
                .expire_after_ns(100)
                .tick_bracket(10.0, 2.0),
        );
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 1_000));

        assert_eq!(broker.position_count(), 0);
        assert_eq!(broker.pending_count(), 0);
    }

    #[test]
    fn expiring_market_entry_still_fills_the_timely_next_bar() {
        let mut broker = broker();
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 100));

        let mut output = StrategyCommands::new(false);
        output.orders.replace(
            EntryIntent::market(TradeSide::Long, 1.0)
                .expire_after_ns(100)
                .tick_bracket(10.0, 2.0),
        );
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 150));

        assert_eq!(broker.position_count(), 1);
        assert_eq!(broker.pending_count(), 0);
    }

    #[test]
    fn additive_orders_open_multiple_lots_and_exit_independently() {
        let mut broker = broker();
        let mut output = StrategyCommands::new(false);
        output
            .orders
            .add(EntryIntent::limit(TradeSide::Long, 1.0, 100.0).tick_bracket(10.0, 2.0));
        output
            .orders
            .add(EntryIntent::limit(TradeSide::Long, 1.0, 99.0).tick_bracket(10.0, 4.0));
        broker.apply_commands(output);

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
        let mut output = StrategyCommands::new(false);
        output
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        assert_eq!(broker.position_count(), 1);

        let mut output = StrategyCommands::new(false);
        output
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));
        assert_eq!(broker.position_count(), 1);
        assert!(broker.trades.is_empty());
    }

    #[test]
    fn limit_entries_keep_the_matched_price_with_slippage_enabled() {
        let bar = fp(100.0, 103.0, 97.0, 100.0, 1);
        for (side, limit, expected) in [
            (TradeSide::Long, 98.0, 98.0),
            (TradeSide::Short, 102.0, 102.0),
            (TradeSide::Long, 101.0, 100.0),
            (TradeSide::Short, 99.0, 100.0),
        ] {
            let mut broker = Broker::new(BrokerConfig {
                slippage_ticks: 2.0,
                ..BrokerConfig::default()
            });
            broker.set_contract(1.0, 1.0);
            let mut output = StrategyCommands::new(false);
            output.orders.add(EntryIntent::limit(side, 1.0, limit));
            broker.apply_commands(output);
            broker.on_new_footprint(&bar);

            assert_eq!(
                broker.portfolio_snapshot().open_position.unwrap().entry_px,
                expected
            );
        }
    }

    /// Enter 1 lot at the next open (with 1 tick slippage) and return the broker.
    fn slipped_entry(side: TradeSide, stop: f64, target: f64) -> Broker {
        let mut broker = Broker::new(BrokerConfig {
            slippage_ticks: 1.0,
            ..BrokerConfig::default()
        });
        broker.set_contract(1.0, 1.0);
        let mut output = StrategyCommands::new(false);
        output
            .orders
            .replace(EntryIntent::market(side, 1.0).tick_bracket(stop, target));
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        broker
    }

    #[test]
    fn triggered_stops_pay_slippage_but_targets_do_not() {
        // Long fills at 101 (open 100 + 1 tick); bracket anchored on the fill: stop 97, target 105.
        let mut b = slipped_entry(TradeSide::Long, 4.0, 4.0);
        assert_eq!(
            b.portfolio_snapshot().open_position.unwrap().entry_px,
            101.0
        );
        b.on_new_footprint(&fp(99.0, 99.0, 96.0, 98.0, 2)); // touches the stop intrabar
        assert_eq!(b.trades[0].reason, TradeReason::StopLoss);
        assert_eq!(b.trades[0].exit_px, 96.0, "stop 97 minus 1 tick slippage");

        // Gap through the stop: triggers at the open (95), then slips to 94.
        let mut b = slipped_entry(TradeSide::Long, 4.0, 4.0);
        b.on_new_footprint(&fp(95.0, 95.5, 94.5, 95.0, 2));
        assert_eq!(b.trades[0].exit_px, 94.0);

        // Short mirror: fills at 99, stop 103 -> exits at 104.
        let mut b = slipped_entry(TradeSide::Short, 4.0, 4.0);
        b.on_new_footprint(&fp(101.0, 103.5, 100.5, 101.0, 2));
        assert_eq!(b.trades[0].exit_px, 104.0);

        // Target is a resting limit: exact target price.
        let mut b = slipped_entry(TradeSide::Long, 4.0, 4.0);
        b.on_new_footprint(&fp(102.0, 105.0, 101.5, 104.0, 2));
        assert_eq!(b.trades[0].reason, TradeReason::TakeProfit);
        assert_eq!(b.trades[0].exit_px, 105.0);
    }

    #[test]
    fn flatten_closes_every_open_lot() {
        let mut broker = broker();
        let mut output = StrategyCommands::new(false);
        output
            .orders
            .add(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        output
            .orders
            .add(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));
        assert_eq!(broker.position_count(), 2);

        let mut output = StrategyCommands::new(false);
        output.orders.exit_position();
        broker.apply_commands(output);
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
        let mut output = StrategyCommands::new(false);
        output
            .orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).tick_bracket(10.0, 0.0));
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));

        let mut output = StrategyCommands::new(false);
        output.orders.cancel_pending();
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));

        assert_eq!(broker.position_count(), 1);
        assert!(broker.trades.is_empty());
    }

    #[test]
    fn entry_tag_is_bound_to_the_resulting_trade() {
        let mut broker = broker();
        let mut output = StrategyCommands::new(false);
        output.orders.replace(
            EntryIntent::market(TradeSide::Long, 1.0)
                .tick_bracket(10.0, 0.0)
                .tagged("breakout"),
        );
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(100.0, 100.0, 100.0, 100.0, 1));

        let mut output = StrategyCommands::new(false);
        output.orders.exit_position();
        broker.apply_commands(output);
        broker.on_new_footprint(&fp(101.0, 101.0, 101.0, 101.0, 2));

        assert_eq!(broker.trades.len(), 1);
        assert_eq!(broker.trades[0].label.as_deref(), Some("breakout"));
    }
}
