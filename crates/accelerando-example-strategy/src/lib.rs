//! Strategy adapters. Each turns enriched footprints into position changes. Register them with a
//! [`accelerando_core::Registry`].

mod indicator_cross;

pub use indicator_cross::IndicatorCross;

use accelerando_core::Registry;

/// Register all built-in strategies into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_strategy::<IndicatorCross>("indicator_cross");
}
