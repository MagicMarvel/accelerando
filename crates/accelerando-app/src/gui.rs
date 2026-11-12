//! `--gui`: open the native viewer (`accelerando-gui`) on one result.

use std::sync::Arc;

use accelerando_core::{BacktestResult, PreparedEventData, Registry};

use crate::RunOptions;

#[cfg(not(feature = "gui"))]
pub(crate) fn view(
    _: Arc<Registry>, _: &RunOptions, _: &str, _: &str, _: BacktestResult, _: Option<PreparedEventData>,
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
    prepared_events: Option<PreparedEventData>,
) -> Result<(), String> {
    use std::sync::OnceLock;

    use accelerando_core::OrderFlowEvent;
    use accelerando_gui::{aggregate, ticks_from, BarSpec, ViewerOptions};

    struct LazyEvents {
        registry: Arc<Registry>,
        options: RunOptions,
        path: String,
        cell: OnceLock<Result<PreparedEventData, String>>,
    }

    impl LazyEvents {
        fn get(&self) -> Result<&[OrderFlowEvent], String> {
            self.cell
                .get_or_init(|| crate::events::load(&self.registry, &self.options, &self.path))
                .as_ref()
                .map(|d| d.events())
                .map_err(|e| e.clone())
        }
    }

    let cell = prepared_events.map_or_else(OnceLock::new, |data| OnceLock::from(Ok(data)));
    let events = Arc::new(LazyEvents {
        registry,
        options: o.clone(),
        path: data_path.to_string(),
        cell,
    });
    let for_bars = Arc::clone(&events);
    let for_replay = Arc::clone(&events);
    let options = ViewerOptions {
        title: format!("Accelerando — {label} — {data_path}"),
        bar_spec: BarSpec::parse(o.agg()),
        rebar: Some(Arc::new(move |spec: &BarSpec| Ok(aggregate(for_bars.get()?, spec)))),
        ticks: Some(Arc::new(move |from: i64, max: usize| {
            Ok(ticks_from(for_replay.get()?, from, max))
        })),
        ..Default::default()
    };
    let trade_events = match events.get() {
        Ok(data) => data,
        Err(err) => {
            eprintln!("1-second trade data unavailable: {err}");
            &[]
        }
    };
    accelerando_gui::view_with_trade_quotes(result, options, trade_events).map_err(|e| e.to_string())
}
