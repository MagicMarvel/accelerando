//! Raw trades (contract + trade events) for the viewer's bar switch / tick replay and for custom
//! modes, with a compact binary disk cache next to the prepared-footprint cache.

use std::path::Path;

use accelerando_core::{prepare_event_data, OrderFlowEvent, PreparedEventData, Registry, Side};

use crate::RunOptions;

const MAGIC: &[u8; 8] = b"ACEVT001";
const RECORD: usize = 1 + 8 + 8 + 8 + 1;

fn cache_path(o: &RunOptions, path: &str) -> Option<std::path::PathBuf> {
    o.cache_dir.as_ref().map(|dir| {
        let key = serde_json::json!({
            "v": 1,
            "file": crate::file_identity(path),
            "source": o.source,
            "from": o.from,
            "to": o.to,
        });
        dir.join(format!("events_{:016x}.bin", crate::hash_json(&key)))
    })
}

/// Store trades read some other way (e.g. while building the depth store) in the cache.
pub(crate) fn save(o: &RunOptions, path: &str, data: &PreparedEventData) {
    if let Some(cp) = cache_path(o, path).filter(|cp| !cp.exists()) {
        write_atomic(&cp, &encode(data.events()));
    }
}

/// Load raw trades for `path`, from the cache when `o.cache_dir` holds a matching one.
pub(crate) fn load(registry: &Registry, o: &RunOptions, path: &str) -> Result<PreparedEventData, String> {
    let cache = cache_path(o, path);
    if let Some(cp) = &cache {
        if let Ok(bytes) = std::fs::read(cp) {
            match decode(&bytes) {
                Ok(events) => {
                    println!("raw-trade cache hit: {}", cp.display());
                    return PreparedEventData::from_events(events);
                }
                Err(e) => eprintln!("raw-trade cache {} unreadable ({e}); re-reading", cp.display()),
            }
        }
    }
    println!("reading raw trades from {path} ...");
    let data = prepare_event_data(crate::build_source(registry, o, path)?)?;
    if let Some(cp) = &cache {
        write_atomic(cp, &encode(data.events()));
    }
    Ok(data)
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("bin.tmp");
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        println!("raw-trade cache written: {}", path.display());
    }
}

fn encode(events: &[OrderFlowEvent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC.len() + events.len() * RECORD);
    out.extend_from_slice(MAGIC);
    let mut put = |tag: u8, ts: i64, a: f64, b: f64, side: u8| {
        out.push(tag);
        out.extend_from_slice(&ts.to_le_bytes());
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
        out.push(side);
    };
    for ev in events {
        match *ev {
            OrderFlowEvent::Contract { tick_size, multiplier } => put(0, 0, tick_size, multiplier, 0),
            OrderFlowEvent::Trade { ts_ns, price, size, aggressor } => {
                let side = match aggressor {
                    Side::Unknown => 0,
                    Side::Buy => 1,
                    Side::Sell => 2,
                };
                put(1, ts_ns, price, size, side)
            }
            // `prepare_event_data` keeps contract + trades only.
            _ => {}
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Result<Vec<OrderFlowEvent>, String> {
    let body = bytes.strip_prefix(MAGIC).ok_or("bad header")?;
    if body.len() % RECORD != 0 {
        return Err("truncated".into());
    }
    let f64_at = |r: &[u8], i: usize| f64::from_le_bytes(r[i..i + 8].try_into().unwrap());
    body.chunks_exact(RECORD)
        .map(|r| {
            let ts = i64::from_le_bytes(r[1..9].try_into().unwrap());
            let (a, b) = (f64_at(r, 9), f64_at(r, 17));
            match (r[0], r[25]) {
                (0, _) => Ok(OrderFlowEvent::Contract { tick_size: a, multiplier: b }),
                (1, side) => Ok(OrderFlowEvent::Trade {
                    ts_ns: ts,
                    price: a,
                    size: b,
                    aggressor: match side {
                        1 => Side::Buy,
                        2 => Side::Sell,
                        _ => Side::Unknown,
                    },
                }),
                (tag, _) => Err(format!("unknown record tag {tag}")),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_trades() {
        let events = vec![
            OrderFlowEvent::Contract { tick_size: 0.25, multiplier: 50.0 },
            OrderFlowEvent::Trade { ts_ns: 1, price: 100.25, size: 3.0, aggressor: Side::Buy },
            OrderFlowEvent::Trade { ts_ns: 2, price: 100.0, size: 1.0, aggressor: Side::Sell },
            OrderFlowEvent::Trade { ts_ns: 2, price: 100.0, size: 2.0, aggressor: Side::Unknown },
        ];
        let bytes = encode(&events);
        assert_eq!(bytes.len(), MAGIC.len() + 4 * RECORD);
        assert_eq!(decode(&bytes).unwrap(), events);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(decode(b"garbage!").is_err());
    }
}
