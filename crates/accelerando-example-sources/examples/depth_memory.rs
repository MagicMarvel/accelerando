//! Compare the GUI's depth storage on a real file: the single in-memory history versus the
//! chunked store (overview in memory, exact chunks on disk).
//!
//! `cargo run --release -p accelerando-example-sources --example depth_memory -- <file> [full|store] [cache dir]`

use std::time::Instant;

use accelerando_core::{prepare_market_history, DepthHistory, DepthStore, Params, Registry};

fn changes(depth: &DepthHistory) -> usize {
    depth
        .levels_between(-1e12, 1e12)
        .map(|(_, level)| level.changes().len())
        .sum()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let params = Params::default().with("path", args[0].as_str());
    let mut registry = Registry::default();
    accelerando_example_sources::register_all(&mut registry);
    let source = registry.build_source("parquet", &params).unwrap();
    let started = Instant::now();
    if args.get(1).map(String::as_str) == Some("full") {
        let (events, depth) = prepare_market_history(source);
        println!(
            "full: {:.1}s, {} trade events, {} depth changes, {} quote changes",
            started.elapsed().as_secs_f64(),
            events.unwrap().events().len(),
            changes(&depth),
            depth.quote_changes().len(),
        );
        return;
    }
    let dir = args.get(2).map(Into::into);
    let (events, store) = DepthStore::build(source, dir).unwrap();
    println!(
        "store build: {:.1}s, {} trade events, overview {} changes, {} quote changes",
        started.elapsed().as_secs_f64(),
        events.unwrap().events().len(),
        changes(store.overview()),
        store.quote_changes().len(),
    );
    let quotes = store.quote_changes();
    let mid = quotes[quotes.len() / 2].0;
    for hours in [1, 6, 24] {
        let started = Instant::now();
        let span = hours * 3_600_000_000_000;
        let window = store.load_window(mid - span / 2, mid + span / 2).unwrap();
        println!(
            "window {hours}h: {:.2}s, {} changes",
            started.elapsed().as_secs_f64(),
            changes(&window.history)
        );
    }
}
