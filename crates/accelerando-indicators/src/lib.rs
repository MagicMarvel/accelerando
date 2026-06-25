//! Indicator adapters. Each enriches footprints with values, tags and chart overlays.

mod whitesnake;

pub use whitesnake::{Regime, Whitesnake};

use accelerando_core::{Configurable, Indicator, ParamSpec, Params};

/// Build a registered indicator by name. User crates can construct their own directly.
pub fn build(name: &str, params: &Params) -> Option<Box<dyn Indicator>> {
    match name {
        "whitesnake" => Some(Box::new(Whitesnake::build(params))),
        _ => None,
    }
}

/// The parameter spec of a registered indicator, for the hyperopt search space.
pub fn spec(name: &str) -> Option<ParamSpec> {
    match name {
        "whitesnake" => Some(Whitesnake::param_spec()),
        _ => None,
    }
}

/// Names of all registered indicators.
pub fn list() -> &'static [&'static str] {
    &["whitesnake"]
}
