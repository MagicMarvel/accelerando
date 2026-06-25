//! Strategy adapters. Each turns enriched footprints into position changes.

mod regime_follow;

pub use regime_follow::RegimeFollow;

use accelerando_core::{Configurable, ParamSpec, Params, Strategy};

/// Build a registered strategy by name. User crates can construct their own directly.
pub fn build(name: &str, params: &Params) -> Option<Box<dyn Strategy>> {
    match name {
        "regime_follow" => Some(Box::new(RegimeFollow::build(params))),
        _ => None,
    }
}

/// The parameter spec of a registered strategy, for the hyperopt search space.
pub fn spec(name: &str) -> Option<ParamSpec> {
    match name {
        "regime_follow" => Some(RegimeFollow::param_spec()),
        _ => None,
    }
}

/// Names of all registered strategies.
pub fn list() -> &'static [&'static str] {
    &["regime_follow"]
}
