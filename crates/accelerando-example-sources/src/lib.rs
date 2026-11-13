//! Data source adapters. Each turns a native feed into an [`accelerando_core::OrderFlowEvent`]
//! stream. Register them with a [`accelerando_core::Registry`] by calling
//! [`register_all`].

mod numcat_l2;
mod parquet;

pub use numcat_l2::NumCatL2Source;
pub use parquet::ParquetSource;

use accelerando_core::Registry;

/// Register all built-in data sources into `registry`.
pub fn register_all(registry: &mut Registry) {
    registry.register_source::<ParquetSource>("parquet");
    registry.register_source::<NumCatL2Source>("numcat_l2");
}

