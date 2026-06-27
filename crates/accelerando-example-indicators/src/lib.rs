//! Indicator adapters. Each enriches footprints with values, tags and chart overlays. Register them
//! with a [`accelerando_core::Registry`].

pub mod adaptive_imbalance;
pub mod big_trades;
mod technical;

pub use adaptive_imbalance::AdaptiveImbalance;
pub use big_trades::BigTrades;
pub use technical::TechnicalIndicators;

use accelerando_core::Registry;

/// Register all built-in indicators into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_indicator::<AdaptiveImbalance>("adaptive_imbalance");
    registry.register_indicator::<BigTrades>("big_trades");
    registry.register_indicator::<TechnicalIndicators>("technical");
}
