//! Strategy adapters. Each turns enriched footprints into position changes. Register them with a
//! [`accelerando_core::Registry`].

mod regime_follow;

pub use regime_follow::RegimeFollow;

use accelerando_core::Registry;

/// Register all built-in strategies into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_strategy::<RegimeFollow>("regime_follow");
}
