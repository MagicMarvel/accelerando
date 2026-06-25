//! Indicator adapters. Each enriches footprints with values, tags and chart overlays. Register them
//! with a [`accelerando_core::Registry`].

mod whitesnake;

pub use whitesnake::{Regime, Whitesnake};

use accelerando_core::Registry;

/// Register all built-in indicators into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_indicator::<Whitesnake>("whitesnake");
}
