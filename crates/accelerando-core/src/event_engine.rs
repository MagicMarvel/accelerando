//! Opt-in trade-event execution. The existing footprint engine remains unchanged.
use crate::{
    BacktestResult, Broker, BrokerConfig, DataSource, EventInterest, Footprint,
    FootprintAggregator, Indicator, LiquidityHeatmap, Metrics, OrderFlowEvent, Plot, Series,
    Strategy, StrategyOutput,
};
use serde::Serialize;
use std::{collections::BTreeMap, io::Write};

/// Immutable normalized events shared across parameter runs, retaining source order for ties.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedEventData {
    events: Vec<OrderFlowEvent>,
}
impl PreparedEventData {
    pub fn from_events(events: Vec<OrderFlowEvent>) -> Result<Self, String> {
        let mut contract = false;
        let mut last = None;
        let mut trades = 0;
        for ev in &events {
            match *ev {
                OrderFlowEvent::Contract {
                    tick_size,
                    multiplier,
                } => {
                    if !tick_size.is_finite()
                        || tick_size <= 0.0
                        || !multiplier.is_finite()
                        || multiplier <= 0.0
                    {
                        return Err(
                            "event replay requires valid contract tick size and multiplier".into(),
                        );
                    }
                    contract = true;
                }
                OrderFlowEvent::Trade {
                    ts_ns, price, size, ..
                } => {
                    if !contract {
                        return Err("contract metadata must precede trades".into());
                    }
                    if !price.is_finite() || !size.is_finite() || size <= 0.0 {
                        return Err("non-finite price or non-positive trade size".into());
                    }
                    if last.is_some_and(|t| ts_ns < t) {
                        return Err("out-of-order trade timestamps".into());
                    }
                    last = Some(ts_ns);
                    trades += 1;
                }
                _ => {
                    return Err(
                        "prepared event data currently accepts contract and trade events only"
                            .into(),
                    )
                }
            }
        }
        if trades == 0 {
            return Err("event replay requires raw trades, not footprint-only data".into());
        }
        Ok(Self { events })
    }
    pub fn events(&self) -> &[OrderFlowEvent] {
        &self.events
    }
}

pub fn prepare_event_data(mut source: Box<dyn DataSource>) -> Result<PreparedEventData, String> {
    source.set_event_interest(EventInterest::CONTRACT.union(EventInterest::TRADE));
    PreparedEventData::from_events(
        source
            .events()
            .filter(|e| {
                EventInterest::CONTRACT
                    .union(EventInterest::TRADE)
                    .matches(e)
            })
            .collect(),
    )
}

