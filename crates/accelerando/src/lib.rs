//! Accelerando facade crate.
//!
//! This crate exposes the shared runtime registry and built-in adapter registration for the
//! downstream applications that embed Accelerando.

pub use accelerando_core::Registry;
pub use accelerando_core::{ParamSpec, ParamValue, Params};

use accelerando_core::Registry as CoreRegistry;
use accelerando_hyperopt::{generate_candidates, grid_size, Algo, SearchSpace};

use accelerando_example_aggregators::register_all as register_aggregators;
use accelerando_example_indicators::register_all as register_indicators;
use accelerando_example_sources::register_all as register_sources;
use accelerando_example_strategy::register_all as register_strategies;
use accelerando_orderflow_features::register_all as register_orderflow_features;

/// Build a registry containing all built-in adapters.
///
/// Downstream applications can use this as a starting point, then register their own adapters
/// before building a pipeline.
pub fn default_registry() -> CoreRegistry {
    let mut registry = CoreRegistry::new();
    register_sources(&mut registry);
    register_aggregators(&mut registry);
    register_indicators(&mut registry);
    register_strategies(&mut registry);
    register_orderflow_features(&mut registry);
    registry
}

/// Generate a strategy's complete parameter sets through the framework.
///
/// There is a single generation model instead of separate "grid" and "manual" modes:
///
/// * `assignments` — raw `name=value` pins from the application's CLI. The framework parses,
///   type-checks and validates them against the strategy's `ParamSpec`. A pinned parameter is
///   removed from the search space and carries its pinned value in every returned candidate.
/// * `grid_enabled` — the framework switch over cartesian enumeration. When `true`, every
///   unpinned tunable dimension is grid-expanded (capped at `max_evals`). When `false`, no
///   enumeration happens at all and exactly one parameter set is returned: spec defaults
///   overlaid with the pins.
///
/// Pinning everything (or a strategy with no tunable dimensions) degenerates to a single
/// candidate even with the grid enabled. ParamSpec lookup, hyperopt namespacing, enumeration,
/// namespace removal, validation, and default filling all remain framework responsibilities.
pub fn strategy_param_grid(
    registry: &CoreRegistry,
    strategy: &str,
    assignments: &[String],
    max_evals: usize,
    grid_enabled: bool,
) -> Result<Vec<Params>, String> {
    let spec = registry.strategy_spec(strategy).ok_or_else(|| {
        format!(
            "unknown strategy `{strategy}`; registered strategies: {}",
            registry.strategy_names().join(", ")
        )
    })?;
    let pins = spec.parse_assignments(assignments)?;

    if !grid_enabled {
        return Ok(vec![spec.resolve(&pins)?]);
    }

    let namespace = "strategy";
    let pinned_names: Vec<&str> = pins.0.keys().map(String::as_str).collect();
    let mut space = SearchSpace::new();
    space.add_spec_excluding(namespace, &spec, &pinned_names);
    generate_candidates(&space, Algo::Grid, max_evals, 0)
        .into_iter()
        .map(|candidate| {
            let prefix = format!("{namespace}.");
            let overrides = Params(
                candidate
                    .0
                    .into_iter()
                    .filter_map(|(key, value)| {
                        key.strip_prefix(&prefix)
                            .map(|key| (key.to_string(), value))
                    })
                    .collect(),
            )
            .merged_with(&pins);
            spec.resolve(&overrides)
        })
        .collect()
}

