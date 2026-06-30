//! The streaming backtest engine: wires the four stages together and runs them one footprint
//! at a time. This is the hot loop used by normal runs and hyperopt evaluators.

use crate::broker::{Broker, BrokerConfig, OrderCtx};
use crate::event::{EventInterest, OrderFlowEvent};
use crate::footprint::Footprint;
use crate::metrics::Metrics;
use crate::progress::ProgressHandle;
use crate::result::{BacktestResult, LiquidityHeatmap, LiquidityLevel, LiquiditySnapshot};
use crate::traits::{DataSource, FootprintAggregator, Indicator, Strategy};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default)]
struct BookLevel {
    bid_size: f64,
    ask_size: f64,
}

#[derive(Debug)]
struct BookDepth {
    tick_size: f64,
    levels: BTreeMap<i64, BookLevel>,
    max_levels_per_snapshot: usize,
}

impl BookDepth {
    fn new() -> Self {
        Self {
            tick_size: 0.25,
            levels: BTreeMap::new(),
            max_levels_per_snapshot: 400,
        }
    }

    fn on_event(&mut self, ev: &OrderFlowEvent) {
        match *ev {
            OrderFlowEvent::Contract { tick_size, .. } => {
                if tick_size > 0.0 && tick_size.is_finite() {
                    self.tick_size = tick_size;
                }
            }
            OrderFlowEvent::AddLimit {
                price, size, side, ..
            } => self.add(price, size, side.sign()),
            OrderFlowEvent::ReduceLimit {
                price, size, side, ..
            } => self.add(price, -size, side.sign()),
            OrderFlowEvent::SetLevel {
                price, size, side, ..
            } => self.set(price, size, side.sign()),
            OrderFlowEvent::Trade { .. } => {}
        }
    }

    /// Overwrite the resting size on one side of `price` with an absolute value. `size <= 0` clears
    /// that side, and the level is dropped once both sides are empty.
    fn set(&mut self, price: f64, size: f64, side_sign: f64) {
        if !(price.is_finite() && size.is_finite()) {
            return;
        }
        let key = self.price_key(price);
        let level = self.levels.entry(key).or_default();
        if side_sign > 0.0 {
            level.bid_size = size.max(0.0);
        } else {
            level.ask_size = size.max(0.0);
        }
        if level.bid_size <= f64::EPSILON && level.ask_size <= f64::EPSILON {
            self.levels.remove(&key);
        }
    }

    fn add(&mut self, price: f64, signed_size: f64, side_sign: f64) {
        if !(price.is_finite() && signed_size.is_finite()) {
            return;
        }
        let key = self.price_key(price);
        let level = self.levels.entry(key).or_default();
        if side_sign > 0.0 {
            level.bid_size = (level.bid_size + signed_size).max(0.0);
        } else {
            level.ask_size = (level.ask_size + signed_size).max(0.0);
        }
        if level.bid_size <= f64::EPSILON && level.ask_size <= f64::EPSILON {
            self.levels.remove(&key);
        }
    }

    fn snapshot(&self, ts_ns: i64, mid_price: f64) -> LiquiditySnapshot {
        let mut levels = Vec::new();
        if self.levels.is_empty() {
            return LiquiditySnapshot { ts_ns, levels };
        }

        let center = self.price_key(mid_price);
        let half = (self.max_levels_per_snapshot / 2) as i64;
        let min_key = center.saturating_sub(half);
        let max_key = center.saturating_add(half);
        for (&key, level) in self.levels.range(min_key..=max_key) {
            if level.bid_size <= f64::EPSILON && level.ask_size <= f64::EPSILON {
                continue;
            }
            levels.push(LiquidityLevel {
                price: self.key_price(key),
                bid_size: level.bid_size,
                ask_size: level.ask_size,
            });
        }
        LiquiditySnapshot { ts_ns, levels }
    }

