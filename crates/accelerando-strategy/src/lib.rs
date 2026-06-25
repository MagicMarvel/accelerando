//! Strategy adapters. Each turns enriched footprints into position changes. Register them with a
//! [`accelerando_core::Registry`].

mod regime_follow;

pub use regime_follow::RegimeFollow;
