//! Accelerando facade crate.
//!
//! This crate exposes the shared runtime registry and built-in adapter registration for the
//! downstream applications that embed Accelerando.

pub use accelerando_core::Registry;
pub use accelerando_core::{ParamSpec, ParamValue, Params};

use accelerando_core::Registry as CoreRegistry;

use accelerando_example_aggregators::register_all as register_aggregators;
use accelerando_example_indicators::register_all as register_indicators;
use accelerando_example_sources::register_all as register_sources;
use accelerando_example_strategy::register_all as register_strategies;

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
    registry
}
