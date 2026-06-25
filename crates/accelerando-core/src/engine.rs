//! The streaming backtest engine: wires the four stages together and runs them one footprint
//! at a time. This is the hot loop the hyperopt search calls thousands of times.

use crate::broker::{Broker, BrokerConfig, OrderCtx};
use crate::event::OrderFlowEvent;
use crate::footprint::Footprint;
use crate::metrics::Metrics;
use crate::progress::ProgressHandle;
use crate::result::BacktestResult;
use crate::traits::{DataSource, FootprintAggregator, Indicator, Strategy};

/// A fully-assembled, ready-to-run pipeline.
pub struct Pipeline {
    pub source: Box<dyn DataSource>,
    pub aggregator: Box<dyn FootprintAggregator>,
    pub indicators: Vec<Box<dyn Indicator>>,
    pub strategy: Box<dyn Strategy>,
    pub broker_cfg: BrokerConfig,
    /// Keep enriched footprints in the result (set false in hyperopt for speed/memory).
    pub keep_footprints: bool,
}

/// Run a single backtest end to end (no progress reporting).
pub fn run_backtest(pipeline: Pipeline) -> BacktestResult {
    run_backtest_progress(pipeline, None)
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
    } = pipeline;

    let mut source = source;
    if let Some(h) = &progress {
        source.set_progress(h.clone());
    }

    let mut broker = Broker::new(broker_cfg);
    let mut history: Vec<Footprint> = Vec::new();
    let mut last_close = f64::NAN;
    let mut last_ts = 0i64;

    let handle = |fp: Footprint,
                      broker: &mut Broker,
                      history: &mut Vec<Footprint>,
                      indicators: &mut Vec<Box<dyn Indicator>>,
                      strategy: &mut Box<dyn Strategy>| {
        let mut fp = fp;
        // Broker first: fill last bar's intent at this open, check stops, mark equity.
        broker.on_new_footprint(&fp);
        // Indicators enrich the footprint causally.
        for ind in indicators.iter_mut() {
            ind.on_footprint(&mut fp, history);
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
                let _ = aggregator.on_event(&ev);
                continue;
            }
            _ => {
                if let OrderFlowEvent::Trade { price, ts_ns, .. } = ev {
                    last_close = price;
                    last_ts = ts_ns;
                }
                if let Some(fp) = aggregator.on_event(&ev) {
                    last_close = fp.close;
                    last_ts = fp.ts_last_ns;
                    handle(fp, &mut broker, &mut history, &mut indicators, &mut strategy);
                    if let Some(h) = &progress {
                        h.inc_footprints();
                    }
                }
            }
        }
    }
    if let Some(fp) = aggregator.flush() {
        last_close = fp.close;
        last_ts = fp.ts_last_ns;
        handle(fp, &mut broker, &mut history, &mut indicators, &mut strategy);
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
        tick_size,
        multiplier,
    }
}
