//! `--gui`: open the native viewer (`accelerando-gui`) on one result.

use std::sync::Arc;

use accelerando_core::{BacktestResult, Registry};

use crate::RunOptions;

#[cfg(not(feature = "gui"))]
pub(crate) fn view(_: Arc<Registry>, _: &RunOptions, _: &str, _: &str, _: BacktestResult) -> Result<(), String> {
    Err("--gui needs accelerando-app built with the `gui` feature".into())
}

/// Shows `result`. Raw trades (for switching the bar type and for tick replay) are loaded the
/// first time the viewer asks for them, through the app's own registry (so custom sources work)
/// and the binary raw-trade cache.
#[cfg(feature = "gui")]
pub(crate) fn view(
    registry: Arc<Registry>,
    o: &RunOptions,
    data_path: &str,
    label: &str,
    result: BacktestResult,
) -> Result<(), String> {
    use std::sync::OnceLock;

    use accelerando_core::{OrderFlowEvent, PreparedEventData};
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

    let events = Arc::new(LazyEvents {
        registry,
        options: o.clone(),
        path: data_path.to_string(),
        cell: OnceLock::new(),
    });
    let for_bars = Arc::clone(&events);
    let options = ViewerOptions {
        title: format!("Accelerando — {label} — {data_path}"),
        bar_spec: BarSpec::parse(o.agg()),
        rebar: Some(Arc::new(move |spec: &BarSpec| Ok(aggregate(for_bars.get()?, spec)))),
        ticks: Some(Arc::new(move |from: i64, max: usize| {
            Ok(ticks_from(events.get()?, from, max))
        })),
        ..Default::default()
    };
    accelerando_gui::view(result, options).map_err(|e| e.to_string())
}
