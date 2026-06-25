//! Indicator adapters. Each enriches footprints with values, tags and chart overlays. Register them
//! with a [`accelerando_core::Registry`].

mod whitesnake;

pub use whitesnake::{Regime, Whitesnake};
