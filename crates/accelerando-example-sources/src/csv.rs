//! CSV order-flow source adapter.
//!
//! Supported row formats:
//!   `f,<ts>,-1,REC`                                   file header (ignored)
//!   `c,<ts>,0,<exch>,<sym>,<type>,<tick>,<mult>,<n>`  contract metadata
//!   `T,<ts_ns>,<id>,<price>,<size>,<side>,<flag>`     trade print
//!
//! `side` is `1` or `2`. Which one is the buy aggressor is feed-dependent, so it is exposed as the
//! `buy_aggressor_code` parameter (default `2`) and can be flipped without touching code.

use std::fs::File;
use std::io::{BufRead, BufReader};

use accelerando_core::{
    Configurable, DataSource, OrderFlowEvent, ParamSpec, Params, ProgressHandle, Side,
};

/// Streams an [`OrderFlowEvent`] sequence from a CSV file.
pub struct CsvSource {
    path: String,
    buy_aggressor_code: i64,
    progress: Option<ProgressHandle>,
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
        }
    }
}

impl DataSource for CsvSource {
    fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
        let file = File::open(&self.path)
            .unwrap_or_else(|e| panic!("open csv {}: {e}", self.path));
        if let Some(p) = &self.progress {
            if let Ok(md) = file.metadata() {
                p.set_total_bytes(md.len());
            }
        }
        let reader = BufReader::with_capacity(16 * 1024 * 1024, file);
        Box::new(CsvIter {
            lines: reader.lines(),
            buy_aggressor_code: self.buy_aggressor_code,
            progress: self.progress,
        })
    }

    fn set_progress(&mut self, progress: ProgressHandle) {
        self.progress = Some(progress);
    }
}

struct CsvIter {
    lines: std::io::Lines<BufReader<File>>,
    buy_aggressor_code: i64,
    progress: Option<ProgressHandle>,
}

impl Iterator for CsvIter {
    type Item = OrderFlowEvent;

    fn next(&mut self) -> Option<OrderFlowEvent> {
        // Skip rows that don't parse into events (headers, malformed lines).
        loop {
            let line = self.lines.next()?.ok()?;
            if let Some(p) = &self.progress {
                // +1 approximates the stripped newline; close enough for a progress bar.
                p.add_bytes(line.len() as u64 + 1);
            }
            if let Some(ev) = self.parse(&line) {
                return Some(ev);
            }
        }
    }
}

impl CsvIter {
    fn parse(&self, line: &str) -> Option<OrderFlowEvent> {
        let mut f = line.split(',');
        match f.next()? {
            "T" => {
                let ts_ns = f.next()?.parse::<i64>().ok()?;
                let _id = f.next()?;
                let price = f.next()?.parse::<f64>().ok().filter(|v| v.is_finite())?;
                let size = f.next()?.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)?;
                let side_code = f.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
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
            "c" => {
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
}
