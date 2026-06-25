//! Accelerando facade crate.
//!
//! This crate exposes the shared runtime registry and built-in adapter registration for the
//! command-line tools and downstream adopters.

pub use accelerando_core::Registry;
pub use accelerando_core::{ParamSpec, ParamValue, Params};

use accelerando_core::Registry as CoreRegistry;

use accelerando_example_aggregators::register_all as register_aggregators;
use accelerando_example_indicators::register_all as register_indicators;
use accelerando_example_sources::register_all as register_sources;
use accelerando_example_strategy::register_all as register_strategies;

/// Build a registry containing all built-in adapters.
///
/// This is the shared entry point for CLI and studio to discover the available adapter names
/// and parameter specs.
pub fn default_registry() -> CoreRegistry {
    let mut registry = CoreRegistry::new();
    register_sources(&mut registry);
    register_aggregators(&mut registry);
    register_indicators(&mut registry);
    register_strategies(&mut registry);
    registry
}
