//! Measure how fast `ParquetSource` streams a file.
//!
//! `cargo run --release -p accelerando-example-sources --example parquet_scan -- <file> [trades|all] [start] [end]`
//!
//! `start` / `end` accept what the source's `start` / `end` parameters accept, e.g. `2026-08-20`
//! or `"2026-08-20 13:30"` (UTC).

use std::time::Instant;

use accelerando_core::{EventInterest, OrderFlowEvent, Params, Registry};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: parquet_scan <file> [trades|all] [start] [end]");
        std::process::exit(2);
    };
    let interest = match args.get(1).map(String::as_str) {
        Some("trades") => EventInterest::CONTRACT.union(EventInterest::TRADE),
        _ => EventInterest::ALL,
    };
    let mut params = Params::default().with("path", path.as_str());
    if let Some(start) = args.get(2) {
        params = params.with("start", start.as_str());
    }
    if let Some(end) = args.get(3) {
        params = params.with("end", end.as_str());
    }
    let mut registry = Registry::default();
    accelerando_example_sources::register_all(&mut registry);
    let mut source = registry.build_source("parquet", &params).expect("parquet source");
    source.set_event_interest(interest);

    let started = Instant::now();
    let (mut trades, mut levels) = (0u64, 0u64);
    let (mut first, mut last) = (None, 0i64);
    for event in source.try_events() {
        let ts = match event.unwrap_or_else(|e| panic!("{e}")) {
            OrderFlowEvent::Trade { ts_ns, .. } => {
                trades += 1;
                ts_ns
            }
            OrderFlowEvent::SetLevel { ts_ns, .. } => {
                levels += 1;
                ts_ns
            }
            _ => continue,
        };
        first.get_or_insert(ts);
        last = ts;
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "trades {trades}, depth {levels}, ts {:?}..{last}, {secs:.2}s ({:.1} M events/s)",
        first,
        (trades + levels) as f64 / secs / 1e6
    );
}
