//! Smoke server for exercising the replay HTTP API by hand or from scripts:
//! `cargo run -p accelerando-web --example replay_smoke` then hit http://localhost:8971.

use accelerando_core::{Footprint, PreparedBacktestData};
use accelerando_web::{AnnotationConfig, ReplayManager};
use std::sync::Arc;

fn main() {
    let footprints: Vec<Footprint> = (0..300)
        .map(|i| {
            let base = 100.0 + (i % 10) as f64 * 0.25;
            Footprint {
                ts_first_ns: i as i64 * 60_000_000_000,
                ts_last_ns: i as i64 * 60_000_000_000 + 59_000_000_000,
                open: base,
                high: base + 1.0,
                low: base - 1.0,
                close: base + 0.5,
                volume: 100.0,
                trades: 10,
                delta: 0.0,
                poc: base,
                ladder: Vec::new(),
                values: Default::default(),
                tags: Default::default(),
                plots: Vec::new(),
            }
        })
        .collect();
    let prepared = Arc::new(PreparedBacktestData {
        footprints,
        liquidity_heatmap: Default::default(),
        tick_size: 0.25,
        multiplier: 2.0,
    });
    let record_dir = std::env::temp_dir().join("accel_replay_smoke_records");
    let ann_path = std::env::temp_dir().join("accel_replay_smoke_ann.jsonl");
    let _ = std::fs::remove_file(&ann_path);
    let replay = ReplayManager::new(prepared, "smoke", &record_dir);
    let port = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(18973);
    accelerando_web::serve_experiment_lazy_heatmap_with_replay(
        Vec::new(),
        port,
        |_id| None,
        |_query| None,
        AnnotationConfig::new(["good", "bad"], ann_path.to_string_lossy()),
        replay,
    )
    .expect("serve");
}
