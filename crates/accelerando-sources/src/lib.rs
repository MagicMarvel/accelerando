//! Data source adapters. Each turns a native feed into an [`accelerando_core::OrderFlowEvent`]
//! stream. Register them with a [`accelerando_core::Registry`] (see the `accelerando` facade crate's
//! `default_registry`).

mod bookmap_csv;

pub use bookmap_csv::BookmapCsvSource;
