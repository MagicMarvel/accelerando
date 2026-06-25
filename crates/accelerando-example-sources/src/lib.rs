//! Data source adapters. Each turns a native feed into an [`accelerando_core::OrderFlowEvent`]
//! stream. Register them with a [`accelerando_core::Registry`] by calling
//! [`register_all`].

mod bookmap_csv;

pub use bookmap_csv::BookmapCsvSource;

use accelerando_core::Registry;

/// Register all built-in data sources into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_source::<BookmapCsvSource>("bookmap_csv");
}