    fn price_key(&self, price: f64) -> i64 {
        if self.tick_size > 0.0 {
            (price / self.tick_size).round() as i64
        } else {
            (price * 1_000_000.0).round() as i64
        }
    }

    fn key_price(&self, key: i64) -> f64 {
        if self.tick_size > 0.0 {
            key as f64 * self.tick_size
        } else {
            key as f64 / 1_000_000.0
        }
    }
}
/// A fully-assembled, ready-to-run pipeline.
pub struct Pipeline {
    pub source: Box<dyn DataSource>,
    pub aggregator: Box<dyn FootprintAggregator>,
    pub indicators: Vec<Box<dyn Indicator>>,
    pub strategy: Box<dyn Strategy>,
    pub broker_cfg: BrokerConfig,
    /// Keep enriched footprints in the result (set false in hyperopt for speed/memory).
    pub keep_footprints: bool,
    /// Keep sampled L2 depth snapshots for heatmap rendering. Disable for metric-only runs.
    pub keep_liquidity_heatmap: bool,
}

/// Footprints and optional depth snapshots prepared once from a raw source.
///
/// This is the reusable middle layer for multi-strategy / multi-parameter batches where the data
/// adapter, aggregator and indicators are identical but strategies differ.
#[derive(Clone, Debug)]
pub struct PreparedBacktestData {
    pub footprints: Vec<Footprint>,
    pub liquidity_heatmap: LiquidityHeatmap,
    pub tick_size: f64,
    pub multiplier: f64,
}

/// Run a single backtest end to end (no progress reporting).
pub fn run_backtest(pipeline: Pipeline) -> BacktestResult {
    run_backtest_progress(pipeline, None)
}

/// Prepare enriched footprints from a raw source without running a strategy or broker.
pub fn prepare_backtest_data(
    mut source: Box<dyn DataSource>,
    mut aggregator: Box<dyn FootprintAggregator>,
    mut indicators: Vec<Box<dyn Indicator>>,
    keep_liquidity_heatmap: bool,
    progress: Option<ProgressHandle>,
) -> PreparedBacktestData {
    let mut history: Vec<Footprint> = Vec::new();
    let mut liquidity_heatmap = LiquidityHeatmap::default();
    let mut book_depth = keep_liquidity_heatmap.then(BookDepth::new);
    let mut tick_size = 0.25;
    let mut multiplier = 1.0;
    let aggregator_interest = aggregator.event_interest();
    let indicator_interests: Vec<EventInterest> =
        indicators.iter().map(|ind| ind.event_interest()).collect();
    let mut source_interest = EventInterest::CONTRACT.union(aggregator_interest);
    for interest in &indicator_interests {
        source_interest = source_interest.union(*interest);
    }
    if keep_liquidity_heatmap {
        source_interest = source_interest.union(EventInterest::L2);
    }

    source.set_event_interest(source_interest);
    if let Some(h) = &progress {
        source.set_progress(h.clone());
    }

    let handle = |fp: Footprint,
                  history: &mut Vec<Footprint>,
                  indicators: &mut Vec<Box<dyn Indicator>>,
                  liquidity_heatmap: &mut LiquidityHeatmap,
                  book_depth: Option<&BookDepth>| {
        let mut fp = fp;
        for ind in indicators.iter_mut() {
            ind.on_footprint(&mut fp, history);
        }
        if let Some(book_depth) = book_depth {
            let depth = book_depth.snapshot(fp.ts_last_ns, fp.close);
            if !depth.levels.is_empty() {
                liquidity_heatmap.snapshots.push(depth);
            }
        }
        history.push(fp);
    };

    for ev in source.events() {
        if let OrderFlowEvent::Contract {
            tick_size: t,
            multiplier: m,
        } = ev
        {
            if t > 0.0 {
                tick_size = t;
            }
            if m > 0.0 {
                multiplier = m;
            }
        }

        if aggregator_interest.matches(&ev) {
            if let Some(fp) = aggregator.on_event(&ev) {
                handle(
                    fp,
                    &mut history,
                    &mut indicators,
                    &mut liquidity_heatmap,
                    book_depth.as_ref(),
                );
                if let Some(h) = &progress {
                    h.inc_footprints();
                }
            }
        }

        for (ind, interest) in indicators.iter_mut().zip(indicator_interests.iter()) {
            if interest.matches(&ev) {
                ind.on_event(&ev);
            }
        }
        if let Some(book_depth) = book_depth.as_mut() {
            book_depth.on_event(&ev);
        }
    }
    if let Some(fp) = aggregator.flush() {
        handle(
            fp,
            &mut history,
            &mut indicators,
            &mut liquidity_heatmap,
            book_depth.as_ref(),
        );
        if let Some(h) = &progress {
            h.inc_footprints();
        }
    }
    if let Some(h) = &progress {
        h.finish();
    }

    PreparedBacktestData {
        footprints: history,
        liquidity_heatmap,
        tick_size,
        multiplier,
    }
}

