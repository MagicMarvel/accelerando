//! Interactive manual bar-by-bar replay: a paper-trading session laid on top of the studio
//! chart page. A host app hands [`ReplayManager`] a prepared footprint series; players step
//! forward one (or a few) bars at a time, place market/limit/breakout orders with a stop/target
//! bracket, and each session is persisted to a JSON file so it can be reviewed afterwards.
//!
//! Wired in via [`crate::Studio::replay`].

use accelerando_core::result::EquityPoint;
use accelerando_core::{
    ExperimentRunSummary, Footprint, Metrics, PreparedBacktestData, Trade, TradeReason,
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
/// Bars of history a fresh session starts with, so the first decision has context.
const DEFAULT_WARMUP_BARS: usize = 200;
/// Cap on stored undo snapshots (~150 bytes each). Oldest are dropped past this.
const MAX_UNDO: usize = 20_000;
const QTY_EPS: f64 = 1e-9;

/// Per-session trading costs and account settings. Costs are editable mid-session (they apply
/// to future fills only); starting equity is fixed at session creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ReplayConfig {
    starting_equity: f64,
    commission_per_contract: f64,
    slippage_ticks: f64,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            starting_equity: STARTING_EQUITY,
            commission_per_contract: COMMISSION_PER_CONTRACT,
            slippage_ticks: SLIPPAGE_TICKS,
        }
    }
}

impl ReplayConfig {
    fn commission(&self, qty: f64) -> f64 {
        self.commission_per_contract * qty
    }
}

fn slipped(px: f64, dir: i32, slippage_ticks: f64, tick_size: f64) -> f64 {
    px + dir as f64 * slippage_ticks * tick_size
}

/// Snapshot taken before every mutation (one per bar during multi-bar skips), so the session
/// can be rewound step by step. The append-only vecs are restored by truncation; the last
/// equity point is stored explicitly because re-marks replace it in place.
#[derive(Clone, Debug)]
struct ReplayUndo {
    cursor: usize,
    realized_pnl: f64,
    peak_equity: f64,
    pending_order: Option<PendingOrder>,
    open_position: Option<OpenPosition>,
    config: ReplayConfig,
    trades_len: usize,
    events_len: usize,
    equity_len: usize,
    last_equity: Option<EquityPoint>,
}

fn push_undo(session: &mut ReplaySession) {
    session.undo.push(ReplayUndo {
        cursor: session.cursor,
        realized_pnl: session.realized_pnl,
        peak_equity: session.peak_equity,
        pending_order: session.pending_order.clone(),
        open_position: session.open_position.clone(),
        config: session.config.clone(),
        trades_len: session.trades.len(),
        events_len: session.events.len(),
        equity_len: session.equity.len(),
        last_equity: session.equity.last().copied(),
    });
    if session.undo.len() > MAX_UNDO {
        let excess = session.undo.len() - MAX_UNDO;
        session.undo.drain(..excess);
    }
}

fn restore_undo(session: &mut ReplaySession, u: ReplayUndo) {
    session.cursor = u.cursor;
    session.realized_pnl = u.realized_pnl;
    session.peak_equity = u.peak_equity;
    session.pending_order = u.pending_order;
    session.open_position = u.open_position;
    session.config = u.config;
    session.trades.truncate(u.trades_len);
    session.events.truncate(u.events_len);
    session.equity.truncate(u.equity_len);
    if let (Some(point), Some(slot)) = (u.last_equity, session.equity.last_mut()) {
        *slot = point;
    }
}