fn diagnostics(
    strategy: &mut dyn Strategy,
    sink: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    for row in strategy.take_diagnostics() {
        if let Some(w) = sink.as_deref_mut() {
            serde_json::to_writer(&mut *w, &row).map_err(|e| e.to_string())?;
            w.write_all(b"\n").map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Display {
    history: Vec<Footprint>,
    plots: Vec<Plot>,
    points: BTreeMap<String, (f64, Option<String>)>,
    series: BTreeMap<String, Series>,
    index: u32,
}
impl Display {
    fn output(&mut self, plots: Vec<Plot>, points: Vec<(String, f64, Option<String>)>) {
        self.plots.extend(plots);
        for (id, value, color) in points {
            self.points.insert(id, (value, color));
        }
    }
    fn finish_bar(&mut self, mut fp: Footprint, keep: bool) {
        fp.plots.append(&mut self.plots);
        for (id, (value, color)) in std::mem::take(&mut self.points) {
            fp.values.insert(id.clone(), value);
            self.series
                .entry(id.clone())
                .or_insert_with(|| Series {
                    id,
                    color: color.unwrap_or_default(),
                    points: vec![],
                })
                .points
                .push((self.index, value));
        }
        if keep {
            self.history.push(fp);
        }
        self.index += 1;
    }
}

/// Trade exits -> event strategy -> immediate market execution. Completed bars are delivered
/// before the boundary print, with explicit availability and completeness metadata. Signals
/// emitted from on_footprint are queued until that boundary print; the bar's OHLC never fills them.
pub fn run_prepared_event_backtest(
    data: &PreparedEventData,
    aggregator: Box<dyn FootprintAggregator>,
    indicators: Vec<Box<dyn Indicator>>,
    strategy: Box<dyn Strategy>,
    cfg: BrokerConfig,
    keep: bool,
    sink: Option<&mut dyn Write>,
) -> Result<BacktestResult, String> {
    run_event_iterator(
        data.events().iter().copied(),
        aggregator,
        indicators,
        strategy,
        cfg,
        keep,
        sink,
    )
}

/// Low-memory equivalent of the prepared event API. For single runs, consume the source
/// incrementally instead of retaining its trade tape. Signal and fill logic are identical.
pub fn run_event_backtest(
    mut source: Box<dyn DataSource>,
    aggregator: Box<dyn FootprintAggregator>,
    indicators: Vec<Box<dyn Indicator>>,
    strategy: Box<dyn Strategy>,
    cfg: BrokerConfig,
    keep: bool,
    sink: Option<&mut dyn Write>,
) -> Result<BacktestResult, String> {
    source.set_event_interest(EventInterest::CONTRACT.union(EventInterest::TRADE));
    let events = source.events().filter(|e| {
        EventInterest::CONTRACT
            .union(EventInterest::TRADE)
            .matches(e)
    });
    run_event_iterator(events, aggregator, indicators, strategy, cfg, keep, sink)
}

fn run_event_iterator(
    events: impl IntoIterator<Item = OrderFlowEvent>,
    mut aggregator: Box<dyn FootprintAggregator>,
    mut indicators: Vec<Box<dyn Indicator>>,
    mut strategy: Box<dyn Strategy>,
    cfg: BrokerConfig,
    keep: bool,
    mut sink: Option<&mut dyn Write>,
) -> Result<BacktestResult, String> {
    if !strategy.event_interest().contains(EventInterest::TRADE) {
        return Err("event runner requires a trade-event strategy".into());
    }
    let mut broker = Broker::new(cfg);
    let mut display = Display::default();
    let mut indicator_history = Vec::new();
    let mut last_trade = None;
    let mut deferred = None;
    let mut contract_seen = false;
    let mut validation_last = None;
    for event in events {
        let ev = &event;
        match *ev {
            OrderFlowEvent::Contract {
                tick_size,
                multiplier,
            } => {
                if !tick_size.is_finite()
                    || tick_size <= 0.0
                    || !multiplier.is_finite()
                    || multiplier <= 0.0
                {
                    return Err("invalid contract metadata".into());
                }
            }
            OrderFlowEvent::Trade {
                ts_ns, price, size, ..
            } => {
                if !contract_seen {
                    return Err("contract metadata must precede trades".into());
                }
                if !price.is_finite() || !size.is_finite() || size <= 0.0 {
                    return Err("invalid trade price or size".into());
                }
                if validation_last.is_some_and(|last| ts_ns < last) {
                    return Err("out-of-order trade timestamps".into());
                }
                validation_last = Some(ts_ns);
            }
            _ => return Err("unsupported event class".into()),
        }
        match *ev {
            OrderFlowEvent::Contract {
                tick_size,
                multiplier,
            } => {
                if contract_seen {
                    if let Some((ts, price)) = last_trade {
                        broker.finalize(price, ts);
                    }
                    if let Some(mut fp) = aggregator.flush() {
                        fp.tags.insert("minute_complete".into(), "false".into());
                        display.finish_bar(fp, keep);
                    }
                    indicator_history.clear();
                    last_trade = None;
                }
                broker.set_contract(tick_size, multiplier);
                aggregator.on_event(ev);
                for ind in &mut indicators {
                    ind.on_event(ev);
                }
                let mut output = StrategyOutput::new(keep);
                strategy.on_event(ev, &broker.portfolio_snapshot(), &mut output);
                // Contract callbacks are lifecycle notifications, not fill opportunities.
                let (plots, points) = broker.apply_strategy_output(output);
                display.output(plots, points);
                contract_seen = true;
            }
            OrderFlowEvent::Trade { ts_ns, price, .. } => {
                if let Some(mut fp) = aggregator.on_event(ev) {
                    broker.sample_event_equity(fp.ts_last_ns, fp.close);
                    fp.tags.insert("minute_complete".into(), "true".into());
                    fp.tags.insert("available_at_ns".into(), ts_ns.to_string());
                    for ind in &mut indicators {
                        ind.on_footprint(&mut fp, &indicator_history);
                    }
                    let mut output = StrategyOutput::new(keep);
                    strategy.on_footprint(&fp, &broker.portfolio_snapshot(), &mut output);
                    if !indicators.is_empty() {
                        indicator_history.push(fp.clone());
                    }
                    display.finish_bar(fp, keep);
                    deferred = Some(output);
                }
                let before = broker.trades.len();
                broker.on_trade_event(ts_ns, price);
                for trade in &broker.trades[before..] {
                    strategy.on_trade_closed(trade);
                }
                for ind in &mut indicators {
                    if ind.event_interest().matches(ev) {
                        ind.on_event(ev);
                    }
                }
                if let Some(output) = deferred.take() {
                    let (plots, points) = broker.apply_event_output(output, ts_ns, price)?;
                    display.output(plots, points);
                }
                let mut output = StrategyOutput::new(keep);
                strategy.on_event(ev, &broker.portfolio_snapshot(), &mut output);
                let (plots, points) = broker.apply_event_output(output, ts_ns, price)?;
                display.output(plots, points);
                if let Some(pos) = broker
                    .open_position_info()
                    .filter(|p| p.entry_ts_ns == ts_ns)
                {
                    // Source sequence disambiguates fills at equal timestamps in the trade log.
                    if let Some(w) = sink.as_deref_mut() {
                        let risk = pos.stop.map(|s| (pos.entry_px - s).abs());
                        let rr = risk
                            .filter(|r| *r > 0.0)
                            .zip(pos.target)
                            .map(|(r, t)| (t - pos.entry_px).abs() / r);
                        writeln!(
                            w,
                            "{}",
                            serde_json::json!({"event":"position_snapshot", "ts_ns":ts_ns,
                            "entry_price":pos.entry_px, "stop":pos.stop, "target":pos.target,
                            "risk":risk,"actual_reward_risk":rr})
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
                last_trade = Some((ts_ns, price));
            }
            _ => unreachable!("validated event data"),
        }
        diagnostics(strategy.as_mut(), &mut sink)?;
    }
    if validation_last.is_none() {
        return Err("event replay requires raw trades".into());
    }
    // Do not pass partial data to indicators or the strategy as a completed minute.
    if let Some(mut fp) = aggregator.flush() {
        fp.tags.insert("minute_complete".into(), "false".into());
        broker.sample_event_equity(fp.ts_last_ns, fp.close);
        display.finish_bar(fp, keep);
    }
    strategy.on_end_of_data();
    diagnostics(strategy.as_mut(), &mut sink)?;
    if let Some((ts, price)) = last_trade {
        broker.finalize(price, ts);
    }
    // Legacy Broker trade logs contain exit fees only; event API reports both entry and exit.
    for trade in &mut broker.trades {
        trade.pnl -= cfg.commission_per_contract * trade.qty;
    }
    if let Some(w) = sink.as_deref_mut() {
        for trade in &broker.trades {
            writeln!(
                w,
                "{}",
                serde_json::json!({"event":"completed_trade", "trade":trade})
            )
            .map_err(|e| e.to_string())?;
        }
        w.flush().map_err(|e| e.to_string())?;
    }
    let tick_size = broker.tick_size();
    let multiplier = broker.multiplier();
    Ok(BacktestResult {
        metrics: Metrics::compute(cfg.starting_equity, &broker.trades, &broker.equity),
        trades: broker.trades,
        equity: broker.equity,
        footprints: display.history,
        liquidity_heatmap: LiquidityHeatmap::default(),
        series: display.series.into_values().collect(),
        tick_size,
        multiplier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryIntent, TradeReason, TradeSide};
    fn output(stop: f64) -> StrategyOutput {
        let mut o = StrategyOutput::new(false);
        o.orders
            .replace(EntryIntent::market(TradeSide::Long, 1.0).risk_reward_bracket(stop, 1.5));
        o
    }
    #[test]
    fn event_broker_rounds_target_after_slippage_and_exits_on_later_print() {
        let mut b = Broker::new(BrokerConfig {
            slippage_ticks: 1.0,
            ..Default::default()
        });
        b.set_contract(1.0, 10.0);
        b.on_trade_event(1, 100.0);
        b.apply_event_output(output(98.0), 1, 100.0).unwrap();
        assert_eq!(b.open_position_info().unwrap().target, Some(106.0));
        assert!(b.trades.is_empty());
        b.on_trade_event(1, 108.0);
        assert_eq!(b.trades[0].exit_px, 106.0);
        assert_eq!(b.trades[0].reason, TradeReason::TakeProfit);
    }
    #[test]
    fn invalid_stop_is_rejected_and_no_fee_is_charged() {
        let mut b = Broker::new(BrokerConfig::default());
        b.set_contract(1.0, 10.0);
        assert!(b.apply_event_output(output(101.0), 1, 100.0).is_err());
        assert!(b.open_position_info().is_none());
    }
    #[test]
    fn raw_data_requires_metadata_and_monotonic_time_but_allows_ties() {
        let t = |ts| OrderFlowEvent::Trade {
            ts_ns: ts,
            price: 100.0,
            size: 1.0,
            aggressor: crate::Side::Buy,
        };
        let c = OrderFlowEvent::Contract {
            tick_size: 1.0,
            multiplier: 10.0,
        };
        assert!(PreparedEventData::from_events(vec![t(1)]).is_err());
        assert!(PreparedEventData::from_events(vec![c]).is_err());
        assert!(PreparedEventData::from_events(vec![c, t(2), t(1)]).is_err());
        assert!(PreparedEventData::from_events(vec![c, t(1), t(1)]).is_ok());
    }
}