/// Run a footprint-only strategy over prepared enriched footprints.
pub fn run_prepared_backtest(
    data: &PreparedBacktestData,
    mut strategy: Box<dyn Strategy>,
    broker_cfg: BrokerConfig,
    keep_footprints: bool,
) -> BacktestResult {
    assert_eq!(
        strategy.event_interest(),
        EventInterest::NONE,
        "run_prepared_backtest only supports footprint-only strategies"
    );

    let mut broker = Broker::new(broker_cfg);
    broker.set_contract(data.tick_size, data.multiplier);
    let mut history = Vec::with_capacity(if keep_footprints {
        data.footprints.len()
    } else {
        0
    });
    let mut last_close = f64::NAN;
    let mut last_ts = 0i64;

    for source_fp in &data.footprints {
        last_close = source_fp.close;
        last_ts = source_fp.ts_last_ns;
        broker.on_new_footprint(source_fp);
        let mut ctx = OrderCtx::new(&mut broker);
        strategy.on_footprint(source_fp, &mut ctx);
        if keep_footprints {
            let mut fp = source_fp.clone();
            fp.plots.extend(ctx.take_plots());
            history.push(fp);
        }
    }

    if last_close.is_finite() {
        broker.finalize(last_close, last_ts);
    }

    let metrics = Metrics::compute(broker.starting_equity(), &broker.trades, &broker.equity);
    BacktestResult {
        metrics,
        trades: broker.trades,
        equity: broker.equity,
        footprints: history,
        liquidity_heatmap: if keep_footprints {
            data.liquidity_heatmap.clone()
        } else {
            LiquidityHeatmap::default()
        },
        tick_size: data.tick_size,
        multiplier: data.multiplier,
    }
}

