//! CSV order-flow source adapter.
//!
//! Supported row formats:
//!   `f,<ts>,-1,REC`                                   file header (ignored)
//!   `c,<ts>,0,<exch>,<sym>,<type>,<tick>,<mult>,<n>`  contract metadata
//!   `T,<ts_ns>,<id>,<price>,<size>,<side>,<flag>`     trade print
//!   `A,<ts_ns>,<id>,<price>,<size>,<side>,...`        resting liquidity added
//!   `R,<ts_ns>,<id>,<price>,<size>,<side>,...`        resting liquidity reduced
//!
//! `side` is `1` or `2`. Which one is the buy aggressor is feed-dependent, so it is exposed as the
//! `buy_aggressor_code` parameter (default `2`) and can be flipped without touching code.

use std::fs::File;
use std::io::{BufRead, BufReader};

use accelerando_core::{
    Configurable, DataSource, EventInterest, OrderFlowEvent, ParamSpec, Params, ProgressHandle,
    Side,
};

/// Streams an [`OrderFlowEvent`] sequence from a CSV file.
pub struct CsvSource {
    path: String,
    buy_aggressor_code: i64,
    progress: Option<ProgressHandle>,
    event_interest: EventInterest,
}

impl Configurable for CsvSource {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .fixed_str("path", "")
            .fixed_int("buy_aggressor_code", 2)
    }

    fn from_params(params: &Params) -> Self {
        Self {
            path: params.str("path", ""),
            buy_aggressor_code: params.int("buy_aggressor_code", 2),
            progress: None,
            event_interest: EventInterest::ALL,
        }
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
            "T" => {
                self.event_interest
                    .contains(EventInterest::TRADE)
                    .then(|| self.parse_trade_fast(line))
                    .flatten()
            }
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
        } else {
            Side::Sell
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
