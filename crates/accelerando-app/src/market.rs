//! Trades and order-book depth for the native viewer, built in one pass over the data file.
//!
//! Depth goes into a chunked [`DepthStore`]: an overview in memory, exact 4-hour chunks on
//! disk, loaded by the viewer for whatever stretch is on screen. With a cache directory the
//! store is kept (`depth_<hash>/`) and reopened instantly next time; without one it lives in a
//! temporary directory for this run. Backtests never read it.

use std::sync::Arc;
use std::time::Instant;

use accelerando_core::{DepthStore, PreparedEventData, Registry};

use crate::RunOptions;

/// Bump when the store format or its contents change.
const STORE_VERSION: u32 = 1;

pub(crate) fn load(
    registry: &Registry,
    o: &RunOptions,
    path: &str,
) -> Result<(Arc<PreparedEventData>, DepthStore), String> {
    let dir = o.cache_dir.as_ref().map(|dir| {
        let key = serde_json::json!({
            "v": STORE_VERSION,
            "file": crate::file_identity(path),
            "source": o.source,
            "from": o.from,
            "to": o.to,
        });
        dir.join(format!("depth_{:016x}", crate::hash_json(&key)))
    });
    if let Some(store) = dir.as_deref().and_then(DepthStore::open) {
        println!("depth cache hit: {}", dir.as_ref().unwrap().display());
        let events = crate::events::load(registry, o, path)?;
        return Ok((Arc::new(events), store));
    }
    println!("reading trades and order book from {path} (cached for the next run) ...");
    let started = Instant::now();
    let (events, store) = DepthStore::build(crate::build_source(registry, o, path)?, dir.clone())?;
    let events = events?;
    crate::events::save(o, path, &events);
    println!(
        "order book ready in {:.0}s{}",
        started.elapsed().as_secs_f64(),
        dir.map_or(String::new(), |d| format!(": {}", d.display()))
    );
    Ok((Arc::new(events), store))
}
