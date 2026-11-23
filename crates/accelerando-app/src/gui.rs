//! `--gui`: open the native viewer (`accelerando-gui`) on one result.

use std::sync::Arc;

use accelerando_core::{BacktestResult, DepthStore, PreparedEventData, Registry};

type MarketHistory = (Arc<PreparedEventData>, DepthStore);

use crate::RunOptions;

#[cfg(not(feature = "gui"))]
pub(crate) fn view(
    _: Arc<Registry>, _: &RunOptions, _: &str, _: &str, _: BacktestResult, _: Option<MarketHistory>, _: bool,
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
    exit_on_close: bool,
) -> Result<(), String> {
    use accelerando_gui::{BarSpec, ViewerOptions};

    let options = ViewerOptions {
        title: format!("Accelerando — {label} — {data_path}"),
        bar_spec: BarSpec::parse(o.agg()),
        ..Default::default()
    };
    let run_options = o.clone();
    let data_path = data_path.to_owned();
    let load = move || {
        prepared_events.map_or_else(|| crate::market::load(&registry, &run_options, &data_path), Ok)
    };
    let opened = if exit_on_close {
        accelerando_gui::view_cli_with_market_loader(result, options, load)
    } else {
        accelerando_gui::view_with_market_loader(result, options, load)
    };
    opened.map_err(|e| e.to_string())
}