/// Run a single backtest, optionally reporting progress through `progress`.
pub fn run_backtest_progress(
    pipeline: Pipeline,
    progress: Option<ProgressHandle>,
) -> BacktestResult {
    let Pipeline {
        source,
        mut aggregator,
        mut indicators,
        mut strategy,
        broker_cfg,
        keep_footprints,
        keep_liquidity_heatmap,
    } = pipeline;

    let mut broker = Broker::new(broker_cfg);
    let mut history: Vec<Footprint> = Vec::new();
    let mut liquidity_heatmap = LiquidityHeatmap::default();
    let mut book_depth = keep_liquidity_heatmap.then(BookDepth::new);
    let mut last_close = f64::NAN;
    let mut last_ts = 0i64;
    let aggregator_interest = aggregator.event_interest();
    let indicator_interests: Vec<EventInterest> =
        indicators.iter().map(|ind| ind.event_interest()).collect();
    assert_eq!(
        strategy.event_interest(),
        EventInterest::NONE,
        "run_backtest_progress only supports footprint-only strategies"
    );
    let mut source_interest = EventInterest::CONTRACT.union(aggregator_interest);
    for interest in &indicator_interests {
        source_interest = source_interest.union(*interest);
    }
    if keep_liquidity_heatmap {
        source_interest = source_interest.union(EventInterest::L2);
    }

    let mut source = source;
    source.set_event_interest(source_interest);
    if let Some(h) = &progress {
        source.set_progress(h.clone());
    }

    let handle = |fp: Footprint,
                  broker: &mut Broker,
                  history: &mut Vec<Footprint>,
                  indicators: &mut Vec<Box<dyn Indicator>>,
                  strategy: &mut Box<dyn Strategy>,
                  liquidity_heatmap: &mut LiquidityHeatmap,
                  book_depth: Option<&BookDepth>| {
        let mut fp = fp;
        // Broker first: fill last bar's intent at this open, check stops, mark equity.
        broker.on_new_footprint(&fp);
        // Indicators enrich the footprint causally.
        for ind in indicators.iter_mut() {
            ind.on_footprint(&mut fp, history);
        }
        if let Some(book_depth) = book_depth {
            let depth = book_depth.snapshot(fp.ts_last_ns, fp.close);
            if !depth.levels.is_empty() {
                liquidity_heatmap.snapshots.push(depth);
            }
        }
        // Strategy reacts and sets intent for the next bar.
        {
            let mut ctx = OrderCtx::new(broker);
            strategy.on_footprint(&fp, &mut ctx);
            fp.plots.extend(ctx.take_plots());
        }
        history.push(fp);
    };

    for ev in source.events() {
        match ev {
            OrderFlowEvent::Contract {
                tick_size,
                multiplier,
            } => {
                broker.set_contract(tick_size, multiplier);
                // Let the aggregator see it too (no footprint emitted from metadata).
                if aggregator_interest.matches(&ev) {
                    let _ = aggregator.on_event(&ev);
                }
            }
            _ => {
                if let OrderFlowEvent::Trade { price, ts_ns, .. } = ev {
                    last_close = price;
                    last_ts = ts_ns;
                }
                if aggregator_interest.matches(&ev) {
                    if let Some(fp) = aggregator.on_event(&ev) {
                        last_close = fp.close;
                        last_ts = fp.ts_last_ns;
                        handle(
                            fp,
                            &mut broker,
                            &mut history,
                            &mut indicators,
                            &mut strategy,
                            &mut liquidity_heatmap,
                            book_depth.as_ref(),
                        );
                        if let Some(h) = &progress {
                            h.inc_footprints();
                        }
                    }
                }
            }
        }

        // Event-level consumers see the same normalized stream after any prior footprint close.
        // That keeps boundary-crossing events associated with the new in-progress footprint.
        for (ind, interest) in indicators.iter_mut().zip(indicator_interests.iter()) {
            if interest.matches(&ev) {
                ind.on_event(&ev);
            }
        }
        if let Some(book_depth) = book_depth.as_mut() {
            book_depth.on_event(&ev);
        }
    }
    if let Some(fp) = aggregator.flush() {
        last_close = fp.close;
        last_ts = fp.ts_last_ns;
        handle(
            fp,
            &mut broker,
            &mut history,
            &mut indicators,
            &mut strategy,
            &mut liquidity_heatmap,
            book_depth.as_ref(),
        );
        if let Some(h) = &progress {
            h.inc_footprints();
        }
    }

    if last_close.is_finite() {
        broker.finalize(last_close, last_ts);
    }
    if let Some(h) = &progress {
        h.finish();
    }

    let metrics = Metrics::compute(broker.starting_equity(), &broker.trades, &broker.equity);
    let tick_size = broker.tick_size();
    let multiplier = broker.multiplier();
    BacktestResult {
        metrics,
        trades: broker.trades,
        equity: broker.equity,
        footprints: if keep_footprints { history } else { Vec::new() },
        liquidity_heatmap,
        tick_size,
        multiplier,
    }
}
