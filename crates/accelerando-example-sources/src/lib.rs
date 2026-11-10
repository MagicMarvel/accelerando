//! Data source adapters. Each turns a native feed into an [`accelerando_core::OrderFlowEvent`]
//! stream. Register them with a [`accelerando_core::Registry`] by calling
//! [`register_all`].

mod csv;
mod numcat_l2;

pub use csv::CsvSource;
pub use numcat_l2::NumCatL2Source;

use accelerando_core::Registry;

/// Register all built-in data sources into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_source::<CsvSource>("csv");
    registry.register_source::<NumCatL2Source>("numcat_l2");
}

