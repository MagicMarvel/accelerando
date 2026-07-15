//! A runtime registry of pluggable adapters, keyed by name.
//!
//! This is what lets users extend the framework **without forking it**: depend on `accelerando`,
//! implement the stage traits + [`Configurable`] in your own crate, register them by name, and the
//! same registry can build pipelines, describe parameters, and feed hyperopt search spaces.
//!
//! The registry stores, per stage kind, a builder closure (`&Params -> Box<dyn Stage>`) and a
//! `ParamSpec` constructor. It is `Send + Sync` so it can be shared across rayon hyperopt workers
//! or application-managed backtest threads.

use std::collections::BTreeMap;

use crate::params::{Configurable, ParamSpec, Params};
use crate::traits::{DataSource, FootprintAggregator, Indicator, Strategy};

type Builder<T> = Box<dyn Fn(&Params) -> Box<T> + Send + Sync>;

struct Entry<T: ?Sized> {
    build: Builder<T>,
    spec: fn() -> ParamSpec,
}

impl<T: ?Sized> Entry<T> {
    fn make(&self, params: &Params) -> Box<T> {
        (self.build)(params)
    }
}

/// Holds every registered adapter for the four pluggable stage kinds.
#[derive(Default)]
pub struct Registry {
    sources: BTreeMap<String, Entry<dyn DataSource>>,
    aggregators: BTreeMap<String, Entry<dyn FootprintAggregator>>,
    indicators: BTreeMap<String, Entry<dyn Indicator>>,
    strategies: BTreeMap<String, Entry<dyn Strategy>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    // --- registration -------------------------------------------------------------------------

    /// Register a data source under `name`. `T` must implement [`DataSource`] + [`Configurable`].
    pub fn register_source<T: DataSource + Configurable + 'static>(&mut self, name: &str) {
        self.sources.insert(
            name.to_string(),
            Entry {
                build: Box::new(|p| Box::new(T::build(p)) as Box<dyn DataSource>),
                spec: T::param_spec,
            },
        );
    }

    /// Register a footprint aggregator under `name`.
    pub fn register_aggregator<T: FootprintAggregator + Configurable + 'static>(
        &mut self,
        name: &str,
    ) {
        self.aggregators.insert(
            name.to_string(),
            Entry {
                build: Box::new(|p| Box::new(T::build(p)) as Box<dyn FootprintAggregator>),
                spec: T::param_spec,
            },
        );
    }

    /// Register an indicator under `name`.
    pub fn register_indicator<T: Indicator + Configurable + 'static>(&mut self, name: &str) {
        self.indicators.insert(
            name.to_string(),
            Entry {
                build: Box::new(|p| Box::new(T::build(p)) as Box<dyn Indicator>),
                spec: T::param_spec,
            },
        );
    }

    /// Register a strategy under `name`.
    pub fn register_strategy<T: Strategy + Configurable + 'static>(&mut self, name: &str) {
        self.strategies.insert(
            name.to_string(),
            Entry {
                build: Box::new(|p| Box::new(T::build(p)) as Box<dyn Strategy>),
                spec: T::param_spec,
            },
        );
    }

    // --- construction -------------------------------------------------------------------------

    pub fn build_source(&self, name: &str, p: &Params) -> Option<Box<dyn DataSource>> {
        self.sources.get(name).map(|e| e.make(p))
    }
    pub fn build_aggregator(&self, name: &str, p: &Params) -> Option<Box<dyn FootprintAggregator>> {
        self.aggregators.get(name).map(|e| e.make(p))
    }
    pub fn build_indicator(&self, name: &str, p: &Params) -> Option<Box<dyn Indicator>> {
        self.indicators.get(name).map(|e| e.make(p))
    }
    pub fn build_strategy(&self, name: &str, p: &Params) -> Option<Box<dyn Strategy>> {
        self.strategies.get(name).map(|e| e.make(p))
    }

    // --- introspection (for hyperopt search spaces and app UIs) -------------------------------

    pub fn source_spec(&self, name: &str) -> Option<ParamSpec> {
        self.sources.get(name).map(|e| (e.spec)())
    }
    pub fn aggregator_spec(&self, name: &str) -> Option<ParamSpec> {
        self.aggregators.get(name).map(|e| (e.spec)())
    }
    pub fn indicator_spec(&self, name: &str) -> Option<ParamSpec> {
        self.indicators.get(name).map(|e| (e.spec)())
    }
    pub fn strategy_spec(&self, name: &str) -> Option<ParamSpec> {
        self.strategies.get(name).map(|e| (e.spec)())
    }

    /// Resolve strategy overrides into a complete, validated parameter map.
    pub fn resolve_strategy_params(
        &self,
        name: &str,
        overrides: &Params,
    ) -> Result<Params, String> {
        let spec = self.strategy_spec(name).ok_or_else(|| {
            format!(
                "unknown strategy `{name}`; registered strategies: {}",
                self.strategy_names().join(", ")
            )
        })?;
        spec.resolve(overrides)
    }

    /// Parse `name=value` assignments and resolve them against a strategy's defaults.
    pub fn parse_strategy_params(
        &self,
        name: &str,
        assignments: &[String],
    ) -> Result<Params, String> {
        let spec = self.strategy_spec(name).ok_or_else(|| {
            format!(
                "unknown strategy `{name}`; registered strategies: {}",
                self.strategy_names().join(", ")
            )
        })?;
        let overrides = spec.parse_assignments(assignments)?;
        spec.resolve(&overrides)
    }

    pub fn source_names(&self) -> Vec<&str> {
        self.sources.keys().map(String::as_str).collect()
    }
    pub fn aggregator_names(&self) -> Vec<&str> {
        self.aggregators.keys().map(String::as_str).collect()
    }
    pub fn indicator_names(&self) -> Vec<&str> {
        self.indicators.keys().map(String::as_str).collect()
    }
    pub fn strategy_names(&self) -> Vec<&str> {
        self.strategies.keys().map(String::as_str).collect()
    }
}