/// Manual bar-replay engine. Construct one per server run and pass it to
/// [`crate::Studio::replay`].
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

    pub(crate) fn create_session(
        &self,
        req: NewSessionRequest,
    ) -> Result<ReplayPublicState, String> {
        let total = self.prepared.footprints.len();
        if total == 0 {
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
        // Default warmup caps at 20% of the data so short datasets still leave room to trade.
        let warmup = req
            .warmup_bars
            .unwrap_or_else(|| DEFAULT_WARMUP_BARS.min(total / 5))
            .min(total - 1);
        let cursor = if req.random_start.unwrap_or(false) {
            // Uniform-ish random start in [warmup, 90% of the data], leaving room to trade
            // forward. A time-seeded LCG is plenty for picking a practice spot.
            let hi = (total * 9 / 10).max(warmup + 1).min(total);
            let span = (hi - warmup).max(1);
            let seed = created_at_ms.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            warmup + (seed >> 16) as usize % span
        } else {
            req.start_bar.unwrap_or(warmup).min(total - 1)
        };
        let mut config = ReplayConfig::default();
        if let Some(v) = req.starting_equity {
            config.starting_equity = finite_positive(v, "starting_equity")?;
        }
        if let Some(v) = req.commission_per_contract {
            config.commission_per_contract = finite_non_negative(v, "commission_per_contract")?;
        }
        if let Some(v) = req.slippage_ticks {
            config.slippage_ticks = finite_non_negative(v, "slippage_ticks")?;
        }
        let peak = config.starting_equity;
        let mut session = ReplaySession {
            id: id.clone(),
            source: self.source.clone(),
            created_at_ms,
            updated_at_ms: created_at_ms,
            cursor,
            realized_pnl: 0.0,
            peak_equity: peak,
            data_fingerprint: self.prepared.footprints.first().map(|b| b.ts_last_ns),
            config,
            pending_order: None,
            open_position: None,
            trades: Vec::new(),
            equity: Vec::new(),
            events: Vec::new(),
            undo: Vec::new(),
        };
        self.mark_equity_at(&mut session, cursor)?;
        session.events.push(ReplayEvent::new(
            session.cursor,
            "created",
            serde_json::json!({ "source": session.source, "start_bar": cursor }),
        ));
        self.save_session(&session)?;
        self.sessions
            .lock()
            .map_err(|_| "replay session lock poisoned".to_string())?
            .insert(id, session.clone());
        Ok(self.public_state(&session))
    }

    /// If `id` is not in memory (e.g. after a server restart), try to restore it from the
    /// persisted record file. Undo history does not survive a restart. Sessions recorded
    /// against different data are refused so bar indices never silently point at the wrong
    /// bars.
    fn ensure_loaded(&self, id: &str) -> Result<(), String> {
        if !valid_session_id(id) {
            return Err("invalid replay session id".to_string());
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "replay session lock poisoned".to_string())?;
        if sessions.contains_key(id) {
            return Ok(());
        }
        let data = fs::read_to_string(self.record_path(id))
            .map_err(|_| format!("replay session not found: {id}"))?;
        let session: ReplaySession = serde_json::from_str(&data)
            .map_err(|e| format!("restore replay record {id}: {e}"))?;
        if !self.session_compatible(&session) {
            return Err(format!(
                "this session was recorded with different data (source: {}); start the server with that feed to resume it",
                session.source
            ));
        }
        sessions.insert(id.to_string(), session);
        Ok(())
    }

    /// Whether a recorded session lines up with the currently loaded data series.
    fn session_compatible(&self, session: &ReplaySession) -> bool {
        let data_ok = match session.data_fingerprint {
            Some(fp) => self.prepared.footprints.first().map(|b| b.ts_last_ns) == Some(fp),
            None => session.source == self.source,
        };
        data_ok && session.cursor < self.prepared.footprints.len()
    }

    /// Summaries of every persisted session in the record dir, newest first, with a
    /// `compatible` flag telling the UI whether it can be resumed against the current data.
    pub(crate) fn list_sessions(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.record_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<ReplaySession>(&text) else {
                continue;
            };
            out.push(serde_json::json!({
                "id": session.id,
                "created_at_ms": session.created_at_ms,
                "updated_at_ms": session.updated_at_ms,
                "cursor": session.cursor,
                "total_bars": self.prepared.footprints.len(),
                "trades": session.trades.len(),
                "realized_pnl": session.realized_pnl,
                "position_open": session.open_position.is_some(),
                "pending_order": session.pending_order.is_some(),
                "source": session.source,
                "compatible": self.session_compatible(&session),
            }));
        }
        out.sort_by(|a, b| b["updated_at_ms"].as_u64().cmp(&a["updated_at_ms"].as_u64()));
        out
    }

    /// Remove a session from memory and delete its record file.
    pub(crate) fn delete_session(&self, id: &str) -> Result<(), String> {
        if !valid_session_id(id) {
            return Err("invalid replay session id".to_string());
        }
        self.sessions
            .lock()
            .map_err(|_| "replay session lock poisoned".to_string())?
            .remove(id);
        let path = self.record_path(id);
        if !path.exists() {
            return Err(format!("replay session not found: {id}"));
        }
        fs::remove_file(&path).map_err(|e| format!("delete replay record: {e}"))
    }

    /// Incremental session state: footprints from `since_fp` and equity from just before
    /// `since_eq` (the last known equity point is resent because re-marks replace it in
    /// place). The client keeps its prefix and appends. `since_*` past the current end (after
    /// a rewind) simply yields a shorter prefix, which the client truncates to. The liquidity
    /// heatmap is never included: the studio fetches heatmap windows lazily via /api/heatmap.
    pub(crate) fn state_value(
        &self,
        id: &str,
        since_fp: usize,
        since_eq: usize,
    ) -> Result<Value, String> {
        self.ensure_loaded(id)?;
        let session = self.session(id)?;
        let end = session
            .cursor
            .saturating_add(1)
            .min(self.prepared.footprints.len());
        let fp_from = since_fp.min(end);
        let eq_from = since_eq.min(session.equity.len()).saturating_sub(1);
        let metrics = Metrics::compute(
            session.config.starting_equity,
            &session.trades,
            &session.equity,
        );
        Ok(serde_json::json!({
            "metrics": metrics,
            "trades": session.trades,
            "equity": session.equity[eq_from..],
            "equity_from": eq_from,
            "footprints": self.prepared.footprints[fp_from..end],
            "footprints_from": fp_from,
            "liquidity_heatmap": { "snapshots": [] },
            "tick_size": self.prepared.tick_size,
            "multiplier": self.prepared.multiplier,
            "replay": self.public_state(&session),
        }))
    }

    pub(crate) fn place_order(
        &self,
        id: &str,
        req: OrderRequest,
    ) -> Result<ReplayPublicState, String> {
        self.ensure_loaded(id)?;
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            let dir = normalize_dir(req.dir)?;
            let order_type = OrderType::from_str(&req.order_type)?;
            let qty = finite_positive(req.qty, "qty")?;
            let current = self.current_footprint(session)?.clone();
            match order_type {
                OrderType::Market => {
                    let fill_px = slipped(
                        current.close,
                        dir,
                        session.config.slippage_ticks,
                        self.prepared.tick_size,
                    );
                    push_undo(session);
                    if let Err(e) = self.apply_fill(
                        session,
                        dir,
                        qty,
                        fill_px,
                        current.ts_last_ns,
                        req.stop,
                        req.target,
                        false,
                        order_type,
                    ) {
                        session.undo.pop();
                        return Err(e);
                    }
                    self.mark_equity_at(session, session.cursor)?;
                }
                OrderType::Limit | OrderType::Breakout => {
                    if session.pending_order.is_some() {
                        return Err(
                            "cancel the pending order before placing another".to_string()
                        );
                    }
                    let raw_entry = req.entry_price.ok_or_else(|| {
                        "entry_price is required for limit and breakout orders".to_string()
                    })?;
                    let (stop, target) = (req.stop, req.target);
                    validate_protection(dir, raw_entry, stop, target)?;
                    push_undo(session);
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

    /// Apply a fill against the current session state: open fresh, scale in (same direction),
    /// reduce / close (opposite direction, qty <= position), or reverse (opposite, qty >
    /// position). A bracket is required only when a new position (or reversed remainder) is
    /// opened; offsetting fills ignore it and keep the existing bracket. Returns Err without
    /// mutating the session.
    #[allow(clippy::too_many_arguments)]
    fn apply_fill(
        &self,
        session: &mut ReplaySession,
        dir: i32,
        qty: f64,
        fill_px: f64,
        ts_ns: i64,
        stop: Option<f64>,
        target: Option<f64>,
        entered_intrabar: bool,
        source_order: OrderType,
    ) -> Result<(), String> {
        match session.open_position.clone() {
            None => {
                validate_protection(dir, fill_px, stop, target)?;
                let position = self.open_position(
                    session,
                    session.cursor,
                    dir,
                    qty,
                    fill_px,
                    ts_ns,
                    stop,
                    target,
                    entered_intrabar,
                    source_order,
                );
                session.realized_pnl -= position.entry_fee;
                session.open_position = Some(position);
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "entry",
                    serde_json::json!({ "type": source_order, "dir": dir, "qty": qty, "entry_px": fill_px, "stop": stop, "target": target }),
                ));
            }
            Some(mut pos) if pos.dir == dir => {
                // Scale in: average the entry, keep the existing bracket.
                let fee = session.config.commission(qty);
                pos.entry_px = (pos.entry_px * pos.qty + fill_px * qty) / (pos.qty + qty);
                pos.qty += qty;
                pos.entry_fee += fee;
                session.realized_pnl -= fee;
                session.open_position = Some(pos);
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "scale_in",
                    serde_json::json!({ "dir": dir, "qty": qty, "fill_px": fill_px }),
                ));
            }
            Some(mut pos) if qty <= pos.qty + QTY_EPS => {
                // Reduce or fully close against the position.
                let closed = qty.min(pos.qty);
                self.close_qty(session, &mut pos, closed, fill_px, ts_ns, TradeReason::Signal);
                let fully = pos.qty <= QTY_EPS;
                session.open_position = if fully { None } else { Some(pos) };
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    if fully { "close" } else { "reduce" },
                    serde_json::json!({ "qty": closed, "exit_px": fill_px }),
                ));
            }
            Some(mut pos) => {
                // Reverse: close everything, open the remainder the other way.
                let remainder = qty - pos.qty;
                validate_protection(dir, fill_px, stop, target)?;
                let closed = pos.qty;
                self.close_qty(session, &mut pos, closed, fill_px, ts_ns, TradeReason::Signal);
                let position = self.open_position(
                    session,
                    session.cursor,
                    dir,
                    remainder,
                    fill_px,
                    ts_ns,
                    stop,
                    target,
                    entered_intrabar,
                    source_order,
                );
                session.realized_pnl -= position.entry_fee;
                session.open_position = Some(position);
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "reverse",
                    serde_json::json!({ "dir": dir, "closed_qty": closed, "new_qty": remainder, "fill_px": fill_px, "stop": stop, "target": target }),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn advance(
        &self,
        id: &str,
        count: usize,
        stop_on_event: bool,
    ) -> Result<ReplayPublicState, String> {
        let count = count.clamp(1, 20_000);
        self.advance_while(id, move |stepped, _| stepped < count, stop_on_event)
    }

    /// Jump forward until the current bar's close time reaches `to_ts_ns` (or data ends).
    /// A target at or before the current bar is an error (use /api/replay/back to rewind).
    pub(crate) fn advance_to_ts(
        &self,
        id: &str,
        to_ts_ns: i64,
        stop_on_event: bool,
    ) -> Result<ReplayPublicState, String> {
        self.ensure_loaded(id)?;
        {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            if let Some(fp) = self.prepared.footprints.get(session.cursor) {
                if fp.ts_last_ns >= to_ts_ns {
                    return Err(
                        "target time is not after the current bar (use Back to rewind)"
                            .to_string(),
                    );
                }
            }
        }
        self.advance_while(
            id,
            move |_, next_fp_ts_last_ns| next_fp_ts_last_ns <= to_ts_ns,
            stop_on_event,
        )
    }

    /// Step the session forward, consuming the next bar as long as
    /// `keep_going(bars_stepped_so_far, next bar's ts_last_ns)` returns true. With
    /// `stop_on_event`, a bar that produced any event (pending fill, exit, rejection) ends the
    /// run early so multi-bar skips pause where something happened. Every bar pushes an undo
    /// snapshot, so skips can be rewound bar by bar.
    fn advance_while(
        &self,
        id: &str,
        keep_going: impl Fn(usize, i64) -> bool,
        stop_on_event: bool,
    ) -> Result<ReplayPublicState, String> {
        self.ensure_loaded(id)?;
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            let mut stepped = 0usize;
            while let Some(next_fp) = self.prepared.footprints.get(session.cursor + 1) {
                if !keep_going(stepped, next_fp.ts_last_ns) {
                    break;
                }
                push_undo(session);
                let events_before = session.events.len();
                session.cursor += 1;
                self.process_current_bar(session)?;
                stepped += 1;
                if stop_on_event && session.events.len() > events_before {
                    break;
                }
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    /// Rewind up to `count` undo steps (one step = one bar of a skip, or one order action).
    pub(crate) fn back(&self, id: &str, count: usize) -> Result<ReplayPublicState, String> {
        self.ensure_loaded(id)?;
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            let count = count.clamp(1, 20_000);
            let mut undone = 0usize;
            while undone < count {
                match session.undo.pop() {
                    Some(u) => {
                        restore_undo(session, u);
                        undone += 1;
                    }
                    None => break,
                }
            }
            if undone == 0 {
                return Err("nothing to rewind (undo history is empty)".to_string());
            }
            session.updated_at_ms = now_ms();
            session.clone()
        };
        self.save_session(&session)?;
        Ok(self.public_state(&session))
    }

    /// Update trading costs mid-session; they apply to future fills only.
    pub(crate) fn update_config(
        &self,
        id: &str,
        req: ConfigRequest,
    ) -> Result<ReplayPublicState, String> {
        self.ensure_loaded(id)?;
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            let commission = match req.commission_per_contract {
                Some(v) => Some(finite_non_negative(v, "commission_per_contract")?),
                None => None,
            };
            let slippage = match req.slippage_ticks {
                Some(v) => Some(finite_non_negative(v, "slippage_ticks")?),
                None => None,
            };
            push_undo(session);
            if let Some(v) = commission {
                session.config.commission_per_contract = v;
            }
            if let Some(v) = slippage {
                session.config.slippage_ticks = v;
            }
            session.events.push(ReplayEvent::new(
                session.cursor,
                "config",
                serde_json::json!({
                    "commission_per_contract": session.config.commission_per_contract,
                    "slippage_ticks": session.config.slippage_ticks,
                }),
            ));
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
            // A pending order and an open position can coexist now; `scope` picks which one
            // the drag applies to (falling back to position-then-pending for old clients).
            let scope = req.scope.as_deref();
            let on_position = match scope {
                Some("open") => true,
                Some("pending") => false,
                _ => session.open_position.is_some(),
            };
            // Merge one side: explicit clear wins, then a new value, else keep the current.
            let merge = |clear: Option<bool>, new: Option<f64>, cur: Option<f64>| {
                if clear.unwrap_or(false) {
                    None
                } else {
                    new.or(cur)
                }
            };
            if on_position {
                let pos = session
                    .open_position
                    .as_ref()
                    .ok_or_else(|| "there is no open position to update".to_string())?;
                let stop = merge(req.clear_stop, req.stop, pos.stop);
                let target = merge(req.clear_target, req.target, pos.target);
                // Unlike order placement, a live bracket may cross the entry (move SL to
                // break-even / trail into profit); only stop-vs-target ordering must hold.
                validate_bracket_update(pos.dir, stop, target)?;
                push_undo(session);
                let pos = session.open_position.as_mut().expect("checked above");
                pos.stop = stop;
                pos.target = target;
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "update_position_bracket",
                    serde_json::json!({ "stop": stop, "target": target }),
                ));
            } else if let Some(order) = session.pending_order.clone() {
                let mut next = order;
                if let Some(entry) = req.entry_price {
                    next.entry_price = finite_value(entry, "entry_price")?;
                }
                if let Some(order_type) = req.order_type.as_deref() {
                    next.order_type = OrderType::from_str(order_type)?;
                }
                next.stop = merge(req.clear_stop, req.stop, next.stop);
                next.target = merge(req.clear_target, req.target, next.target);
                validate_protection(next.dir, next.entry_price, next.stop, next.target)?;
                push_undo(session);
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "update_pending_order",
                    serde_json::json!({ "type": next.order_type, "entry_price": next.entry_price, "stop": next.stop, "target": next.target }),
                ));
                session.pending_order = Some(next);
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
        self.ensure_loaded(id)?;
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            if session.pending_order.is_some() {
                push_undo(session);
                session.pending_order = None;
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
        self.ensure_loaded(id)?;
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "replay session lock poisoned".to_string())?;
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| format!("replay session not found: {id}"))?;
            let fp = self.current_footprint(session)?.clone();
            if session.open_position.is_some() {
                push_undo(session);
                let mut pos = session.open_position.take().expect("checked above");
                let exit_px = slipped(
                    fp.close,
                    -pos.dir,
                    session.config.slippage_ticks,
                    self.prepared.tick_size,
                );
                let qty = pos.qty;
                self.close_qty(session, &mut pos, qty, exit_px, fp.ts_last_ns, TradeReason::Signal);
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
        if self.ensure_loaded(id).is_err() {
            return false;
        }
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
        if let Some(order) = session.pending_order.clone() {
            if let Some((raw_fill, entered_intrabar)) = fill_pending_order(&order, &fp) {
                let fill_px = if order.order_type == OrderType::Breakout {
                    slipped(
                        raw_fill,
                        order.dir,
                        session.config.slippage_ticks,
                        self.prepared.tick_size,
                    )
                } else {
                    raw_fill
                };
                session.pending_order = None;
                session.events.push(ReplayEvent::new(
                    session.cursor,
                    "pending_fill",
                    serde_json::json!({ "entry_px": fill_px, "type": order.order_type }),
                ));
                // A fill that fails validation (e.g. breakout slippage crossed its own stop)
                // cancels the order with an event instead of aborting the whole skip.
                if let Err(e) = self.apply_fill(
                    session,
                    order.dir,
                    order.qty,
                    fill_px,
                    fp.ts_first_ns,
                    order.stop,
                    order.target,
                    entered_intrabar,
                    order.order_type,
                ) {
                    session.events.push(ReplayEvent::new(
                        session.cursor,
                        "pending_rejected",
                        serde_json::json!({ "error": e }),
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
                let mut pos = pos;
                let qty = pos.qty;
                self.close_qty(session, &mut pos, qty, exit_px, fp.ts_last_ns, reason);
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
        session: &ReplaySession,
        entry_bar: usize,
        dir: i32,
        qty: f64,
        entry_px: f64,
        entry_ts_ns: i64,
        stop: Option<f64>,
        target: Option<f64>,
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
            entry_fee: session.config.commission(qty),
        }
    }

    /// Close `qty` of the position at `exit_px`, realizing PnL and recording one trade with a
    /// proportional share of the entry fee. The caller decides what to do with the remainder.
    fn close_qty(
        &self,
        session: &mut ReplaySession,
        pos: &mut OpenPosition,
        qty: f64,
        exit_px: f64,
        exit_ts_ns: i64,
        reason: TradeReason,
    ) {
        let qty = qty.min(pos.qty);
        let frac = if pos.qty > 0.0 { qty / pos.qty } else { 1.0 };
        let gross = (exit_px - pos.entry_px) * pos.dir as f64 * qty * self.prepared.multiplier;
        let exit_fee = session.config.commission(qty);
        let entry_fee_share = pos.entry_fee * frac;
        session.realized_pnl += gross - exit_fee;
        session.trades.push(Trade {
            entry_ts_ns: pos.entry_ts_ns,
            exit_ts_ns,
            dir: pos.dir,
            qty,
            entry_px: pos.entry_px,
            exit_px,
            stop: pos.stop,
            target: pos.target,
            max_adverse_excursion: pos.max_adverse_excursion * frac,
            max_adverse_ticks: pos.max_adverse_ticks,
            pnl: gross - entry_fee_share - exit_fee,
            reason,
        });
        pos.entry_fee -= entry_fee_share;
        pos.qty -= qty;
    }

    fn mark_equity_at(&self, session: &mut ReplaySession, cursor: usize) -> Result<(), String> {
        let fp = self
            .prepared
            .footprints
            .get(cursor)
            .ok_or_else(|| "cursor is outside the prepared data".to_string())?;
        let mut equity = session.config.starting_equity + session.realized_pnl;
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
            .fold(session.config.starting_equity, |peak, point| {
                peak.max(point.equity)
            })
            .max(equity);
        session.equity.push(EquityPoint {
            ts_ns: fp.ts_last_ns,
            equity,
            drawdown: equity - session.peak_equity,
        });
        Ok(())
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
            config: session.config.clone(),
            can_back: !session.undo.is_empty(),
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
    /// First bar's close timestamp when the session was created. Used to detect that the
    /// server was started with different data before resuming. Old records lack it and
    /// fall back to comparing `source`.
    #[serde(default)]
    data_fingerprint: Option<i64>,
    #[serde(default)]
    config: ReplayConfig,
    pending_order: Option<PendingOrder>,
    open_position: Option<OpenPosition>,
    trades: Vec<Trade>,
    equity: Vec<EquityPoint>,
    events: Vec<ReplayEvent>,
    /// In-memory only: not persisted, so rewind history is lost on server restart.
    #[serde(skip)]
    undo: Vec<ReplayUndo>,
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
    /// Optional protection; either side can be absent and added/removed later.
    stop: Option<f64>,
    target: Option<f64>,
    created_bar: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpenPosition {
    dir: i32,
    qty: f64,
    entry_px: f64,
    entry_ts_ns: i64,
    entry_bar: usize,
    /// Optional protection; either side can be absent and added/removed later.
    stop: Option<f64>,
    target: Option<f64>,
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
    config: ReplayConfig,
    can_back: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct NewSessionRequest {
    /// Explicit starting bar; defaults to `warmup_bars` so the chart opens with context.
    start_bar: Option<usize>,
    /// Pick a random starting bar (for unbiased practice).
    random_start: Option<bool>,
    warmup_bars: Option<usize>,
    starting_equity: Option<f64>,
    commission_per_contract: Option<f64>,
    slippage_ticks: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConfigRequest {
    commission_per_contract: Option<f64>,
    slippage_ticks: Option<f64>,
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
    /// Jump forward until the current bar's close time reaches this UTC millisecond timestamp.
    pub(crate) to_ts_ms: Option<i64>,
    /// Pause a multi-bar skip on the first bar that fills, exits, or rejects an order.
    pub(crate) stop_on_event: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BackRequest {
    pub(crate) count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateOrderRequest {
    /// "open" or "pending": which side to update when both exist.
    scope: Option<String>,
    order_type: Option<String>,
    entry_price: Option<f64>,
    stop: Option<f64>,
    target: Option<f64>,
    /// Remove the corresponding protection side entirely.
    clear_stop: Option<bool>,
    clear_target: Option<bool>,
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
        let worst = match pos.stop {
            Some(stop) if fp.low <= stop => stop,
            _ => fp.low,
        };
        (pos.entry_px - worst).max(0.0)
    } else {
        let worst = match pos.stop {
            Some(stop) if fp.high >= stop => stop,
            _ => fp.high,
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
        if let Some(stop) = pos.stop {
            if fp.low <= stop {
                return Some((stop, TradeReason::StopLoss));
            }
        }
        if !skip_target {
            if let Some(target) = pos.target {
                if fp.high >= target {
                    return Some((target, TradeReason::TakeProfit));
                }
            }
        }
        None
    } else {
        if let Some(stop) = pos.stop {
            if fp.high >= stop {
                return Some((stop, TradeReason::StopLoss));
            }
        }
        if !skip_target {
            if let Some(target) = pos.target {
                if fp.low <= target {
                    return Some((target, TradeReason::TakeProfit));
                }
            }
        }
        None
    }
}

/// Entry-time validation: each protection side is optional, but if present it must sit on
/// the correct side of the entry price.
fn validate_protection(
    dir: i32,
    entry: f64,
    stop: Option<f64>,
    target: Option<f64>,
) -> Result<(), String> {
    finite_value(entry, "entry_price")?;
    if let Some(stop) = stop {
        finite_value(stop, "stop")?;
        if dir > 0 && stop >= entry {
            return Err("long orders require the stop below entry".to_string());
        }
        if dir < 0 && stop <= entry {
            return Err("short orders require the stop above entry".to_string());
        }
    }
    if let Some(target) = target {
        finite_value(target, "target")?;
        if dir > 0 && target <= entry {
            return Err("long orders require the target above entry".to_string());
        }
        if dir < 0 && target >= entry {
            return Err("short orders require the target below entry".to_string());
        }
    }
    Ok(())
}

/// Bracket update on a live position: the bracket may cross the entry (break-even stops,
/// trailing into profit), but when both sides exist the stop must stay on the losing side
/// of the target.
fn validate_bracket_update(dir: i32, stop: Option<f64>, target: Option<f64>) -> Result<(), String> {
    if let Some(stop) = stop {
        finite_value(stop, "stop")?;
    }
    if let Some(target) = target {
        finite_value(target, "target")?;
    }
    if let (Some(stop), Some(target)) = (stop, target) {
        if dir > 0 && !(stop < target) {
            return Err("long position: stop must stay below target".to_string());
        }
        if dir < 0 && !(stop > target) {
            return Err("short position: stop must stay above target".to_string());
        }
    }
    Ok(())
}

/// Session ids are `replay_<ms>_<n>`; anything else (especially path separators) is rejected
/// before being joined into a file path.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn normalize_dir(dir: i32) -> Result<i32, String> {
    match dir {
        1 => Ok(1),
        -1 => Ok(-1),
        _ => Err("dir must be 1 for long or -1 for short".to_string()),
    }
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

fn finite_non_negative(value: f64, name: &str) -> Result<f64, String> {
    let value = finite_value(value, name)?;
    if value >= 0.0 {
        Ok(value)
    } else {
        Err(format!("{name} must be >= 0"))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
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
.replay-panel{display:flex;gap:8px;flex-wrap:wrap;align-items:center;padding:7px 14px;background:#e7edf5;border-bottom:1px solid #d3dce8}
.replay-panel label{display:inline-flex;gap:5px;align-items:center;font-size:12px;color:#374151}
.replay-panel input,.replay-panel select{height:28px;border:1px solid #cbd5e1;border-radius:6px;padding:3px 6px;font-size:12px;background:#fff}
.replay-panel input{width:72px}.replay-panel button{height:28px;padding:0 9px;border-radius:6px;font-size:12px}
.replay-panel .primary{background:#2563eb;border-color:#2563eb;color:#fff;font-weight:800}
.replay-panel .danger{color:#b45309}.replay-panel .state{font-size:12px;color:#475569;font-weight:700}
.replay-panel .err{font-size:12px;color:#b45309;min-width:160px}
.replay-panel .mode{font-size:12px;color:#0f172a;background:#eef2ff;border:1px solid #c7d2fe;border-radius:6px;padding:5px 8px;font-weight:800}
.replay-panel .mode.armed{background:#16a34a;color:#fff;border-color:#16a34a}
.replay-panel .hint{font-size:12px;color:#64748b}
.replay-panel .sep{width:1px;align-self:stretch;background:#e2e8f0;margin:0 2px}
.replay-panel .buy{background:#2962ff;border-color:#2962ff;color:#fff;font-weight:800}
.replay-panel .buy:hover{background:#1e4fd6}
.replay-panel .sell{background:#f23645;border-color:#f23645;color:#fff;font-weight:800}
.replay-panel .sell:hover{background:#d92835}
.replay-panel button:disabled{opacity:.45;cursor:not-allowed}
.charts.replay-armed #price{cursor:crosshair}
.replay-toast{position:absolute;left:50%;top:14px;transform:translateX(-50%);z-index:9;max-width:70%;padding:8px 14px;border-radius:8px;font-size:12.5px;font-weight:700;box-shadow:0 6px 20px rgba(15,23,42,.28);opacity:0;pointer-events:none;transition:opacity .15s}
.replay-toast.visible{opacity:1}
.replay-toast.err{background:#7f1d1d;color:#fff}
.replay-toast.ok{background:#0f172a;color:#fff}
.replay-trades{display:none;position:absolute;right:12px;top:10px;z-index:7;width:380px;max-height:62%;overflow:auto;background:#fff;border:1px solid #cbd5e1;border-radius:10px;box-shadow:0 10px 30px rgba(15,23,42,.22);font-size:12px}
.replay-trades.visible{display:block}
.replay-trades .tr-head{position:sticky;top:0;display:flex;align-items:center;gap:8px;padding:7px 10px;background:#fff;border-bottom:1px solid #eef2f7;font-weight:800;color:#334155}
.replay-trades .tr-head button{margin-left:auto;height:22px;width:22px;padding:0;border-radius:5px;font-size:12px;line-height:1}
.replay-trades .tr-row{display:flex;align-items:center;gap:8px;padding:5px 10px;cursor:pointer;border-top:1px solid #f1f5f9;white-space:nowrap}
.replay-trades .tr-row:hover{background:#eff6ff}
.replay-trades .tr-row .d{font-weight:800;width:38px}
.replay-trades .tr-row .d.long{color:#2962ff}.replay-trades .tr-row .d.short{color:#f23645}
.replay-trades .tr-row .px{color:#475569}
.replay-trades .tr-row .pnl{margin-left:auto;font-weight:800}
.replay-trades .tr-row .pnl.pos{color:#089981}.replay-trades .tr-row .pnl.neg{color:#f23645}
.replay-trades .tr-row .rsn{color:#94a3b8;font-size:11px}
.replay-trades .tr-empty{padding:14px;color:#94a3b8;text-align:center}
"#;

const REPLAY_JS: &str = r##"
let REPLAY={};
let replayDrag=null;
let replayDraft=null;
let replaySpace=false;
let replayHoverPrice=null;
// TradingView-style order/position widgets: chip groups drawn at the right edge of each
// entry/SL/TP line. Rebuilt on every draw; hit-tested for dragging and the close (x) button.
let replayWidgets=[];
let replayHoverKind=null;
let replayHoverAction=null;
const REPLAY_TP_COLOR="#089981";
const REPLAY_SL_COLOR="#f23645";
const REPLAY_LONG_COLOR="#2962ff";
const REPLAY_SHORT_COLOR="#f23645";
let replayBusy=false;
let replayToastTimer=null;
function replayTick(){return (DATA&&DATA.tick_size)||0.25;}
function snapPrice(v){const t=replayTick();return Math.round(Number(v)/t)*t;}
function num(id){const v=parseFloat($(id).value);return Number.isFinite(v)?v:null;}
function setNum(id,v){$(id).value=(v==null||!Number.isFinite(v))?"":Number(v).toFixed(2);}
function replayQty(){return Math.max(0.01,num("replayQty")||1);}
function replayRiskTicks(){return Math.max(1,Math.round(num("replayRisk")||8));}
function replayRMultiple(){return Math.max(0.25,num("replayR")||2);}
function replaySlipTicks(){const c=REPLAY.config;return c&&Number.isFinite(c.slippage_ticks)?c.slippage_ticks:1;}
function replayCurrentPrice(){return REPLAY.current_close??currentVisiblePrice();}
// Transient toast over the chart: drag rejections and other errors are invisible in the
// toolbar while the eyes are on the chart, so surface them where the user is looking.
function replayToast(msg,ok=false){
  let el=$("replayToast");
  if(!el){
    el=document.createElement("div"); el.id="replayToast";
    document.querySelector(".charts").appendChild(el);
  }
  el.textContent=msg;
  el.className="replay-toast "+(ok?"ok":"err")+" visible";
  clearTimeout(replayToastTimer);
  replayToastTimer=setTimeout(()=>el.classList.remove("visible"),2800);
}
// Incremental state loader (replaces the studio's full-result loadResult): only bars/equity
// beyond what the client already has travel over the wire; a rewind simply returns a shorter
// prefix, which the slice() below truncates to.
async function loadResult(){
  const url="/api/replay/state?id="+encodeURIComponent(globalThis.RUN_ID||"")
    +"&since_fp="+fps.length+"&since_eq="+eq.length;
  const data=await fetchJson(url);
  if(data&&data.error)throw new Error(data.error);
  data.footprints=fps.slice(0,data.footprints_from||0).concat(data.footprints||[]);
  data.equity=eq.slice(0,data.equity_from||0).concat(data.equity||[]);
  renderResult(data);
  await loadAnnotations();
}
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
  const risk=replayRiskTicks()*replayTick();
  return {stop:snapPrice(entry-dir*risk),target:snapPrice(entry+dir*risk*replayRMultiple())};
}
function replayDraftFrom(entry,dir,at){
  entry=snapPrice(entry); at=snapPrice(at);
  const type=replayOrderType(entry,dir);
  const t=replayTick();
  let stop=null,target=null;
  const signed=(at-entry)*dir;
  if(Math.abs(signed)<t){
    // Plain click: a bare entry line. TP/SL are added later by dragging the chips off
    // the order tag (or by dragging during placement).
  } else if(signed<0){
    stop=at;
    const risk=Math.max(t,Math.abs(entry-stop));
    target=snapPrice(entry+dir*risk*replayRMultiple());
  } else {
    target=at;
    const reward=Math.max(t,Math.abs(target-entry));
    stop=snapPrice(entry-dir*Math.max(t,reward/replayRMultiple()));
  }
  return {scope:"draft",dir,order_type:type,qty:replayQty(),entry_price:entry,stop,target};
}
function replayStoredNum(key,fallback){
  try{ const v=parseFloat(localStorage.getItem(key)); return Number.isFinite(v)?v:fallback; }catch(_){ return fallback; }
}
function replayStoreNum(key,v){ try{ localStorage.setItem(key,String(v)); }catch(_){} }
function replayStopOnEvent(){ const el=$("replayStopEvt"); return el?el.checked:true; }
async function replayStep(n){
  if(await replayAction("/api/replay/step",{count:n,stop_on_event:replayStopOnEvent()}))replayAfterStep();
}
async function replayBack(n){
  if(await replayAction("/api/replay/back",{count:n}))replayAfterStep();
}
// After any cursor move: recenter the view on the latest bar and, if the user has a manual
// y-zoom, pan the price window minimally so the current price never walks off-screen.
function replayAfterStep(){
  jumpToLatestCentered();
  if(REPLAY.current_close!=null)panPriceViewToReveal(REPLAY.current_close);
}
// Market orders enter bare (one line); TP/SL are added afterwards by dragging the chips
// off the position tag.
async function replayMarket(dir){
  await replayAction("/api/replay/order",{dir,order_type:"market",qty:replayQty()});
}
function installReplayUi(){
  const content=document.querySelector(".content");
  const panel=document.createElement("div");
  panel.className="replay-panel";
  panel.innerHTML=`<button id="replayBackBtn" title="Rewind one step: one bar of a skip or one order action (←)">← Back</button>
    <button id="replayStep" class="primary" title="Next bar (→)">Next K</button><button id="replayStep5" title="+5 bars (Shift+→)">+5</button>
    <label>Skip <input id="replaySkipN" type="number" min="1" max="20000" step="1" value="20" style="width:64px"></label><button id="replaySkip" title="Skip forward this many bars">Skip</button>
    <label>Go to <input id="replayGoTime" type="datetime-local" step="60" style="width:168px"></label><button id="replayGoto" title="Jump forward to this time">Go</button>
    <label title="Pause a skip on the first bar where an order fills or the position exits"><input type="checkbox" id="replayStopEvt" checked> stop on fill</label>
    <span class="sep"></span>
    <button id="replayBuy" class="buy" title="Buy at market (scales in / reduces / reverses against an open position)">Buy Mkt</button>
    <button id="replaySell" class="sell" title="Sell at market (scales in / reduces / reverses against an open position)">Sell Mkt</button>
    <label>Qty <input id="replayQty" type="number" min="0.01" step="0.01" value="1"></label>
    <label title="Default stop distance for new orders">Risk <input id="replayRisk" type="number" min="1" step="1" value="8" style="width:52px">t</label>
    <label title="Default reward:risk multiple for new orders">R <input id="replayR" type="number" min="0.25" step="0.25" value="2" style="width:48px"></label>
    <label title="Commission per contract per side">Fee $<input id="replayFee" type="number" min="0" step="0.25" style="width:56px"></label>
    <label title="Slippage in ticks applied to market/breakout fills">Slip <input id="replaySlip" type="number" min="0" step="0.5" style="width:48px">t</label>
    <span class="sep"></span>
    <span id="replayMode" class="mode">Hold Space</span>
    <button id="replayCancel">Cancel order</button><button id="replayFlat" class="danger">Flat</button>
    <button id="replayTradesBtn" title="Show the closed-trade list">Trades</button>
    <span id="replayState" class="state"></span><span id="replayHint" class="hint"></span><span id="replayErr" class="err"></span>`;
  content.insertBefore(panel, document.querySelector(".chart-area")||document.querySelector(".charts"));
  const oldRender=renderResult;
  renderResult=function(data){oldRender(data);REPLAY=data.replay||{};syncReplayUi();};
  const oldDraw=drawPrice;
  drawPrice=function(){oldDraw();drawReplayOverlay();};
  $("modeHeatmap").style.display="none";
  const heat=$("heatMetric"); if(heat&&heat.parentElement)heat.parentElement.style.display="none";
  $("replayRisk").value=replayStoredNum("accelerando.replayRiskTicks",8);
  $("replayR").value=replayStoredNum("accelerando.replayRMultiple",2);
  $("replayRisk").addEventListener("change",()=>replayStoreNum("accelerando.replayRiskTicks",replayRiskTicks()));
  $("replayR").addEventListener("change",()=>replayStoreNum("accelerando.replayRMultiple",replayRMultiple()));
  $("replayBackBtn").onclick=()=>replayBack(1);
  $("replayStep").onclick=()=>replayStep(1);
  $("replayStep5").onclick=()=>replayStep(5);
  $("replaySkip").onclick=()=>{
    const n=Math.max(1,Math.min(20000,Math.round(parseFloat($("replaySkipN").value)||0)));
    if(!n){replayToast("enter how many bars to skip");return;}
    replayStep(n);
  };
  $("replayGoto").onclick=async()=>{
    const v=$("replayGoTime").value;
    if(!v){replayToast("pick a target time first");return;}
    const ts=new Date(v).getTime();
    if(!Number.isFinite(ts)){replayToast("invalid target time");return;}
    if(await replayAction("/api/replay/step",{to_ts_ms:ts,stop_on_event:replayStopOnEvent()}))replayAfterStep();
  };
  $("replayBuy").onclick=()=>replayMarket(1);
  $("replaySell").onclick=()=>replayMarket(-1);
  $("replayCancel").onclick=async()=>{ if(await replayAction("/api/replay/cancel",{}))replayToast("Order cancelled — Back (←) undoes",true); };
  $("replayFlat").onclick=async()=>{ if(await replayAction("/api/replay/flatten",{}))replayToast("Position closed — Back (←) undoes",true); };
  $("replayTradesBtn").onclick=()=>replayToggleTrades();
  for(const id of ["replayFee","replaySlip"]){
    $(id).addEventListener("change",async()=>{
      const body={commission_per_contract:num("replayFee"),slippage_ticks:num("replaySlip")};
      await replayAction("/api/replay/config",body);
    });
  }
  for(const id of ["replayQty"]){
    $(id).addEventListener("input",()=>{replayDraft=null;syncReplayUi();draw();});
    $(id).addEventListener("change",()=>{replayDraft=null;syncReplayUi();draw();});
  }
  price.addEventListener("mousedown",replayMouseDown,true);
  price.addEventListener("mousemove",replayHoverMove,true);
  price.addEventListener("mousemove",replayCursorMove);
  price.addEventListener("mouseleave",replayHoverLeave,true);
  price.addEventListener("contextmenu",replayContextMenu,true);
  window.addEventListener("mousemove",replayMouseMove,true);
  window.addEventListener("mouseup",replayMouseUp,true);
  window.addEventListener("keydown",replayKeyDown,true);
  window.addEventListener("keyup",replayKeyUp,true);
  window.addEventListener("blur",()=>setReplaySpace(false));
  updateReplayMode();
}
function replayToggleTrades(force){
  const el=replayTradesPanel();
  const on=force!==undefined?force:!el.classList.contains("visible");
  el.classList.toggle("visible",on);
  if(on)replayRenderTrades();
}
function replayTradesPanel(){
  let el=$("replayTrades");
  if(!el){
    el=document.createElement("div"); el.id="replayTrades"; el.className="replay-trades";
    document.querySelector(".charts").appendChild(el);
  }
  return el;
}
function replayRenderTrades(){
  const el=replayTradesPanel();
  if(!el.classList.contains("visible"))return;
  const list=sortedTrades();
  const rows=list.map((t,i)=>{
    const d=new Date(Number(t.entry_ts_ns)/1e6);
    const pad=n=>String(n).padStart(2,"0");
    const when=`${pad(d.getMonth()+1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
    return `<div class="tr-row" data-i="${i}">
      <span>${i+1}</span><span class="d ${t.dir>0?"long":"short"}">${t.dir>0?"LONG":"SHORT"}</span>
      <span>${when}</span><span class="px">${fmt(t.qty,2)} @ ${fmt(t.entry_px,2)} → ${fmt(t.exit_px,2)}</span>
      <span class="rsn">${esc(String(t.reason??""))}</span>
      <span class="pnl ${t.pnl>=0?"pos":"neg"}">$${fmt(t.pnl,2)}</span>
    </div>`;
  }).join("");
  el.innerHTML=`<div class="tr-head">Trades (${list.length})<button title="Close" onclick="replayToggleTrades(false)">✕</button></div>`
    +(rows||`<div class="tr-empty">No closed trades yet</div>`);
  for(const row of el.querySelectorAll(".tr-row")){
    row.onclick=()=>{
      const t=sortedTrades()[Number(row.dataset.i)];
      if(!t||!Number.isFinite(t._ei))return;
      start=clamp(Math.round(t._ei-barsPerView/2),0,maxStart());
      autoYZoom("price"); draw();
    };
  }
}
function syncReplayUi(){
  const st=$("replayState");
  st.textContent=REPLAY.id?`bar ${Math.min(REPLAY.cursor+1,REPLAY.total_bars)} / ${REPLAY.total_bars}`:"";
  st.title=REPLAY.record_path?`session saved to ${REPLAY.record_path}`:"";
  const goEl=$("replayGoTime");
  if(goEl&&document.activeElement!==goEl&&fps.length){
    const d=new Date(Number(fps[fps.length-1].ts_last_ns)/1e6), pad=n=>String(n).padStart(2,"0");
    goEl.value=`${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }
  const cfg=REPLAY.config||{};
  const fee=$("replayFee"), slip=$("replaySlip");
  if(fee&&document.activeElement!==fee&&Number.isFinite(cfg.commission_per_contract))fee.value=cfg.commission_per_contract;
  if(slip&&document.activeElement!==slip&&Number.isFinite(cfg.slippage_ticks))slip.value=cfg.slippage_ticks;
  $("replayBackBtn").disabled=!REPLAY.can_back;
  $("replayStep").disabled=!!REPLAY.done;
  $("replayCancel").disabled=!REPLAY.pending_order;
  $("replayFlat").disabled=!REPLAY.open_position;
  const tb=$("replayTradesBtn");
  if(tb)tb.textContent=`Trades (${REPLAY.trades||0})`;
  replayRenderTrades();
  if(REPLAY.done&&!replayDoneToasted){replayDoneToasted=true;replayToast("End of data reached",true);}
  if(!REPLAY.done)replayDoneToasted=false;
  updateReplayMode();
}
let replayDoneToasted=false;
function replayKeyEditable(){
  const tag=(document.activeElement&&document.activeElement.tagName||"").toLowerCase();
  return tag==="input"||tag==="textarea"||tag==="select";
}
function replayKeyDown(ev){
  if(replayKeyEditable())return;
  if(ev.code==="Space"){ setReplaySpace(true); ev.preventDefault(); return; }
  if(ev.code==="ArrowRight"){ replayStep(ev.shiftKey?5:1); ev.preventDefault(); return; }
  if(ev.code==="ArrowLeft"){ replayBack(ev.shiftKey?5:1); ev.preventDefault(); return; }
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
  const blocked=REPLAY.pending_order?"order pending":"";
  mode.textContent=replaySpace&&!blocked?"Chart order":replaySpace?blocked:"Hold Space";
  if(REPLAY.pending_order)hint.textContent="Drag TP/SL chips off the order tag to add protection; drag lines to move; × cancels.";
  else if(replaySpace&&replayHoverPrice!=null)hint.textContent=`${replayClickText(replayHoverPrice,0)} | ${replayClickText(replayHoverPrice,2)}. Click = bare entry, drag = draw SL/TP.`;
  else if(replaySpace)hint.textContent="Move over the chart. Left click buys, right click sells; price decides LMT or STOP.";
  else if(REPLAY.open_position)hint.textContent="Drag TP/SL chips off the position tag to add protection; × closes. → next bar, ← rewind.";
  else hint.textContent="→ next bar, ← rewind. Hold Space + click for chart orders, or use Buy/Sell Mkt.";
}
function replayOrderError(body){
  if(!body)return "no order";
  const entry=body.entry_price, stop=body.stop, target=body.target, dir=body.dir;
  if(entry==null)return "entry price required";
  if(stop!=null){
    if(dir>0&&stop>=entry)return "long: SL must be below entry";
    if(dir<0&&stop<=entry)return "short: SL must be above entry";
  }
  if(target!=null){
    if(dir>0&&target<=entry)return "long: TP must be above entry";
    if(dir<0&&target>=entry)return "short: TP must be below entry";
  }
  return "";
}
async function placeReplayOrder(body){
  const err=replayOrderError(body);
  if(err){$("replayErr").textContent=err;return false;}
  return await replayAction("/api/replay/order",body);
}
async function replayAction(path,body){
  if(replayBusy)return false; // holding ArrowRight etc. must not pile up overlapping requests
  replayBusy=true;
  try{
    $("replayErr").textContent="";
    const r=await fetch(path+"?id="+encodeURIComponent(globalThis.RUN_ID||""),{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(body||{})});
    const data=await r.json().catch(()=>({}));
    if(!r.ok||data.ok===false){
      const msg=data.error||`failed ${r.status}`;
      $("replayErr").textContent=msg;
      replayToast(msg);
      return false;
    }
    await loadResult();
    return true;
  }catch(err){
    const msg=String(err&&err.message?err.message:err);
    $("replayErr").textContent=msg;
    replayToast(msg);
    return false;
  }finally{
    replayBusy=false;
  }
}
function replayLines(){
  const lines=[];
  const push=(o,scope)=>{
    const entry=o.entry_price??o.entry_px;
    if(!Number.isFinite(entry))return;
    const info={dir:o.dir,qty:o.qty,entry,order_type:o.order_type||o.source_order||"market",scope,
                stop:o.stop??null,target:o.target??null,
                closable:(scope==="pending"||scope==="open")&&!replayDrag};
    const live=!replayDraft;
    const mk=(kind,price,drag)=>({kind,scope,key:`${scope}:${kind}`,price,drag,o:info});
    lines.push(mk("entry",entry,live&&scope==="pending"));
    lines.push(mk("stop",o.stop,live&&(scope==="pending"||scope==="open")));
    lines.push(mk("target",o.target,live&&(scope==="pending"||scope==="open")));
  };
  // A pending order and an open position can coexist; while a draft is being dragged it
  // replaces its own scope's lines but the other scope stays visible for context.
  const draftScope=replayDraft?(replayDraft.scope||"draft"):null;
  if(replayDraft)push(replayDraft,draftScope);
  if(REPLAY.pending_order&&draftScope!=="pending")push(REPLAY.pending_order,"pending");
  if(REPLAY.open_position&&draftScope!=="open")push(REPLAY.open_position,"open");
  return lines.filter(l=>Number.isFinite(l.price));
}
function replayLineColor(line){
  if(line.kind==="stop")return REPLAY_SL_COLOR;
  if(line.kind==="target")return REPLAY_TP_COLOR;
  return line.o.dir>0?REPLAY_LONG_COLOR:REPLAY_SHORT_COLOR;
}
function replayQtyText(o){const q=Number(o.qty);return Number.isFinite(q)?String(+q.toFixed(2)):"?";}
function replayEntryChipText(o){
  if(o.scope==="open")return `${o.dir>0?"LONG":"SHORT"} ${replayQtyText(o)}`;
  return `${replayTypeLabel(o.order_type)} ${replaySideLabel(o.dir)} ${replayQtyText(o)}`;
}
function replayFmtUsd(v){return `${v>=0?"+":"−"}${Math.abs(v).toFixed(2)} USD`;}
// Projected (TP/SL lines) or floating (open-position entry line) PnL in account currency.
function replayLinePnl(line){
  const o=line.o, mult=(DATA&&DATA.multiplier)||1;
  const ref=line.kind==="entry"?replayCurrentPrice():line.price;
  if(ref==null||!Number.isFinite(o.entry))return null;
  return (ref-o.entry)*o.dir*o.qty*mult;
}
// One pill group: measures the segments, draws them right-aligned at xRight, and records
// each segment's rect (for the close button) plus the whole group's rect (for dragging).
function drawReplaySegs(segs,xRight,yy){
  const h=19,padX=7,gap=2;
  pctx.save();
  let total=0;
  for(const s of segs){
    pctx.font=(s.bold?"800 ":"700 ")+"11px Segoe UI,Arial";
    s.w=s.text==="×"?h:Math.ceil(pctx.measureText(s.text).width)+padX*2;
    total+=s.w+gap;
  }
  total-=gap;
  let x=Math.max(priceScale.L+4,Math.round(xRight-total));
  const y0=Math.round(yy-h/2), rect={x,y:y0,w:total,h};
  pctx.shadowColor="rgba(15,23,42,0.25)";pctx.shadowBlur=5;pctx.shadowOffsetY=1;
  pctx.fillStyle="#fff";roundedRect(pctx,x-1.5,y0-1.5,total+3,h+3,5);pctx.fill();
  pctx.shadowColor="transparent";
  pctx.textAlign="center";pctx.textBaseline="middle";
  for(const s of segs){
    pctx.font=(s.bold?"800 ":"700 ")+"11px Segoe UI,Arial";
    pctx.fillStyle=s.bg;roundedRect(pctx,x,y0,s.w,h,4);pctx.fill();
    if(s.border){
      pctx.strokeStyle=s.border;pctx.lineWidth=1;
      if(s.dash)pctx.setLineDash([3,2]);
      roundedRect(pctx,x+0.5,y0+0.5,s.w-1,h-1,3.5);pctx.stroke();
      pctx.setLineDash([]);
    }
    pctx.fillStyle=s.fg;
    pctx.fillText(s.text,x+s.w/2,y0+h/2+0.5);
    s.rect={x,y:y0,w:s.w,h};
    x+=s.w+gap;
  }
  pctx.restore();
  return rect;
}
function drawReplayLineWidget(line,yy,color,groupRects){
  const o=line.o, segs=[];
  const fill=o.dir>0?REPLAY_LONG_COLOR:REPLAY_SHORT_COLOR;
  const live=(line.scope==="open"||line.scope==="pending")&&!replayDrag;
  if(line.kind==="entry"){
    // TradingView-style setter chips: shown while that side is unset; press one and drag
    // it off the line to create the TP/SL at the release price.
    if(live&&o.target==null)segs.push({text:"TP",bg:"#fff",fg:REPLAY_TP_COLOR,border:REPLAY_TP_COLOR,bold:true,dash:true,set:"target"});
    if(live&&o.stop==null)segs.push({text:"SL",bg:"#fff",fg:REPLAY_SL_COLOR,border:REPLAY_SL_COLOR,bold:true,dash:true,set:"stop"});
    segs.push({text:replayEntryChipText(o),bg:fill,fg:"#fff"});
    if(line.scope==="open"){
      const pnl=replayLinePnl(line);
      if(pnl!=null)segs.push({text:replayFmtUsd(pnl),bg:"#fff",fg:pnl>=0?REPLAY_TP_COLOR:REPLAY_SL_COLOR,border:"#cbd5e1"});
    } else {
      segs.push({text:Number(line.price).toFixed(2),bg:"#fff",fg:"#334155",border:"#cbd5e1"});
    }
    if(o.closable)segs.push({text:"×",bg:fill,fg:"#fff",action:line.scope==="pending"?"cancel":"close"});
  } else {
    const isTp=line.kind==="target";
    const pnl=replayLinePnl(line);
    const ticks=Math.round(Math.abs(line.price-o.entry)/replayTick());
    segs.push({text:isTp?"TP":"SL",bg:"#fff",fg:color,border:color,bold:true});
    segs.push({text:`${ticks}t`,bg:"#fff",fg:"#475569",border:"#cbd5e1"});
    if(pnl!=null)segs.push({text:replayFmtUsd(pnl),bg:"#fff",fg:pnl>=0?REPLAY_TP_COLOR:REPLAY_SL_COLOR,border:"#cbd5e1"});
    if(o.closable)segs.push({text:"×",bg:"#fff",fg:"#94a3b8",border:"#cbd5e1",action:isTp?"clear-target":"clear-stop"});
  }
  const {L,T,pw,ph}=priceScale;
  // Nudge the tag downward while it overlaps an already-drawn one (lines at nearby prices),
  // so stacked widgets stay readable and clickable.
  let yc=clamp(yy,T+11,T+ph-11);
  for(let guard=0;guard<8;guard++){
    const clash=groupRects.find(g=>Math.abs(g-yc)<21);
    if(clash===undefined)break;
    yc=clash+21;
  }
  yc=clamp(yc,T+11,T+ph-11);
  groupRects.push(yc);
  const rect=drawReplaySegs(segs,L+pw-8,yc);
  if(line.drag)replayWidgets.push({type:"drag",line,rect});
  for(const s of segs){
    if(s.action)replayWidgets.push({type:s.action,scope:line.scope,rect:s.rect});
    if(s.set)replayWidgets.push({type:"set",kind:s.set,scope:line.scope,rect:s.rect});
  }
}
function drawReplayOverlay(){
  replayWidgets=[];
  if(!priceScale||chartMode==="heatmap")return;
  const {L,T,pw,ph,lo,hi}=priceScale, y=p=>T+(hi-p)/(hi-lo)*ph;
  const W=price.clientWidth,R=AXIS_W;
  pctx.save();
  const groupRects=[];
  for(const line of replayLines()){
    const yy=y(line.price); if(yy<T-9||yy>T+ph+9)continue;
    const color=replayLineColor(line);
    const draggingLine=replayDrag&&replayDrag.mode==="line"&&replayDrag.line.key===line.key;
    const draggingSet=replayDrag&&replayDrag.mode==="set"&&replayDrag.scope===line.scope&&replayDrag.kind===line.kind;
    const dragging=draggingLine||draggingSet;
    const hot=dragging||(line.drag&&replayHoverKind===line.key);
    pctx.strokeStyle=colorAlpha(color,hot?1:0.8);
    pctx.lineWidth=hot?2:1.2;
    pctx.setLineDash(line.kind==="entry"?(line.scope==="open"?[]:[7,4]):[3,3]);
    pctx.beginPath(); pctx.moveTo(L,yy); pctx.lineTo(L+pw,yy); pctx.stroke(); pctx.setLineDash([]);
    drawReplayLineWidget(line,yy,color,groupRects);
    if(replayDrag&&(replayDrag.mode==="create"||dragging))axisPriceTag(pctx,W,R,yy,Number(line.price).toFixed(2),color);
  }
  if(replaySpace&&!REPLAY.pending_order){
    pctx.font="12px Segoe UI,Arial"; pctx.textAlign="left"; pctx.textBaseline="middle";
    pctx.fillStyle="#16a34a"; pctx.fillText("SPACE: chart order",L+6,T+14);
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
function panPriceViewToReveal(rawPrice){
  if(!priceScale)return rawPrice;
  const {lo,hi}=priceScale;
  if(rawPrice>=lo&&rawPrice<=hi)return rawPrice;
  const range=Math.max(1e-9,hi-lo), margin=range*0.08;
  const st=yZoom.price;
  st.manual=true;
  if(rawPrice>hi){ const shift=rawPrice-hi+margin; st.lo=lo+shift; st.hi=hi+shift; }
  else { const shift=lo-rawPrice+margin; st.lo=lo-shift; st.hi=hi-shift; }
  draw();
  return rawPrice;
}
// Dragging past the visible edge pans the view to reveal more room (see panPriceViewToReveal),
// but a straight linear extrapolation of price-per-pixel makes that pan feel too fast/twitchy
// once the cursor is well past the edge. Damp price movement in the overflow zone so the user
// has to move further to get the same amount of pan, while in-bounds dragging stays 1:1.
const REPLAY_EDGE_PAN_DAMPING=0.15;
function replayPriceFromEventForDrag(ev){
  const r=price.getBoundingClientRect(), y=ev.clientY-r.top;
  const {T,ph,lo,hi}=priceScale;
  const pxToPrice=(hi-lo)/Math.max(1,ph);
  let raw;
  if(y<T) raw=hi+(T-y)*pxToPrice*REPLAY_EDGE_PAN_DAMPING;
  else if(y>T+ph) raw=lo-(y-(T+ph))*pxToPrice*REPLAY_EDGE_PAN_DAMPING;
  else raw=hi-(y-T)*pxToPrice;
  return snapPrice(panPriceViewToReveal(raw));
}
function replayWidgetAt(ev){
  const r=price.getBoundingClientRect(),x=ev.clientX-r.left,yv=ev.clientY-r.top;
  const hit=w=>x>=w.rect.x&&x<=w.rect.x+w.rect.w&&yv>=w.rect.y&&yv<=w.rect.y+w.rect.h;
  // Action buttons win over the surrounding drag group (the x sits inside the group rect).
  for(let i=replayWidgets.length-1;i>=0;i--){const w=replayWidgets[i];if(w.type!=="drag"&&hit(w))return w;}
  for(let i=replayWidgets.length-1;i>=0;i--){const w=replayWidgets[i];if(hit(w))return w;}
  return null;
}
function updateReplayHoverTarget(ev){
  if(replayDrag||chartMode==="heatmap"||!priceScale){replayHoverKind=null;replayHoverAction=null;return;}
  const w=replayWidgetAt(ev);
  if(w&&w.type!=="drag"){replayHoverAction=w.type;replayHoverKind=null;return;}
  replayHoverAction=null;
  const line=w?w.line:replayLineAt(ev);
  replayHoverKind=line&&line.drag?line.key:null;
}
// Runs after the studio's own mousemove handler (registered later, bubble phase), so the
// replay cursor wins over the default pan/axis-resize cursors when hovering a widget.
function replayCursorMove(){
  if(replayDrag){price.style.cursor="ns-resize";return;}
  if(replayHoverAction)price.style.cursor="pointer";
  else if(replayHoverKind)price.style.cursor="ns-resize";
}
function replayHoverMove(ev){
  updateReplayHoverTarget(ev);
  if(!replaySpace||REPLAY.pending_order||chartMode==="heatmap"||!priceScale)return;
  replayHoverPrice=replayPriceFromEvent(ev);
  updateReplayMode();
  draw();
}
function replayHoverLeave(){
  replayHoverKind=null; replayHoverAction=null;
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
// Clone the current pending order / open position as a draft the set-drag can write into.
function replaySetDraft(scope){
  if(scope==="pending"&&REPLAY.pending_order)return {...REPLAY.pending_order,scope:"pending"};
  if(scope==="open"&&REPLAY.open_position){
    const p=REPLAY.open_position;
    return {scope:"open",dir:p.dir,order_type:p.source_order||"market",qty:p.qty,entry_price:p.entry_px,stop:p.stop,target:p.target};
  }
  return null;
}
function replayMouseDown(ev){
  lastDown={x:ev.clientX,y:ev.clientY};
  if(ev.button===0){
    const w=replayWidgetAt(ev);
    if(w){
      ev.preventDefault(); ev.stopPropagation();
      if(w.type==="drag"){ replayDrag={mode:"line",line:w.line}; return; }
      if(w.type==="set"){
        const draft=replaySetDraft(w.scope);
        if(!draft)return;
        replayDrag={mode:"set",kind:w.kind,scope:w.scope,moved:false};
        replayDraft=draft;
        draw();
        return;
      }
      if(w.type==="cancel"||w.type==="close"){
        replayAction(w.type==="cancel"?"/api/replay/cancel":"/api/replay/flatten",{})
          .then(ok=>{ if(ok)replayToast(w.type==="cancel"?"Order cancelled — Back (←) undoes":"Position closed — Back (←) undoes",true); });
        return;
      }
      if(w.type==="clear-stop"||w.type==="clear-target"){
        const body={scope:w.scope};
        body[w.type==="clear-stop"?"clear_stop":"clear_target"]=true;
        replayAction("/api/replay/update",body)
          .then(ok=>{ if(ok)replayToast(w.type==="clear-stop"?"SL removed":"TP removed",true); });
        return;
      }
      return;
    }
  }
  const line=replayLineAt(ev);
  if(line){
    replayDrag={mode:"line",line}; ev.preventDefault(); ev.stopPropagation(); return;
  }
  if(!replaySpace||(ev.button!==0&&ev.button!==2)||REPLAY.pending_order||chartMode==="heatmap")return;
  const entry=replayPriceFromEvent(ev), dir=replayDirForButton(ev.button);
  replayHoverPrice=entry;
  replayDraft=replayDraftFrom(entry,dir,entry);
  replayDrag={mode:"create",entry,dir};
  updateReplayMode(); draw();
  ev.preventDefault(); ev.stopPropagation();
}
function replayMouseMove(ev){
  if(!replayDrag)return;
  const p=replayPriceFromEventForDrag(ev);
  if(replayDrag.mode==="create"){
    replayDraft=replayDraftFrom(replayDrag.entry,replayDrag.dir,p);
  } else if(replayDrag.mode==="set"){
    if(replayDraft){ replayDraft[replayDrag.kind]=p; replayDrag.moved=true; }
  } else {
    const line=replayDrag.line;
    if(line.scope==="pending"&&REPLAY.pending_order){
      replayDraft={...REPLAY.pending_order,scope:"pending"};
      if(line.kind==="entry")replayDraft.entry_price=p;
      if(line.kind==="stop")replayDraft.stop=p;
      if(line.kind==="target")replayDraft.target=p;
      replayDraft.order_type=replayOrderType(replayDraft.entry_price,replayDraft.dir);
    } else if(line.scope==="open"&&REPLAY.open_position){
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
  } else if(dragNow.mode==="set"){
    const d=replayDraft;
    replayDraft=null;
    // A press without movement is just a click on the chip: nothing to set.
    if(d&&dragNow.moved){
      const body={scope:dragNow.scope};
      body[dragNow.kind]=d[dragNow.kind];
      await replayAction("/api/replay/update",body);
    }
  } else {
    const line=dragNow.line, d=replayDraft;
    replayDraft=null;
    if((line.scope==="pending"||line.scope==="open")&&d){
      await replayAction("/api/replay/update",{scope:line.scope,order_type:line.scope==="pending"?d.order_type:null,entry_price:line.scope==="pending"?d.entry_price:null,stop:d.stop,target:d.target});
    }
  }
  updateReplayMode(); draw();
}
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fp(i: i64, base: f64) -> Footprint {
        Footprint {
            ts_first_ns: i * 60_000_000_000,
            ts_last_ns: i * 60_000_000_000 + 59_000_000_000,
            open: base,
            high: base + 1.0,
            low: base - 1.0,
            close: base + 0.5,
            volume: 100.0,
            trades: 10,
            delta: 0.0,
            poc: base,
            ladder: Vec::new(),
            values: Default::default(),
            tags: Default::default(),
            plots: Vec::new(),
        }
    }

    /// 100 bars, price cycling 100.0..102.25 in 0.25 steps (period 10).
    fn manager(record_dir: &std::path::Path) -> ReplayManager {
        let footprints = (0..100)
            .map(|i| fp(i as i64, 100.0 + (i % 10) as f64 * 0.25))
            .collect();
        let prepared = PreparedBacktestData {
            footprints,
            liquidity_heatmap: Default::default(),
            tick_size: 0.25,
            multiplier: 2.0,
        };
        ReplayManager::new(Arc::new(prepared), "test", record_dir)
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("accel_replay_test_{tag}_{}", now_ms()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn new_session(mgr: &ReplayManager, req: NewSessionRequest) -> ReplayPublicState {
        mgr.create_session(req).expect("create session")
    }

    fn wide_market(dir: i32, qty: f64, close: f64) -> OrderRequest {
        // Bracket far outside the 1-point bar range so nothing triggers while stepping.
        OrderRequest {
            dir,
            order_type: "market".to_string(),
            qty,
            entry_price: None,
            stop: Some(close - dir as f64 * 50.0),
            target: Some(close + dir as f64 * 50.0),
        }
    }

    #[test]
    fn warmup_and_step_and_back() {
        let dir = tmp_dir("back");
        let mgr = manager(&dir);
        let st = new_session(&mgr, NewSessionRequest::default());
        assert_eq!(st.cursor, 20, "default warmup = 20% of 100 bars");
        assert!(!st.can_back);
        let st = mgr.advance(&st.id, 5, false).unwrap();
        assert_eq!(st.cursor, 25);
        assert!(st.can_back);
        let st = mgr.back(&st.id, 2).unwrap();
        assert_eq!(st.cursor, 23);
        let st = mgr.back(&st.id, 100).unwrap();
        assert_eq!(st.cursor, 20, "rewinds stop at session start");
        assert!(!st.can_back);
        assert!(mgr.back(&st.id, 1).is_err(), "empty undo stack errors");
    }

    #[test]
    fn scale_reduce_reverse_and_flatten_undo() {
        let dir = tmp_dir("scale");
        let mgr = manager(&dir);
        let st = new_session(&mgr, NewSessionRequest::default());
        let id = st.id.clone();
        let close = st.current_close.unwrap();

        let st = mgr.place_order(&id, wide_market(1, 2.0, close)).unwrap();
        let pos = st.open_position.clone().unwrap();
        assert_eq!(pos.dir, 1);
        assert!((pos.qty - 2.0).abs() < 1e-9);

        // Scale in without a bracket: qty grows, entry averages, bracket kept.
        let mut scale = wide_market(1, 1.0, close);
        scale.stop = None;
        scale.target = None;
        let st = mgr.place_order(&id, scale).unwrap();
        let pos2 = st.open_position.clone().unwrap();
        assert!((pos2.qty - 3.0).abs() < 1e-9);
        assert_eq!(pos2.stop, pos.stop);
        assert_eq!(pos2.target, pos.target);

        // Reduce: opposite direction, smaller qty, no bracket needed.
        let mut reduce = wide_market(-1, 1.0, close);
        reduce.stop = None;
        reduce.target = None;
        let st = mgr.place_order(&id, reduce).unwrap();
        assert!((st.open_position.clone().unwrap().qty - 2.0).abs() < 1e-9);
        assert_eq!(st.trades, 1, "partial close records a trade");

        // Reverse: opposite direction, larger qty, bracket required for the remainder.
        let st = mgr.place_order(&id, wide_market(-1, 5.0, close)).unwrap();
        let pos3 = st.open_position.clone().unwrap();
        assert_eq!(pos3.dir, -1);
        assert!((pos3.qty - 3.0).abs() < 1e-9);
        assert_eq!(st.trades, 2);

        // Flatten, then undo restores the position.
        let st = mgr.flatten(&id).unwrap();
        assert!(st.open_position.is_none());
        assert_eq!(st.trades, 3);
        let st = mgr.back(&id, 1).unwrap();
        let pos4 = st.open_position.clone().unwrap();
        assert_eq!(pos4.dir, -1);
        assert_eq!(st.trades, 2, "undo removed the flatten trade");
    }

    #[test]
    fn stop_can_cross_entry_on_open_position() {
        let dir = tmp_dir("breakeven");
        let mgr = manager(&dir);
        let st = new_session(&mgr, NewSessionRequest::default());
        let id = st.id.clone();
        let close = st.current_close.unwrap();
        let st = mgr.place_order(&id, wide_market(1, 1.0, close)).unwrap();
        let pos = st.open_position.clone().unwrap();

        // Move the stop above entry (lock in profit): must be allowed now.
        let req = UpdateOrderRequest {
            scope: Some("open".to_string()),
            order_type: None,
            entry_price: None,
            stop: Some(pos.entry_px + 2.0),
            target: None,
            clear_stop: None,
            clear_target: None,
        };
        let st = mgr.update_active_order(&id, req).unwrap();
        let pos = st.open_position.clone().unwrap();
        assert!(pos.stop.unwrap() > pos.entry_px);

        // But the stop may not cross the target.
        let req = UpdateOrderRequest {
            scope: Some("open".to_string()),
            order_type: None,
            entry_price: None,
            stop: Some(pos.target.unwrap() + 1.0),
            target: None,
            clear_stop: None,
            clear_target: None,
        };
        assert!(mgr.update_active_order(&id, req).is_err());
    }

    #[test]
    fn bare_order_then_set_and_clear_protection() {
        let dir = tmp_dir("bare");
        let mgr = manager(&dir);
        let st = new_session(&mgr, NewSessionRequest::default());
        let id = st.id.clone();

        // Market order with no protection at all: a single entry line.
        let order = OrderRequest {
            dir: 1,
            order_type: "market".to_string(),
            qty: 1.0,
            entry_price: None,
            stop: None,
            target: None,
        };
        let st = mgr.place_order(&id, order).unwrap();
        let pos = st.open_position.clone().unwrap();
        assert_eq!(pos.stop, None);
        assert_eq!(pos.target, None);

        // Stepping with no bracket never exits.
        let st = mgr.advance(&id, 10, true).unwrap();
        assert!(st.open_position.is_some());
        assert_eq!(st.trades, 0);

        // Drag a TP out of the position tag, then remove it again.
        let set_tp = UpdateOrderRequest {
            scope: Some("open".to_string()),
            order_type: None,
            entry_price: None,
            stop: None,
            target: Some(200.0),
            clear_stop: None,
            clear_target: None,
        };
        let st = mgr.update_active_order(&id, set_tp).unwrap();
        assert_eq!(st.open_position.clone().unwrap().target, Some(200.0));
        let clear_tp = UpdateOrderRequest {
            scope: Some("open".to_string()),
            order_type: None,
            entry_price: None,
            stop: None,
            target: None,
            clear_stop: None,
            clear_target: Some(true),
        };
        let st = mgr.update_active_order(&id, clear_tp).unwrap();
        assert_eq!(st.open_position.clone().unwrap().target, None);
    }

    #[test]
    fn skip_pauses_on_fill() {
        let dir = tmp_dir("stopevt");
        let mgr = manager(&dir);
        let st = new_session(&mgr, NewSessionRequest::default());
        let id = st.id.clone();
        // Breakout buy above the current highs; fills a few bars in as the cycle rises.
        let entry = 101.6;
        let order = OrderRequest {
            dir: 1,
            order_type: "breakout".to_string(),
            qty: 1.0,
            entry_price: Some(entry),
            stop: Some(entry - 50.0),
            target: Some(entry + 50.0),
        };
        let st = mgr.place_order(&id, order).unwrap();
        let before = st.cursor;
        let st = mgr.advance(&id, 50, true).unwrap();
        assert!(st.open_position.is_some(), "order filled");
        assert!(
            st.cursor - before < 50,
            "skip paused on the fill bar (stepped {} bars)",
            st.cursor - before
        );
        // Without stop_on_event the same skip runs to completion.
        let st2 = mgr.advance(&id, 10, false).unwrap();
        assert_eq!(st2.cursor, st.cursor + 10);
    }

    #[test]
    fn list_delete_and_reject_mismatched_data() {
        let dir = tmp_dir("list");
        let mgr = manager(&dir);
        let st = new_session(&mgr, NewSessionRequest::default());
        let id = st.id.clone();
        mgr.advance(&id, 3, false).unwrap();

        let list = mgr.list_sessions();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], serde_json::json!(id));
        assert_eq!(list[0]["compatible"], serde_json::json!(true));

        // A manager over different data must flag the record and refuse to resume it.
        let footprints = (0..50).map(|i| fp(i as i64 + 1000, 200.0)).collect();
        let prepared = PreparedBacktestData {
            footprints,
            liquidity_heatmap: Default::default(),
            tick_size: 0.25,
            multiplier: 2.0,
        };
        let other = ReplayManager::new(Arc::new(prepared), "other", &dir);
        let list2 = other.list_sessions();
        assert_eq!(list2[0]["compatible"], serde_json::json!(false));
        let err = other.state_value(&id, 0, 0).unwrap_err();
        assert!(err.contains("different data"), "got: {err}");

        // Delete removes the record; bogus / traversal ids are rejected.
        mgr.delete_session(&id).unwrap();
        assert!(mgr.list_sessions().is_empty());
        assert!(mgr.delete_session(&id).is_err());
        assert!(mgr.delete_session("..\\evil").is_err());
    }

    #[test]
    fn restore_from_disk_and_incremental_state() {
        let dir = tmp_dir("restore");
        let id;
        let cursor_before;
        {
            let mgr = manager(&dir);
            let st = new_session(&mgr, NewSessionRequest::default());
            id = st.id.clone();
            let st = mgr.advance(&st.id, 7, false).unwrap();
            cursor_before = st.cursor;
        }
        // Fresh manager (server restart): the session comes back from the record file.
        let mgr = manager(&dir);
        let full = mgr.state_value(&id, 0, 0).unwrap();
        let replay = &full["replay"];
        assert_eq!(replay["cursor"].as_u64().unwrap() as usize, cursor_before);
        assert_eq!(replay["can_back"], serde_json::json!(false));
        let n_fp = full["footprints"].as_array().unwrap().len();
        assert_eq!(n_fp, cursor_before + 1, "full fetch returns bars 0..=cursor");
        let n_eq = full["equity"].as_array().unwrap().len();

        let st = mgr.advance(&id, 3, false).unwrap();
        let inc = mgr.state_value(&id, n_fp, n_eq).unwrap();
        assert_eq!(inc["footprints_from"].as_u64().unwrap() as usize, n_fp);
        assert_eq!(
            inc["footprints"].as_array().unwrap().len(),
            3,
            "incremental fetch returns only the new bars"
        );
        assert_eq!(st.cursor, cursor_before + 3);

        // After a rewind the requested offset is clamped and the client truncates.
        mgr.back(&id, 2).unwrap();
        let inc2 = mgr.state_value(&id, n_fp + 3, n_eq + 3).unwrap();
        assert_eq!(
            inc2["footprints_from"].as_u64().unwrap() as usize,
            n_fp + 1,
            "offset clamps to the rewound end"
        );
        assert_eq!(inc2["footprints"].as_array().unwrap().len(), 0);
    }
}
