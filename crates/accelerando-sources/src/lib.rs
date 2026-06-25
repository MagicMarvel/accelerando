//! Data source adapters. Each turns a native feed into an [`OrderFlowEvent`] stream.

mod bookmap_csv;

pub use bookmap_csv::BookmapCsvSource;

use accelerando_core::{Configurable, DataSource, ParamSpec, Params};

/// Build a registered data source by name. User crates can bypass this and construct directly.
pub fn build(name: &str, params: &Params) -> Option<Box<dyn DataSource>> {
    match name {
        "bookmap_csv" => Some(Box::new(BookmapCsvSource::build(params))),
        _ => None,
    }
}

/// The parameter spec of a registered data source, for the hyperopt search space.
pub fn spec(name: &str) -> Option<ParamSpec> {
    match name {
        "bookmap_csv" => Some(BookmapCsvSource::param_spec()),
        _ => None,
    }
}

/// Names of all registered data sources.
pub fn list() -> &'static [&'static str] {
    &["bookmap_csv"]
}
