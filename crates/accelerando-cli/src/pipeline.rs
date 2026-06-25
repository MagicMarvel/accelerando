//! Assemble a runnable [`Pipeline`] from a [`RunConfig`] (plus optional hyperopt overrides), and
//! build the [`SearchSpace`] from the registered adapters' parameter specs.

use accelerando_core::{BrokerConfig, Params, Pipeline, Registry};
use accelerando_hyperopt::SearchSpace;

use crate::config::{params_from_table, RunConfig};

/// Merge a stage's base (TOML) overrides with the hyperopt-sampled keys for its namespace.
fn stage_overrides(base: &toml::Table, sampled: &Params, ns: &str) -> Params {
    let mut p = params_from_table(base);
    let prefix = format!("{ns}.");
    for (k, v) in &sampled.0 {
        if let Some(local) = k.strip_prefix(&prefix) {
            p.0.insert(local.to_string(), v.clone());
        }
    }
    p
}

/// Build a full pipeline. `sampled` carries namespaced hyperopt overrides (empty for a plain run).
pub fn build_pipeline(
    cfg: &RunConfig,
    sampled: &Params,
    keep_footprints: bool,
    registry: &Registry,
) -> Result<Pipeline, String> {
    let source = registry
        .build_source(&cfg.data.adapter, &stage_overrides(&cfg.data.params, sampled, "source"))
        .ok_or_else(|| format!("unknown data source: {}", cfg.data.adapter))?;

    let aggregator = registry
        .build_aggregator(
            &cfg.aggregator.adapter,
            &stage_overrides(&cfg.aggregator.params, sampled, "aggregator"),
        )
        .ok_or_else(|| format!("unknown aggregator: {}", cfg.aggregator.adapter))?;

    let mut indicators = Vec::new();
    for (i, ind) in cfg.indicator.iter().enumerate() {
        let ns = format!("indicator.{i}");
        let b = registry
            .build_indicator(&ind.adapter, &stage_overrides(&ind.params, sampled, &ns))
            .ok_or_else(|| format!("unknown indicator: {}", ind.adapter))?;
        indicators.push(b);
    }

    let strategy = registry
        .build_strategy(
            &cfg.strategy.adapter,
            &stage_overrides(&cfg.strategy.params, sampled, "strategy"),
        )
        .ok_or_else(|| format!("unknown strategy: {}", cfg.strategy.adapter))?;

    let broker_cfg = BrokerConfig {
        commission_per_contract: cfg.broker.commission_per_contract,
        slippage_ticks: cfg.broker.slippage_ticks,
        starting_equity: cfg.broker.starting_equity,
    };

    Ok(Pipeline {
        source,
        aggregator,
        indicators,
        strategy,
        broker_cfg,
        keep_footprints,
    })
}

/// Build the hyperopt search space from every stage's registered parameter spec.
pub fn build_search_space(cfg: &RunConfig, registry: &Registry) -> Result<SearchSpace, String> {
    let mut space = SearchSpace::new();
    if let Some(s) = registry.source_spec(&cfg.data.adapter) {
        space.add_spec("source", &s);
    }
    if let Some(s) = registry.aggregator_spec(&cfg.aggregator.adapter) {
        space.add_spec("aggregator", &s);
    }
    for (i, ind) in cfg.indicator.iter().enumerate() {
        if let Some(s) = registry.indicator_spec(&ind.adapter) {
            space.add_spec(&format!("indicator.{i}"), &s);
        }
    }
    if let Some(s) = registry.strategy_spec(&cfg.strategy.adapter) {
        space.add_spec("strategy", &s);
    }
    if space.dims.is_empty() {
        return Err("search space is empty (no tunable parameters)".to_string());
    }
    Ok(space)
}
