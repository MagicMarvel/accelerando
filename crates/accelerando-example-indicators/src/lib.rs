//! Indicator adapters. Each enriches footprints with values, tags and chart overlays. Register them
//! with a [`accelerando_core::Registry`].

pub mod adaptive_imbalance;
pub mod big_trades;
pub mod economic_calendar;
pub mod regime;
pub mod stacked_imbalance;
mod technical;
pub mod zigzag;

pub use adaptive_imbalance::AdaptiveImbalance;
pub use big_trades::BigTrades;
pub use economic_calendar::EconomicCalendar;
pub use regime::Regime;
pub use stacked_imbalance::StackedImbalance;
pub use technical::TechnicalIndicators;
pub use zigzag::{Pivot, PivotConfirmation, PivotKind, ZigZag, ZigZagIndicator};

use accelerando_core::Registry;

/// Register all built-in indicators into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_indicator::<AdaptiveImbalance>("adaptive_imbalance");
    registry.register_indicator::<BigTrades>("big_trades");
    registry.register_indicator::<EconomicCalendar>("economic_calendar");
    registry.register_indicator::<Regime>("regime");
    registry.register_indicator::<StackedImbalance>("stacked_imbalance");
    registry.register_indicator::<TechnicalIndicators>("technical");
    registry.register_indicator::<ZigZagIndicator>("zigzag");
}
