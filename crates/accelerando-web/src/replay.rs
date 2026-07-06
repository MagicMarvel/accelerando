//! Interactive manual bar-by-bar replay: a paper-trading session laid on top of the studio
//! chart page. A host app hands [`ReplayManager`] a prepared footprint series; players step
//! forward one (or a few) bars at a time, place market/limit/breakout orders with a stop/target
//! bracket, and each session is persisted to a JSON file so it can be reviewed afterwards.
//!
//! Wired in via [`crate::serve_experiment_lazy_heatmap_with_replay`].

use accelerando_core::result::EquityPoint;
use accelerando_core::{
    BacktestResult, ExperimentRunSummary, Footprint, LiquidityHeatmap, Metrics,
    PreparedBacktestData, Trade, TradeReason,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::Request;

const STARTING_EQUITY: f64 = 100_000.0;
const COMMISSION_PER_CONTRACT: f64 = 2.0;
const SLIPPAGE_TICKS: f64 = 1.0;

/// Manual bar-replay engine. Construct one per server run and pass it to
/// [`crate::serve_experiment_lazy_heatmap_with_replay`].
#[derive(Clone)]
pub struct ReplayManager {
    prepared: Arc<PreparedBacktestData>,
    source: String,
    record_dir: PathBuf,
    sessions: Arc<Mutex<HashMap<String, ReplaySession>>>,
}

impl ReplayManager {
    /// `prepared` supplies the footprint series to step through; `source` is recorded on each
    /// session for provenance; `record_dir` is where per-session JSON snapshots are written.
    pub fn new(
        prepared: Arc<PreparedBacktestData>,
        source: impl Into<String>,
        record_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            prepared,
            source: source.into(),
            record_dir: record_dir.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn create_session(&self) -> Result<ReplayPublicState, String> {
        if self.prepared.footprints.is_empty() {
            return Err("no footprints are available for replay".to_string());
        }
        let created_at_ms = now_ms();
        let id = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            format!("replay_{created_at_ms}_{}", sessions.len() + 1)
        };
        let mut session = ReplaySession {
            id: id.clone(),
            source: self.source.clone(),
            created_at_ms,
            updated_at_ms: created_at_ms,
            cursor: 0,
            realized_pnl: 0.0,
            peak_equity: STARTING_EQUITY,
            pending_order: None,
            open_position: None,
            trades: Vec::new(),
            equity: Vec::new(),
            events: Vec::new(),
        };
        self.mark_equity_at(&mut session, 0)?;
        session.events.push(ReplayEvent::new(
            session.cursor,
            "created",
            serde_json::json!({ "source": session.source }),
        ));
        self.save_session(&session)?;
        self.sessions
            .lock()
            .map_err(|_| "replay session lock poisoned".to_string())?
            .insert(id, session.clone());
        Ok(self.public_state(&session))
    }

    pub(crate) fn state_value(&self, id: &str) -> Result<Value, String> {
        let session = self.session(id)?;
        let result = self.result_for(&session)?;
        let mut value = serde_json::to_value(result).map_err(|e| format!("serialize: {e}"))?;
        if let Value::Object(obj) = &mut value {
            obj.insert(
                "replay".to_string(),
                serde_json::to_value(self.public_state(&session))
                    .map_err(|e| format!("serialize replay state: {e}"))?,
            );
        }
        Ok(value)
    }

    pub(crate) fn place_order(
        &self,
        id: &str,
        req: OrderRequest,
    ) -> Result<ReplayPublicState, String> {
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            if session.open_position.is_some() {
                return Err("close the open position before placing another order".to_string());
            }
            let dir = normalize_dir(req.dir)?;
            let order_type = OrderType::from_str(&req.order_type)?;
            let qty = finite_positive(req.qty, "qty")?;
            let current = self.current_footprint(session)?;
            let raw_entry = match order_type {
                OrderType::Market => current.close,
                OrderType::Limit | OrderType::Breakout => req.entry_price.ok_or_else(|| {
                    "entry_price is required for limit and breakout orders".to_string()
                })?,
            };
            let entry_ref = match order_type {
                OrderType::Market => self.slip(raw_entry, dir),
                _ => raw_entry,
            };
            let stop = finite_required(req.stop, "stop")?;
            let target = finite_required(req.target, "target")?;
            validate_protection(dir, entry_ref, stop, target)?;

            match order_type {
                OrderType::Market => {
                    let position = self.open_position(
                        session.cursor,
                        dir,
                        qty,
                        entry_ref,
                        current.ts_last_ns,
                        stop,
                        target,
                        false,
                        order_type,
                    );
                    session.realized_pnl -= position.entry_fee;
                    session.open_position = Some(position);
                    session.pending_order = None;
                    session.events.push(ReplayEvent::new(
                        session.cursor,
                        "market_entry",
                        serde_json::json!({ "dir": dir, "qty": qty, "entry_px": entry_ref, "stop": stop, "target": target }),
                    ));
                    self.mark_equity_at(session, session.cursor)?;
                }
                OrderType::Limit | OrderType::Breakout => {
                    session.pending_order = Some(PendingOrder {
                        order_type,
                        dir,
                        qty,
                        entry_price: raw_entry,
                        stop,
                        target,
                        created_bar: session.cursor,
                    });
                    session.events.push(ReplayEvent::new(
                        session.cursor,
                        "pending_order",
                        serde_json::json!({ "type": order_type, "dir": dir, "qty": qty, "entry_price": raw_entry, "stop": stop, "target": target }),
                    ));
                }
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    pub(crate) fn advance(&self, id: &str, count: usize) -> Result<ReplayPublicState, String> {
        let count = count.max(1).min(500);
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            for _ in 0..count {
                if session.cursor + 1 >= self.prepared.footprints.len() {
                    break;
                }
                session.cursor += 1;
                self.process_current_bar(session)?;
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    pub(crate) fn update_active_order(
        &self,
        id: &str,
        req: UpdateOrderRequest,
    ) -> Result<ReplayPublicState, String> {
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            if let Some(pos) = session.open_position.as_mut() {
                let stop = req.stop.unwrap_or(pos.stop);
                let target = req.target.unwrap_or(pos.target);
                validate_protection(pos.dir, pos.entry_px, stop, target)?;
                pos.stop = stop;
                pos.target = target;
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "update_position_bracket",
                    serde_json::json!({ "stop": stop, "target": target }),
                ));
            } else if let Some(order) = session.pending_order.as_mut() {
                if let Some(entry) = req.entry_price {
                    order.entry_price = finite_value(entry, "entry_price")?;
                }
                if let Some(order_type) = req.order_type.as_deref() {
                    order.order_type = OrderType::from_str(order_type)?;
                }
                let stop = req.stop.unwrap_or(order.stop);
                let target = req.target.unwrap_or(order.target);
                validate_protection(order.dir, order.entry_price, stop, target)?;
                order.stop = stop;
                order.target = target;
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "update_pending_order",
                    serde_json::json!({ "type": order.order_type, "entry_price": order.entry_price, "stop": stop, "target": target }),
                ));
            } else {
                return Err("there is no pending order or open position to update".to_string());
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    pub(crate) fn cancel_pending(&self, id: &str) -> Result<ReplayPublicState, String> {
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            if session.pending_order.take().is_some() {
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "cancel_pending",
                    serde_json::json!({}),
                ));
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    pub(crate) fn flatten(&self, id: &str) -> Result<ReplayPublicState, String> {
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            let fp = self.current_footprint(session)?.clone();
            if let Some(pos) = session.open_position.take() {
                let exit_px = self.slip(fp.close, -pos.dir);
                self.close_position(session, pos, exit_px, fp.ts_last_ns, TradeReason::Signal);
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "manual_flatten",
                    serde_json::json!({ "exit_px": exit_px }),
                ));
                self.mark_equity_at(session, session.cursor)?;
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    pub(crate) fn validate_heatmap_query(&self, id: &str, query: &str) -> bool {
        let Ok(session) = self.session(id) else {
            return false;
        };
        let Some(last) = self.prepared.footprints.get(session.cursor) else {
            return false;
        };
        let t1 = crate::query_param_from_query(query, "t1")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(i64::MAX);
        t1 <= last.ts_last_ns
    }

    fn session(&self, id: &str) -> Result<ReplaySession, String> {
        self.sessions
            .lock()
            .map_err(|_| "replay session lock poisoned".to_string())?
            .get(id)
            .cloned()
            .ok_or_else(|| format!("replay session not found: {id}"))
    }

    fn current_footprint<'a>(&'a self, session: &ReplaySession) -> Result<&'a Footprint, String> {
        self.prepared
            .footprints
            .get(session.cursor)
            .ok_or_else(|| "cursor is outside the prepared data".to_string())
    }

    fn process_current_bar(&self, session: &mut ReplaySession) -> Result<(), String> {
        let fp = self.current_footprint(session)?.clone();
        if session.open_position.is_none() {
            if let Some(order) = session.pending_order.clone() {
                if let Some((raw_fill, entered_intrabar)) = fill_pending_order(&order, &fp) {
                    let fill_px = if order.order_type == OrderType::Breakout {
                        self.slip(raw_fill, order.dir)
                    } else {
                        raw_fill
                    };
                    validate_protection(order.dir, fill_px, order.stop, order.target)?;
                    let position = self.open_position(
                        session.cursor,
                        order.dir,
                        order.qty,
                        fill_px,
                        fp.ts_first_ns,
                        order.stop,
                        order.target,
                        entered_intrabar,
                        order.order_type,
                    );
                    session.realized_pnl -= position.entry_fee;
                    session.open_position = Some(position);
                    session.pending_order = None;
                    session.events.push(ReplayEvent::new(
                        session.cursor,
                        "pending_fill",
                        serde_json::json!({ "entry_px": fill_px, "type": order.order_type }),
                    ));
                }
            }
        }

        if let Some(mut pos) = session.open_position.take() {
            update_adverse(
                &mut pos,
                &fp,
                self.prepared.tick_size,
                self.prepared.multiplier,
            );
            let skip_target = pos.entered_intrabar && pos.entry_bar == session.cursor;
            if let Some((exit_px, reason)) = exit_for_bar(&pos, &fp, skip_target) {
                self.close_position(session, pos, exit_px, fp.ts_last_ns, reason);
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "exit",
                    serde_json::json!({ "exit_px": exit_px, "reason": reason }),
                ));
            } else {
                session.open_position = Some(pos);
            }
        }
        self.mark_equity_at(session, session.cursor)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_position(
        &self,
        entry_bar: usize,
        dir: i32,
        qty: f64,
        entry_px: f64,
        entry_ts_ns: i64,
        stop: f64,
        target: f64,
        entered_intrabar: bool,
        source_order: OrderType,
    ) -> OpenPosition {
        OpenPosition {
            dir,
            qty,
            entry_px,
            entry_ts_ns,
            entry_bar,
            stop,
            target,
            max_adverse_excursion: 0.0,
            max_adverse_ticks: 0.0,
            entered_intrabar,
            source_order,
            entry_fee: commission(qty),
        }
    }

    fn close_position(
        &self,
        session: &mut ReplaySession,
        pos: OpenPosition,
        exit_px: f64,
        exit_ts_ns: i64,
        reason: TradeReason,
    ) {
        let gross = (exit_px - pos.entry_px) * pos.dir as f64 * pos.qty * self.prepared.multiplier;
        let exit_fee = commission(pos.qty);
        let pnl = gross - pos.entry_fee - exit_fee;
        session.realized_pnl += gross - exit_fee;
        session.trades.push(Trade {
            entry_ts_ns: pos.entry_ts_ns,
            exit_ts_ns,
            dir: pos.dir,
            qty: pos.qty,
            entry_px: pos.entry_px,
            exit_px,
            stop: Some(pos.stop),
            target: Some(pos.target),
            max_adverse_excursion: pos.max_adverse_excursion,
            max_adverse_ticks: pos.max_adverse_ticks,
            pnl,
            reason,
        });
    }

    fn mark_equity_at(&self, session: &mut ReplaySession, cursor: usize) -> Result<(), String> {
        let fp = self
            .prepared
            .footprints
            .get(cursor)
            .ok_or_else(|| "cursor is outside the prepared data".to_string())?;
        let mut equity = STARTING_EQUITY + session.realized_pnl;
        if let Some(pos) = &session.open_position {
            equity +=
                (fp.close - pos.entry_px) * pos.dir as f64 * pos.qty * self.prepared.multiplier;
        }
        if session
            .equity
            .last()
            .is_some_and(|last| last.ts_ns == fp.ts_last_ns)
        {
            session.equity.pop();
        }
        session.peak_equity = session
            .equity
            .iter()
            .fold(STARTING_EQUITY, |peak, point| peak.max(point.equity))
            .max(equity);
        session.equity.push(EquityPoint {
            ts_ns: fp.ts_last_ns,
            equity,
            drawdown: equity - session.peak_equity,
        });
        Ok(())
    }

    fn result_for(&self, session: &ReplaySession) -> Result<BacktestResult, String> {
        let end = session
            .cursor
            .saturating_add(1)
            .min(self.prepared.footprints.len());
        let footprints = self.prepared.footprints[..end].to_vec();
        let last_ts = footprints
            .last()
            .map(|fp| fp.ts_last_ns)
            .unwrap_or_default();
        let liquidity_heatmap = LiquidityHeatmap {
            snapshots: self
                .prepared
                .liquidity_heatmap
                .snapshots
                .iter()
                .take_while(|snap| snap.ts_ns <= last_ts)
                .cloned()
                .collect(),
        };
        let metrics = Metrics::compute(STARTING_EQUITY, &session.trades, &session.equity);
        Ok(BacktestResult {
            metrics,
            trades: session.trades.clone(),
            equity: session.equity.clone(),
            footprints,
            liquidity_heatmap,
            tick_size: self.prepared.tick_size,
            multiplier: self.prepared.multiplier,
        })
    }

    fn public_state(&self, session: &ReplaySession) -> ReplayPublicState {
        let current = self.prepared.footprints.get(session.cursor);
        ReplayPublicState {
            id: session.id.clone(),
            source: session.source.clone(),
            record_path: self.record_path(&session.id).display().to_string(),
            created_at_ms: session.created_at_ms,
            updated_at_ms: session.updated_at_ms,
            cursor: session.cursor,
            total_bars: self.prepared.footprints.len(),
            current_close: current.map(|fp| fp.close),
            done: session.cursor + 1 >= self.prepared.footprints.len(),
            pending_order: session.pending_order.clone(),
            open_position: session.open_position.clone(),
            realized_pnl: session.realized_pnl,
            trades: session.trades.len(),
        }
    }

    fn save_session(&self, session: &ReplaySession) -> Result<(), String> {
        fs::create_dir_all(&self.record_dir)
            .map_err(|e| format!("create replay record dir: {e}"))?;
        let path = self.record_path(&session.id);
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = File::create(&tmp).map_err(|e| format!("create replay record: {e}"))?;
            serde_json::to_writer_pretty(&mut file, session)
                .map_err(|e| format!("serialize replay record: {e}"))?;
            writeln!(file).map_err(|e| format!("finish replay record: {e}"))?;
        }
        fs::rename(&tmp, &path).map_err(|e| format!("save replay record: {e}"))?;
        Ok(())
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.record_dir.join(format!("{id}.json"))
    }

    fn slip(&self, px: f64, dir: i32) -> f64 {
        px + dir as f64 * SLIPPAGE_TICKS * self.prepared.tick_size
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReplaySession {
    id: String,
    source: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    cursor: usize,
    realized_pnl: f64,
    peak_equity: f64,
    pending_order: Option<PendingOrder>,
    open_position: Option<OpenPosition>,
    trades: Vec<Trade>,
    equity: Vec<EquityPoint>,
    events: Vec<ReplayEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReplayEvent {
    ts_ms: u64,
    bar: usize,
    event: String,
    data: Value,
}

impl ReplayEvent {
    fn new(bar: usize, event: &str, data: Value) -> Self {
        Self {
            ts_ms: now_ms(),
            bar,
            event: event.to_string(),
            data,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OrderType {
    Market,
    Limit,
    Breakout,
}

impl OrderType {
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "market" => Ok(Self::Market),
            "limit" => Ok(Self::Limit),
            "breakout" => Ok(Self::Breakout),
            _ => Err("order_type must be market, limit, or breakout".to_string()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingOrder {
    order_type: OrderType,
    dir: i32,
    qty: f64,
    entry_price: f64,
    stop: f64,
    target: f64,
    created_bar: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpenPosition {
    dir: i32,
    qty: f64,
    entry_px: f64,
    entry_ts_ns: i64,
    entry_bar: usize,
    stop: f64,
    target: f64,
    max_adverse_excursion: f64,
    max_adverse_ticks: f64,
    entered_intrabar: bool,
    source_order: OrderType,
    entry_fee: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReplayPublicState {
    pub(crate) id: String,
    source: String,
    record_path: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    cursor: usize,
    total_bars: usize,
    current_close: Option<f64>,
    done: bool,
    pending_order: Option<PendingOrder>,
    open_position: Option<OpenPosition>,
    realized_pnl: f64,
    trades: usize,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrderRequest {
    dir: i32,
    order_type: String,
    qty: f64,
    entry_price: Option<f64>,
    stop: Option<f64>,
    target: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StepRequest {
    pub(crate) count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateOrderRequest {
    order_type: Option<String>,
    entry_price: Option<f64>,
    stop: Option<f64>,
    target: Option<f64>,
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(request: &mut Request) -> Result<T, String> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("read request body: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("parse json: {e}"))
}

fn fill_pending_order(order: &PendingOrder, fp: &Footprint) -> Option<(f64, bool)> {
    match order.order_type {
        OrderType::Market => Some((fp.open, false)),
        OrderType::Limit if order.dir > 0 => {
            if fp.open <= order.entry_price {
                Some((fp.open, false))
            } else if fp.low <= order.entry_price {
                Some((order.entry_price, true))
            } else {
                None
            }
        }
        OrderType::Limit => {
            if fp.open >= order.entry_price {
                Some((fp.open, false))
            } else if fp.high >= order.entry_price {
                Some((order.entry_price, true))
            } else {
                None
            }
        }
        OrderType::Breakout if order.dir > 0 => {
            if fp.open >= order.entry_price {
                Some((fp.open, false))
            } else if fp.high >= order.entry_price {
                Some((order.entry_price, true))
            } else {
                None
            }
        }
        OrderType::Breakout => {
            if fp.open <= order.entry_price {
                Some((fp.open, false))
            } else if fp.low <= order.entry_price {
                Some((order.entry_price, true))
            } else {
                None
            }
        }
    }
}

fn update_adverse(pos: &mut OpenPosition, fp: &Footprint, tick_size: f64, multiplier: f64) {
    let adverse_px = if pos.dir > 0 {
        let worst = if fp.low <= pos.stop { pos.stop } else { fp.low };
        (pos.entry_px - worst).max(0.0)
    } else {
        let worst = if fp.high >= pos.stop {
            pos.stop
        } else {
            fp.high
        };
        (worst - pos.entry_px).max(0.0)
    };
    let adverse_ticks = if tick_size > 0.0 {
        adverse_px / tick_size
    } else {
        0.0
    };
    pos.max_adverse_ticks = pos.max_adverse_ticks.max(adverse_ticks);
    pos.max_adverse_excursion = pos
        .max_adverse_excursion
        .max(adverse_px * pos.qty * multiplier);
}

fn exit_for_bar(
    pos: &OpenPosition,
    fp: &Footprint,
    skip_target: bool,
) -> Option<(f64, TradeReason)> {
    if pos.dir > 0 {
        if fp.low <= pos.stop {
            Some((pos.stop, TradeReason::StopLoss))
        } else if !skip_target && fp.high >= pos.target {
            Some((pos.target, TradeReason::TakeProfit))
        } else {
            None
        }
    } else if fp.high >= pos.stop {
        Some((pos.stop, TradeReason::StopLoss))
    } else if !skip_target && fp.low <= pos.target {
        Some((pos.target, TradeReason::TakeProfit))
    } else {
        None
    }
}

fn validate_protection(dir: i32, entry: f64, stop: f64, target: f64) -> Result<(), String> {
    finite_value(entry, "entry_price")?;
    finite_value(stop, "stop")?;
    finite_value(target, "target")?;
    if dir > 0 {
        if !(stop < entry && target > entry) {
            return Err("long orders require stop below entry and target above entry".to_string());
        }
    } else if !(stop > entry && target < entry) {
        return Err("short orders require stop above entry and target below entry".to_string());
    }
    Ok(())
}

fn normalize_dir(dir: i32) -> Result<i32, String> {
    match dir {
        1 => Ok(1),
        -1 => Ok(-1),
        _ => Err("dir must be 1 for long or -1 for short".to_string()),
    }
}

fn finite_required(value: Option<f64>, name: &str) -> Result<f64, String> {
    value
        .ok_or_else(|| format!("{name} is required"))
        .and_then(|v| finite_value(v, name))
}

fn finite_positive(value: f64, name: &str) -> Result<f64, String> {
    let value = finite_value(value, name)?;
    if value > 0.0 {
        Ok(value)
    } else {
        Err(format!("{name} must be positive"))
    }
}

fn finite_value(value: f64, name: &str) -> Result<f64, String> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{name} must be finite"))
    }
}

fn commission(qty: f64) -> f64 {
    COMMISSION_PER_CONTRACT * qty
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// The experiment index page with a "New bar replay" button wired to `POST /api/replay/new`.
pub(crate) fn experiment_html_with_replay_button() -> String {
    replace_script_tail(
        crate::EXPERIMENT_HTML.replace(
            r#"<button id="expandAll">Expand all</button>"#,
            r#"<button id="newReplay">New bar replay</button>
    <button id="expandAll">Expand all</button>"#,
        ),
        &experiment_script_tail(),
    )
}

fn experiment_script_tail() -> String {
    r#"document.getElementById("newReplay").addEventListener("click", async()=>{
  const r=await fetch("/api/replay/new",{method:"POST"});
  const data=await r.json().catch(()=>({}));
  if(!r.ok||data.ok===false){ alert(data.error||"failed to create replay"); return; }
  location.href=data.url;
});
init();
</script>"#
        .to_string()
}

/// The studio chart page adapted for manual replay: chart-order UI, SL/TP drag handles, and its
/// data feed pointed at `/api/replay/state` instead of `/api/result`.
pub(crate) fn replay_html(session_id: &str, annotation_json: &str) -> String {
    let summary = ExperimentRunSummary {
        id: session_id.to_string(),
        label: "manual_replay".to_string(),
        strategy: "manual_replay".to_string(),
        params: serde_json::json!({ "mode": "manual_bar_replay" }),
        metrics: Metrics::default(),
    };
    let escaped = crate::json_string(session_id);
    let escaped_strategy = crate::json_string("manual_replay");
    let summary_json = serde_json::to_string(&summary).expect("serialize replay summary");
    replace_script_tail(
        crate::studio_html()
            .replace("</style>", &format!("{}\n</style>", REPLAY_CSS))
            .replace(
                "const price=$(\"price\"), pctx=price.getContext(\"2d\");",
                &format!(
                    "globalThis.RUN_ID={escaped};\nglobalThis.RUN_STRATEGY={escaped_strategy};\nglobalThis.RUN_SUMMARY={summary_json};\nglobalThis.ANNOTATION_CONFIG={annotation_json};\nconst price=$(\"price\"), pctx=price.getContext(\"2d\");"
                ),
            )
            .replace(
                "fetch(\"/api/result\")",
                "fetch(\"/api/replay/state?id=\"+encodeURIComponent(globalThis.RUN_ID||\"\"))",
            ),
        &replay_script_tail(),
    )
}

fn replay_script_tail() -> String {
    format!("{REPLAY_JS}\ninstallReplayUi();\ninit();\n</script>")
}

fn replace_script_tail(html: String, tail: &str) -> String {
    if html.contains("init();\r\n</script>") {
        html.replace("init();\r\n</script>", tail)
    } else {
        html.replace("init();\n</script>", tail)
    }
}

const REPLAY_CSS: &str = r#"
.replay-panel{display:flex;gap:8px;flex-wrap:wrap;align-items:center;padding:7px 14px;background:#fff;border-bottom:1px solid #e2e8f0}
.replay-panel label{display:inline-flex;gap:5px;align-items:center;font-size:12px;color:#374151}
.replay-panel input,.replay-panel select{height:28px;border:1px solid #cbd5e1;border-radius:6px;padding:3px 6px;font-size:12px;background:#fff}
.replay-panel input{width:72px}.replay-panel button{height:28px;padding:0 9px;border-radius:6px;font-size:12px}
.replay-panel .primary{background:#2563eb;border-color:#2563eb;color:#fff;font-weight:800}
.replay-panel .danger{color:#b45309}.replay-panel .state{font-size:12px;color:#475569;font-weight:700}
.replay-panel .err{font-size:12px;color:#b45309;min-width:160px}
.replay-panel .mode{font-size:12px;color:#0f172a;background:#eef2ff;border:1px solid #c7d2fe;border-radius:6px;padding:5px 8px;font-weight:800}
.replay-panel .mode.armed{background:#16a34a;color:#fff;border-color:#16a34a}
.replay-panel .hint{font-size:12px;color:#64748b}
.charts.replay-armed #price{cursor:crosshair}
"#;

const REPLAY_JS: &str = r##"
let REPLAY={};
let replayDrag=null;
let replayDraft=null;
let replaySpace=false;
let replayHoverPrice=null;
const REPLAY_DEFAULT_RISK_TICKS=8;
const REPLAY_R_MULTIPLE=2;
function replayTick(){return (DATA&&DATA.tick_size)||0.25;}
function snapPrice(v){const t=replayTick();return Math.round(Number(v)/t)*t;}
function num(id){const v=parseFloat($(id).value);return Number.isFinite(v)?v:null;}
function setNum(id,v){$(id).value=(v==null||!Number.isFinite(v))?"":Number(v).toFixed(2);}
function replayQty(){return Math.max(0.01,num("replayQty")||1);}
function replayCurrentPrice(){return REPLAY.current_close??currentVisiblePrice();}
function replaySideLabel(dir){return dir>0?"BUY":"SELL";}
function replayDirForButton(button){return button===2?-1:1;}
function replayOrderType(entry,dir){
  const ref=replayCurrentPrice();
  if(ref==null)return "limit";
  return dir>0?(entry<=ref?"limit":"breakout"):(entry>=ref?"limit":"breakout");
}
function replayTypeLabel(type){return type==="breakout"?"STOP":"LMT";}
function replayClickText(price,button){
  const dir=replayDirForButton(button), type=replayOrderType(price,dir);
  return `${button===2?"RIGHT":"LEFT"} ${replayTypeLabel(type)} ${replaySideLabel(dir)} ${price.toFixed(2)}`;
}
function replayDefaultBracket(entry,dir){
  const risk=REPLAY_DEFAULT_RISK_TICKS*replayTick();
  return {stop:snapPrice(entry-dir*risk),target:snapPrice(entry+dir*risk*REPLAY_R_MULTIPLE)};
}
function replayDraftFrom(entry,dir,at){
  entry=snapPrice(entry); at=snapPrice(at);
  const type=replayOrderType(entry,dir);
  const t=replayTick();
  let stop,target;
  const signed=(at-entry)*dir;
  if(Math.abs(signed)<t){
    const b=replayDefaultBracket(entry,dir); stop=b.stop; target=b.target;
  } else if(signed<0){
    stop=at;
    const risk=Math.max(t,Math.abs(entry-stop));
    target=snapPrice(entry+dir*risk*REPLAY_R_MULTIPLE);
  } else {
    target=at;
    const reward=Math.max(t,Math.abs(target-entry));
    stop=snapPrice(entry-dir*Math.max(t,reward/REPLAY_R_MULTIPLE));
  }
  return {scope:"draft",dir,order_type:type,qty:replayQty(),entry_price:entry,stop,target};
}
function installReplayUi(){
  const content=document.querySelector(".content");
  content.style.gridTemplateRows="auto auto auto auto 1fr";
  const panel=document.createElement("div");
  panel.className="replay-panel";
  panel.innerHTML=`<button id="replayStep" class="primary">Next K</button><button id="replayStep5">+5</button>
    <label>Qty <input id="replayQty" type="number" min="0.01" step="0.01" value="1"></label>
    <span id="replayMode" class="mode">Hold Space</span>
    <button id="replayCancel">Cancel order</button><button id="replayFlat" class="danger">Flat</button>
    <span id="replayState" class="state"></span><span id="replayHint" class="hint"></span><span id="replayErr" class="err"></span>`;
  content.insertBefore(panel, document.querySelector(".charts"));
  const oldRender=renderResult;
  renderResult=function(data){oldRender(data);REPLAY=data.replay||{};syncReplayUi();};
  const oldDraw=drawPrice;
  drawPrice=function(){oldDraw();drawReplayOverlay();};
  $("modeHeatmap").style.display="none";
  const heat=$("heatMetric"); if(heat&&heat.parentElement)heat.parentElement.style.display="none";
  $("replayStep").onclick=()=>replayAction("/api/replay/step",{count:1});
  $("replayStep5").onclick=()=>replayAction("/api/replay/step",{count:5});
  $("replayCancel").onclick=()=>replayAction("/api/replay/cancel",{});
  $("replayFlat").onclick=()=>replayAction("/api/replay/flatten",{});
  for(const id of ["replayQty"]){
    $(id).addEventListener("input",()=>{replayDraft=null;syncReplayUi();draw();});
    $(id).addEventListener("change",()=>{replayDraft=null;syncReplayUi();draw();});
  }
  price.addEventListener("mousedown",replayMouseDown,true);
  price.addEventListener("mousemove",replayHoverMove,true);
  price.addEventListener("mouseleave",replayHoverLeave,true);
  price.addEventListener("contextmenu",replayContextMenu,true);
  window.addEventListener("mousemove",replayMouseMove,true);
  window.addEventListener("mouseup",replayMouseUp,true);
  window.addEventListener("keydown",replayKeyDown,true);
  window.addEventListener("keyup",replayKeyUp,true);
  window.addEventListener("blur",()=>setReplaySpace(false));
  updateReplayMode();
}
function syncReplayUi(){
  const st=$("replayState");
  st.textContent=REPLAY.id?`bar ${Math.min(REPLAY.cursor+1,REPLAY.total_bars)} / ${REPLAY.total_bars}  trades ${REPLAY.trades||0}  saved ${REPLAY.record_path||""}`:"";
  if(REPLAY.pending_order){
    const o=REPLAY.pending_order; setNum("replayQty",o.qty);
  } else if(REPLAY.open_position){
    const p=REPLAY.open_position; setNum("replayQty",p.qty);
  }
  $("replayStep").disabled=!!REPLAY.done;
  $("replayCancel").disabled=!REPLAY.pending_order;
  $("replayFlat").disabled=!REPLAY.open_position;
  updateReplayMode();
}
function replayKeyEditable(){
  const tag=(document.activeElement&&document.activeElement.tagName||"").toLowerCase();
  return tag==="input"||tag==="textarea"||tag==="select";
}
function replayKeyDown(ev){
  if(ev.code!=="Space"||replayKeyEditable())return;
  setReplaySpace(true);
  ev.preventDefault();
}
function replayKeyUp(ev){
  if(ev.code!=="Space")return;
  setReplaySpace(false);
  ev.preventDefault();
}
function setReplaySpace(on){
  replaySpace=!!on;
  if(replaySpace&&hover&&hover.canvas==="price"&&priceScale)replayHoverPrice=replayPriceFromCanvasY(hover.y);
  if(!replaySpace&&!replayDrag)replayHoverPrice=null;
  updateReplayMode();
  draw();
}
function updateReplayMode(){
  const mode=$("replayMode"), hint=$("replayHint");
  if(!mode||!hint)return;
  mode.classList.toggle("armed",replaySpace);
  const blocked=REPLAY.open_position?"position open":REPLAY.pending_order?"order pending":"";
  mode.textContent=replaySpace&&!blocked?"Chart order":replaySpace?blocked:"Hold Space";
  if(REPLAY.open_position)hint.textContent="Drag SL/TP lines to modify, Flat to close.";
  else if(REPLAY.pending_order)hint.textContent="Drag entry/SL/TP lines to modify, Cancel order to remove.";
  else if(replaySpace&&replayHoverPrice!=null)hint.textContent=`${replayClickText(replayHoverPrice,0)} | ${replayClickText(replayHoverPrice,2)}. Drag to draw SL/TP.`;
  else if(replaySpace)hint.textContent="Move over the chart. Left click buys, right click sells; price decides LMT or STOP.";
  else hint.textContent="Hold Space on the chart: left click BUY, right click SELL. Drag out SL or TP before release.";
}
function replayOrderError(body){
  if(!body)return "no order";
  const entry=body.entry_price, stop=body.stop, target=body.target, dir=body.dir;
  if(entry==null||stop==null||target==null)return "entry, SL and TP required";
  if(dir>0&&!(stop<entry&&target>entry))return "long: SL < entry < TP";
  if(dir<0&&!(target<entry&&entry<stop))return "short: TP < entry < SL";
  return "";
}
async function placeReplayOrder(body){
  const err=replayOrderError(body);
  if(err){$("replayErr").textContent=err;return false;}
  return await replayAction("/api/replay/order",body);
}
async function replayAction(path,body){
  $("replayErr").textContent="";
  const r=await fetch(path+"?id="+encodeURIComponent(globalThis.RUN_ID||""),{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(body||{})});
  const data=await r.json().catch(()=>({}));
  if(!r.ok||data.ok===false){$("replayErr").textContent=data.error||`failed ${r.status}`;return false;}
  await loadResult();
  return true;
}
function replayLines(){
  const lines=[];
  if(replayDraft) {
    const o=replayDraft, scope=o.scope||"draft";
    const entryLabel=scope==="open"?`OPEN ${replaySideLabel(o.dir)}`:`${replayTypeLabel(o.order_type)} ${replaySideLabel(o.dir)}`;
    lines.push({kind:"entry",scope,price:o.entry_price,label:entryLabel,color:"#111827",drag:false});
    lines.push({kind:"stop",scope,price:o.stop,label:"SL",color:COLORS.down,drag:false});
    lines.push({kind:"target",scope,price:o.target,label:"TP",color:COLORS.up,drag:false});
  } else if(REPLAY.pending_order){
    const o=REPLAY.pending_order;
    lines.push({kind:"entry",scope:"pending",price:o.entry_price,label:`${replayTypeLabel(o.order_type)} ${replaySideLabel(o.dir)}`,color:"#111827",drag:true});
    lines.push({kind:"stop",scope:"pending",price:o.stop,label:"SL",color:COLORS.down,drag:true});
    lines.push({kind:"target",scope:"pending",price:o.target,label:"TP",color:COLORS.up,drag:true});
  } else if(REPLAY.open_position){
    const p=REPLAY.open_position;
    lines.push({kind:"entry",scope:"open",price:p.entry_px,label:`OPEN ${replaySideLabel(p.dir)}`,color:"#111827",drag:false});
    lines.push({kind:"stop",scope:"open",price:p.stop,label:"SL",color:COLORS.down,drag:true});
    lines.push({kind:"target",scope:"open",price:p.target,label:"TP",color:COLORS.up,drag:true});
  }
  return lines.filter(l=>Number.isFinite(l.price));
}
function drawReplayOverlay(){
  if(!priceScale||chartMode==="heatmap")return;
  const {L,T,pw,ph,lo,hi}=priceScale, y=p=>T+(hi-p)/(hi-lo)*ph;
  pctx.save(); pctx.font="11px Segoe UI,Arial"; pctx.textBaseline="middle";
  for(const line of replayLines()){
    const yy=y(line.price); if(yy<T-8||yy>T+ph+8)continue;
    pctx.strokeStyle=colorAlpha(line.color,0.9); pctx.lineWidth=1.6; pctx.setLineDash(line.kind==="entry"?[5,4]:[]);
    pctx.beginPath(); pctx.moveTo(L,yy); pctx.lineTo(L+pw,yy); pctx.stroke(); pctx.setLineDash([]);
    pctx.fillStyle=line.color; pctx.fillText(`${line.label} ${Number(line.price).toFixed(2)}`,L+6,yy-8);
  }
  if(replaySpace&&!REPLAY.pending_order&&!REPLAY.open_position){
    pctx.fillStyle="#16a34a"; pctx.font="12px Segoe UI,Arial"; pctx.fillText("SPACE: chart order",L+6,T+14);
    if(!replayDraft&&replayHoverPrice!=null)drawReplayPreviewLine(y);
  }
  pctx.restore();
}
function drawReplayPreviewLine(y){
  const {L,T,pw,ph}=priceScale;
  const p=replayHoverPrice, yy=y(p);
  if(yy<T-8||yy>T+ph+8)return;
  const leftText=replayClickText(p,0), rightText=replayClickText(p,2);
  const color="#4f46e5";
  pctx.save();
  pctx.strokeStyle=colorAlpha(color,0.95); pctx.lineWidth=1.8; pctx.setLineDash([8,5]);
  pctx.beginPath(); pctx.moveTo(L,yy); pctx.lineTo(L+pw,yy); pctx.stroke(); pctx.setLineDash([]);
  pctx.font="12px Segoe UI,Arial";
  const tw=Math.max(pctx.measureText(leftText).width,pctx.measureText(rightText).width);
  const x=Math.max(L+8,Math.min(L+pw-tw-14,(hover&&hover.canvas==="price"?hover.x+14:L+8)));
  const boxY=Math.max(T+4,Math.min(T+ph-44,yy-42));
  pctx.fillStyle="rgba(255,255,255,0.96)"; pctx.fillRect(x-6,boxY,tw+12,40);
  pctx.strokeStyle=colorAlpha(color,0.9); pctx.strokeRect(x-6,boxY,tw+12,40);
  pctx.textBaseline="middle";
  pctx.fillStyle=COLORS.buy; pctx.fillText(leftText,x,boxY+12);
  pctx.fillStyle=COLORS.sell; pctx.fillText(rightText,x,boxY+29);
  pctx.restore();
}
function replayLineAt(ev){
  if(!priceScale)return null;
  const r=price.getBoundingClientRect(), yMouse=ev.clientY-r.top;
  const {T,ph,lo,hi}=priceScale, y=p=>T+(hi-p)/(hi-lo)*ph;
  return replayLines().filter(l=>l.drag).find(l=>Math.abs(y(l.price)-yMouse)<=7)||null;
}
function replayPriceFromCanvasY(y){
  const {T,ph,lo,hi}=priceScale;
  return snapPrice(hi-(Math.max(T,Math.min(T+ph,y))-T)/Math.max(1,ph)*(hi-lo));
}
function replayPriceFromEvent(ev){
  const r=price.getBoundingClientRect(), y=ev.clientY-r.top;
  return replayPriceFromCanvasY(y);
}
function replayHoverMove(ev){
  if(!replaySpace||REPLAY.pending_order||REPLAY.open_position||chartMode==="heatmap"||!priceScale)return;
  replayHoverPrice=replayPriceFromEvent(ev);
  updateReplayMode();
  draw();
}
function replayHoverLeave(){
  if(replayHoverPrice==null)return;
  replayHoverPrice=null;
  updateReplayMode();
  draw();
}
function replayContextMenu(ev){
  if(!replaySpace&&!replayDrag)return;
  ev.preventDefault();
  ev.stopPropagation();
}
function replayMouseDown(ev){
  const line=replayLineAt(ev);
  if(line){
    replayDrag={mode:"line",line}; ev.preventDefault(); ev.stopPropagation(); return;
  }
  if(!replaySpace||(ev.button!==0&&ev.button!==2)||REPLAY.pending_order||REPLAY.open_position||chartMode==="heatmap")return;
  const entry=replayPriceFromEvent(ev), dir=replayDirForButton(ev.button);
  replayHoverPrice=entry;
  replayDraft=replayDraftFrom(entry,dir,entry);
  replayDrag={mode:"create",entry,dir};
  updateReplayMode(); draw();
  ev.preventDefault(); ev.stopPropagation();
}
function replayMouseMove(ev){
  if(!replayDrag)return;
  const p=replayPriceFromEvent(ev);
  if(replayDrag.mode==="create"){
    replayDraft=replayDraftFrom(replayDrag.entry,replayDrag.dir,p);
  } else {
    const line=replayDrag.line;
    if(REPLAY.pending_order){
      replayDraft={...REPLAY.pending_order,scope:"pending"};
      if(line.kind==="entry")replayDraft.entry_price=p;
      if(line.kind==="stop")replayDraft.stop=p;
      if(line.kind==="target")replayDraft.target=p;
      replayDraft.order_type=replayOrderType(replayDraft.entry_price,replayDraft.dir);
    } else if(REPLAY.open_position){
      replayDraft={scope:"open",dir:REPLAY.open_position.dir,order_type:REPLAY.open_position.source_order||"market",qty:REPLAY.open_position.qty,entry_price:REPLAY.open_position.entry_px,stop:REPLAY.open_position.stop,target:REPLAY.open_position.target};
      if(line.kind==="stop")replayDraft.stop=p;
      if(line.kind==="target")replayDraft.target=p;
    }
  }
  draw(); ev.preventDefault(); ev.stopPropagation();
}
async function replayMouseUp(ev){
  if(!replayDrag)return;
  const dragNow=replayDrag;
  replayDrag=null; ev.preventDefault(); ev.stopPropagation();
  if(dragNow.mode==="create"){
    const body=replayDraft;
    replayDraft=null;
    await placeReplayOrder(body);
  } else {
    const line=dragNow.line, d=replayDraft;
    replayDraft=null;
    if((line.scope==="pending"||line.scope==="open")&&d){
      await replayAction("/api/replay/update",{order_type:line.scope==="pending"?d.order_type:null,entry_price:line.scope==="pending"?d.entry_price:null,stop:d.stop,target:d.target});
    }
  }
  updateReplayMode(); draw();
}
"##;
