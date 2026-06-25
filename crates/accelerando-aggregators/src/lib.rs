//! Footprint aggregator adapters. Each folds the order-flow stream into footprints on a
//! different bar boundary (time, price range, tick count, traded volume).

mod time;

pub use time::TimeAggregator;

use accelerando_core::{Configurable, FootprintAggregator, ParamSpec, Params};

/// Build a registered aggregator by name. User crates can construct their own directly.
pub fn build(name: &str, params: &Params) -> Option<Box<dyn FootprintAggregator>> {
    match name {
        "time" => Some(Box::new(TimeAggregator::build(params))),
        _ => None,
    }
}

/// The parameter spec of a registered aggregator, for the hyperopt search space.
pub fn spec(name: &str) -> Option<ParamSpec> {
    match name {
        "time" => Some(TimeAggregator::param_spec()),
        _ => None,
    }
}

/// Names of all registered aggregators.
pub fn list() -> &'static [&'static str] {
    &["time"]
}
