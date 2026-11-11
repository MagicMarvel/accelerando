//! CSV order-flow source adapter.
//!
//! Supported row formats:
//!   `f,<ts>,-1,REC`                                   file header (ignored)
//!   `c,<ts>,0,<exch>,<sym>,<type>,<tick>,<mult>,<n>`  contract metadata
//!   `T,<ts_ns>,<id>,<price>,<size>,<side>,<flag>`     trade print
//!   `A,<ts_ns>,<id>,<price>,<size>,<side>,...`        resting liquidity added (delta)
//!   `R,<ts_ns>,<id>,<price>,<size>,<side>,...`        resting liquidity reduced (delta)
//!   `r,<ts_ns>,<id>,<side>,<price>,<size>`            absolute book level snapshot/update
//!
//! For trade / `A` / `R` rows `side` is `1` or `2`; which one is the buy aggressor is feed-dependent,
//! so it is exposed as the `buy_aggressor_code` parameter (default `2`).
//!
//! Lowercase `r` rows are absolute depth levels (Bookmap-style MBP feed): each row overwrites the
//! resting size at `price` on `side`, and `size == 0` clears the level. Note these rows use a
//! *different* column order (`side` before `price`) and a *different* side encoding than the
//! aggressor rows, so the bid side code is exposed separately as `bid_book_code` (default `1`).

use std::fs::File;
use std::io::{BufRead, BufReader};

use accelerando_core::{
    Configurable, DataSource, EventInterest, OrderFlowEvent, ProgressHandle,
    Side,
};

/// Streams an [`OrderFlowEvent`] sequence from a CSV file.
#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct CsvSource {
    #[param(fixed, default = "")]
    path: String,
    #[param(fixed, default = 2)]
    buy_aggressor_code: i64,
    #[param(fixed, default = 1)]
    bid_book_code: i64,
    progress: Option<ProgressHandle>,
    event_interest: EventInterest,
}

impl CsvSource {
    fn init(&mut self) {
        self.event_interest = EventInterest::ALL;
    }
}

impl DataSource for CsvSource {
    fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
        let file = File::open(&self.path).unwrap_or_else(|e| panic!("open csv {}: {e}", self.path));
        if let Some(p) = &self.progress {
            if let Ok(md) = file.metadata() {
                p.set_total_bytes(md.len());
            }
        }
        let reader = BufReader::with_capacity(16 * 1024 * 1024, file);
        Box::new(CsvIter {
            reader,
            line: String::with_capacity(128),
            buy_aggressor_code: self.buy_aggressor_code,
            bid_book_code: self.bid_book_code,
            progress: self.progress,
            event_interest: self.event_interest,
        })
    }

    fn set_event_interest(&mut self, interest: EventInterest) {
        self.event_interest = interest;
    }

    fn set_progress(&mut self, progress: ProgressHandle) {
        self.progress = Some(progress);
    }
}

struct CsvIter {
    reader: BufReader<File>,
    line: String,
    buy_aggressor_code: i64,
    bid_book_code: i64,
    progress: Option<ProgressHandle>,
    event_interest: EventInterest,
}

impl Iterator for CsvIter {
    type Item = OrderFlowEvent;

    fn next(&mut self) -> Option<OrderFlowEvent> {
        // Skip rows that don't parse into events (headers, malformed lines).
        loop {
            self.line.clear();
            let bytes = self.reader.read_line(&mut self.line).ok()?;
            if bytes == 0 {
                return None;
            }
            if let Some(p) = &self.progress {
                p.add_bytes(bytes as u64);
            }
            let line = self.line.trim_end_matches(['\r', '\n']);
            if let Some(ev) = self.parse(line) {
                return Some(ev);
            }
        }
    }
}

impl CsvIter {
    fn parse(&self, line: &str) -> Option<OrderFlowEvent> {
        let mut f = line.split(',');
        let kind = f.next()?;
        match kind {
            "T" => self
                .event_interest
                .contains(EventInterest::TRADE)
                .then(|| self.parse_trade_fast(line))
                .flatten(),
            "A" | "AddLimit" => {
                if self.event_interest.contains(EventInterest::L2) {
                    self.parse_l2(f, true)
                } else {
                    None
                }
            }
            "R" | "ReduceLimit" => {
                if self.event_interest.contains(EventInterest::L2) {
                    self.parse_l2(f, false)
                } else {
                    None
                }
            }
            "r" => {
                if self.event_interest.contains(EventInterest::L2) {
                    self.parse_snapshot(f)
                } else {
                    None
                }
            }
            "c" => {
                if !self.event_interest.contains(EventInterest::CONTRACT) {
                    return None;
                }
                // c,ts,0,exch,sym,type,tick,mult,...
                let _ts = f.next();
                let _zero = f.next();
                let _exch = f.next();
                let _sym = f.next();
                let _typ = f.next();
                let tick_size = f.next()?.parse::<f64>().ok()?;
                let multiplier = f.next()?.parse::<f64>().ok().unwrap_or(1.0);
                Some(OrderFlowEvent::Contract {
                    tick_size,
                    multiplier,
                })
            }
            _ => None,
        }
    }