/// Size of the full (uncapped) grid `strategy_param_grid` would sample from, after pins.
/// When it exceeds `max_evals`, the grid is sampled evenly rather than enumerated.
pub fn strategy_grid_size(
    registry: &CoreRegistry,
    strategy: &str,
    assignments: &[String],
) -> Result<u128, String> {
    let spec = registry
        .strategy_spec(strategy)
        .ok_or_else(|| format!("unknown strategy `{strategy}`"))?;
    let pins = spec.parse_assignments(assignments)?;
    let pinned: Vec<&str> = pins.0.keys().map(String::as_str).collect();
    let mut space = SearchSpace::new();
    space.add_spec_excluding("strategy", &spec, &pinned);
    Ok(grid_size(&space))
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::{prepare_backtest_data, DataSource, OrderFlowEvent, Side};
    use accelerando_example_indicators::big_trades::{COUNT, TOTAL_BUY};

    struct TradeTape(Vec<OrderFlowEvent>);

    impl DataSource for TradeTape {
        fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
            Box::new(self.0.into_iter())
        }
    }

    #[test]
    fn built_in_big_trades_attributes_the_trigger_print_to_its_own_bar() {
        let registry = default_registry();
        let trade = |ts_ns, size| OrderFlowEvent::Trade {
            ts_ns,
            price: 100.0,
            size,
            aggressor: Side::Buy,
        };
        let tape = vec![
            trade(0, 15.0),
            trade(100_000, 15.0),
            trade(5_000_000_000, 40.0),
        ];
        for (name, params) in [
            ("time", Params::default().with("bar_secs", 5)),
            ("volume", Params::default().with("volume", 30)),
        ] {
            let bars = prepare_backtest_data(
                Box::new(TradeTape(tape.clone())),
                registry.build_aggregator(name, &params).unwrap(),
                vec![registry
                    .build_indicator("big_trades", &Params::default())
                    .unwrap()],
                false,
                None,
            )
            .footprints;
            assert_eq!(bars.len(), 2, "{name}");
            assert_eq!(bars[0].value(TOTAL_BUY), Some(30.0), "{name}");
            assert_eq!(bars[1].value(TOTAL_BUY), Some(40.0), "{name}");
            assert!(
                bars.iter().all(|bar| bar.value(COUNT) == Some(1.0)),
                "{name}"
            );
        }
    }

    #[test]
    fn strategy_grid_returns_complete_framework_resolved_params() {
        let registry = default_registry();
        let candidates = strategy_param_grid(&registry, "indicator_cross", &[], 3, true).unwrap();

        assert_eq!(candidates.len(), 3);
        assert!(candidates.iter().all(|params| params.get("qty").is_some()));
        assert!(candidates
            .iter()
            .all(|params| params.get("stop_ticks").is_some()));
        assert!(candidates.iter().all(|params| params.get("side").is_some()));
    }

    #[test]
    fn pinned_assignments_hold_across_every_grid_candidate() {
        let registry = default_registry();
        let pins = vec!["side=long_short".to_string()];
        let candidates = strategy_param_grid(&registry, "indicator_cross", &pins, 8, true).unwrap();

        assert!(candidates.len() > 1, "unpinned dimensions still expand");
        assert!(candidates
            .iter()
            .all(|params| params.str("side", "") == "long_short"));
    }

    #[test]
    fn disabled_grid_returns_single_defaults_plus_pins() {
        let registry = default_registry();
        let pins = vec!["side=long_short".to_string()];
        let candidates =
            strategy_param_grid(&registry, "indicator_cross", &pins, 256, false).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].str("side", ""), "long_short");
        // 其余参数保持 ParamSpec 默认值。
        let spec = registry.strategy_spec("indicator_cross").unwrap();
        assert_eq!(
            candidates[0].get("qty"),
            spec.defaults().get("qty"),
            "unpinned parameters stay at their defaults when the grid is off"
        );
    }

    #[test]
    fn invalid_pins_fail_loudly_in_both_grid_states() {
        let registry = default_registry();
        for grid_enabled in [true, false] {
            let err = strategy_param_grid(
                &registry,
                "indicator_cross",
                &["side=diagonal".to_string()],
                4,
                grid_enabled,
            )
            .unwrap_err();
            assert!(err.contains("valid options"), "unexpected error: {err}");
        }
    }
}
