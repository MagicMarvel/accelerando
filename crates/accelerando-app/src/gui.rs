//! `--gui`: open the native viewer (`accelerando-gui`) on one result.

use std::sync::Arc;

use accelerando_core::{BacktestResult, DepthHistory, PreparedEventData, Registry};

type MarketHistory = (Arc<PreparedEventData>, DepthHistory);

use crate::RunOptions;

#[cfg(not(feature = "gui"))]
pub(crate) fn view(
    _: Arc<Registry>, _: &RunOptions, _: &str, _: &str, _: BacktestResult, _: Option<MarketHistory>,
) -> Result<(), String> {
    Err("--gui needs accelerando-app built with the `gui` feature".into())
}

/// Shows `result`. Raw trades are shared by the quote fallback, bar switching and tick replay.
/// Reuse the event backtest's trades when available, otherwise load through the app's registry.
#[cfg(feature = "gui")]
pub(crate) fn view(
    registry: Arc<Registry>,
    o: &RunOptions,
    data_path: &str,
    label: &str,
    result: BacktestResult,
    prepared_events: Option<MarketHistory>,
) -> Result<(), String> {
    use accelerando_gui::{aggregate, ticks_from, BarSpec, ViewerOptions};

    let (events, depth) = if let Some((events, depth)) = prepared_events {
        (Ok(events), depth)
    } else {
        match crate::build_source(&registry, o, data_path) {
            Ok(source) => {
                let (events, depth) = accelerando_core::prepare_market_history(source);
                (events.map(Arc::new), depth)
            }
            Err(err) => (Err(err), DepthHistory::default()),
        }
    };
    if let Err(err) = &events {
        eprintln!("Trade data unavailable: {err}");
    }
    let mut options = ViewerOptions {
        title: format!("Accelerando — {label} — {data_path}"),
        bar_spec: BarSpec::parse(o.agg()),
        ..Default::default()
    };
    let events = events.ok();
    if let Some(events) = &events {
        let for_bars = Arc::clone(events);
        let for_replay = Arc::clone(events);
        options.rebar = Some(Arc::new(move |spec: &BarSpec| Ok(aggregate(for_bars.events(), spec))));
        options.ticks = Some(Arc::new(move |from, max| Ok(ticks_from(for_replay.events(), from, max))));
    }
    accelerando_gui::view_with_market_history(result, options, events, depth).map_err(|e| e.to_string())
}