    fn parse_trade_fast(&self, line: &str) -> Option<OrderFlowEvent> {
        let bytes = line.as_bytes();
        if bytes.first().copied()? != b'T' || bytes.get(1).copied()? != b',' {
            return None;
        }

        let mut pos = 2;
        let ts_ns = parse_i64_field(bytes, &mut pos)?;
        skip_field(bytes, &mut pos)?;
        let price = parse_f64_field(bytes, &mut pos)?.filter(|v| v.is_finite())?;
        let size = parse_f64_field(bytes, &mut pos)?.filter(|v| v.is_finite() && *v >= 0.0)?;
        let side_code = parse_i64_field(bytes, &mut pos).unwrap_or(0);
        let aggressor = if side_code == self.buy_aggressor_code {
            Side::Buy
        } else if side_code == 1 || side_code == 2 {
            Side::Sell
        } else {
            Side::Unknown
        };
        Some(OrderFlowEvent::Trade {
            ts_ns,
            price,
            size,
            aggressor,
        })
    }

    fn parse_l2<'a>(
        &self,
        mut f: impl Iterator<Item = &'a str>,
        add: bool,
    ) -> Option<OrderFlowEvent> {
        let ts_ns = f.next()?.parse::<i64>().ok()?;
        let _id = f.next()?;
        let price = f.next()?.parse::<f64>().ok().filter(|v| v.is_finite())?;
        let size = f
            .next()?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)?;
        let side_code = f.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let side = if side_code == self.buy_aggressor_code {
            Side::Buy
        } else {
            Side::Sell
        };
        if add {
            Some(OrderFlowEvent::AddLimit {
                ts_ns,
                price,
                size,
                side,
            })
        } else {
            Some(OrderFlowEvent::ReduceLimit {
                ts_ns,
                price,
                size,
                side,
            })
        }
    }

    /// Parse an absolute book-level snapshot row: `r,<ts>,<id>,<side>,<price>,<size>`.
    ///
    /// `f` is positioned just after the leading `r` token. The column order (`side` then `price`)
    /// and side encoding differ from the aggressor rows, so `bid_book_code` is used here.
    fn parse_snapshot<'a>(&self, mut f: impl Iterator<Item = &'a str>) -> Option<OrderFlowEvent> {
        let ts_ns = f.next()?.parse::<i64>().ok()?;
        let _id = f.next()?;
        let side_code = f.next().and_then(|v| v.parse::<i64>().ok())?;
        let price = f.next()?.parse::<f64>().ok().filter(|v| v.is_finite())?;
        let size = f
            .next()?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)?;
        let side = if side_code == self.bid_book_code {
            Side::Buy
        } else {
            Side::Sell
        };
        Some(OrderFlowEvent::SetLevel {
            ts_ns,
            price,
            size,
            side,
        })
    }
}

fn skip_field(bytes: &[u8], pos: &mut usize) -> Option<()> {
    while *pos < bytes.len() && bytes[*pos] != b',' {
        *pos += 1;
    }
    if *pos < bytes.len() && bytes[*pos] == b',' {
        *pos += 1;
    }
    Some(())
}

fn parse_i64_field(bytes: &[u8], pos: &mut usize) -> Option<i64> {
    let mut sign = 1i64;
    if bytes.get(*pos).copied() == Some(b'-') {
        sign = -1;
        *pos += 1;
    }
    let mut value = 0i64;
    let mut saw_digit = false;
    while *pos < bytes.len() {
        let b = bytes[*pos];
        if !b.is_ascii_digit() {
            break;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as i64)?;
        saw_digit = true;
        *pos += 1;
    }
    if !saw_digit {
        return None;
    }
    if *pos < bytes.len() && bytes[*pos] == b',' {
        *pos += 1;
    }
    Some(value * sign)
}

fn parse_f64_field(bytes: &[u8], pos: &mut usize) -> Option<Option<f64>> {
    let mut sign = 1.0;
    if bytes.get(*pos).copied() == Some(b'-') {
        sign = -1.0;
        *pos += 1;
    }
    let mut value = 0.0;
    let mut saw_digit = false;
    while *pos < bytes.len() {
        let b = bytes[*pos];
        if !b.is_ascii_digit() {
            break;
        }
        value = value * 10.0 + f64::from(b - b'0');
        saw_digit = true;
        *pos += 1;
    }
    if bytes.get(*pos).copied() == Some(b'.') {
        *pos += 1;
        let mut scale = 0.1;
        while *pos < bytes.len() {
            let b = bytes[*pos];
            if !b.is_ascii_digit() {
                break;
            }
            value += f64::from(b - b'0') * scale;
            scale *= 0.1;
            saw_digit = true;
            *pos += 1;
        }
    }
    if !saw_digit {
        return None;
    }
    if *pos < bytes.len() && bytes[*pos] == b',' {
        *pos += 1;
    }
    Some(Some(value * sign))
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use accelerando_core::Params;
    #[test]
    fn csv_preserves_unknown_aggressor_instead_of_inventing_sell_volume() {
        let path = std::env::temp_dir().join(format!(
            "accel-side-{}-{}.csv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "c,0,0,CME,ES,FUTURE,0.25,50,1\nT,1,0,100,2,1,0\nT,2,0,100,3,2,0\nT,3,0,101,4,0,0\n",
        )
        .unwrap();
        let mut p = Params::default();
        p.set(
            "path",
            accelerando_core::ParamValue::Str(path.to_string_lossy().into()),
        );
        p.set("buy_aggressor_code", accelerando_core::ParamValue::Int(1));
        let rows: Vec<_> = Box::new(CsvSource::from_params(&p)).events().collect();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            rows[1],
            OrderFlowEvent::Trade {
                aggressor: Side::Buy,
                ..
            }
        ));
        assert!(matches!(
            rows[2],
            OrderFlowEvent::Trade {
                aggressor: Side::Sell,
                ..
            }
        ));
        assert!(matches!(
            rows[3],
            OrderFlowEvent::Trade {
                aggressor: Side::Unknown,
                ..
            }
        ));
    }
}
