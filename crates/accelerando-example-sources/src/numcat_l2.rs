//! NumCat (猫爪数据 numcat.net) Stock & ETF Level 2 (L2) Data Source Adapter.
//!
//! Parses NumCat L2 tick snapshot data into normalized [`accelerando_core::OrderFlowEvent`]s.
//! Supports both offline JSON files (matching NumCat API response format or raw items)
//! and live HTTP API fetch via `numcat.net` API key.

use serde_json::Value;
use std::fs::File;
use std::io::BufReader;

use accelerando_core::{
    Configurable, DataSource, EventInterest, OrderFlowEvent, ProgressHandle,
    Side,
};

/// Data source that yields normalized order flow events from NumCat Stock/ETF L2 snapshots.
#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct NumCatL2Source {
    #[param(fixed, default = "")]
    path: String,
    #[param(fixed, default = "")]
    api_key: String,
    #[param(fixed, default = "510300.SH")]
    symbol: String,
    #[param(fixed, default = "")]
    tradedate: String,
    #[param(fixed, default = 0.001)]
    tick_size: f64,
    #[param(fixed, default = 100.0)]
    multiplier: f64,
    #[param(fixed, default = true)]
    infer_aggressor: bool,
    progress: Option<ProgressHandle>,
    event_interest: EventInterest,
}

impl NumCatL2Source {
    fn init(&mut self) {
        self.event_interest = EventInterest::ALL;
    }
}

impl DataSource for NumCatL2Source {
    fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
        let items = self.fetch_or_read_items();
        Box::new(NumCatL2Iter::new(
            items,
            self.tick_size,
            self.multiplier,
            self.infer_aggressor,
            self.event_interest,
            self.progress,
        ))
    }

    fn set_event_interest(&mut self, interest: EventInterest) {
        self.event_interest = interest;
    }

    fn set_progress(&mut self, progress: ProgressHandle) {
        self.progress = Some(progress);
    }
}

impl NumCatL2Source {
    fn fetch_or_read_items(&self) -> Vec<ParsedL2Item> {
        if !self.path.is_empty() {
            return parse_json_file(&self.path).unwrap_or_default();
        }

        if !self.api_key.is_empty() {
            return fetch_numcat_api(&self.api_key, &self.symbol, &self.tradedate)
                .unwrap_or_default();
        }

        Vec::new()
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct ParsedL2Item {

    pub ts_ns: i64,
    pub price: f64,
    pub vol: f64,
    pub amount: f64,
    pub bids: [(f64, f64); 5],
    pub asks: [(f64, f64); 5],
}

pub struct NumCatL2Iter {
    items: Vec<ParsedL2Item>,
    index: usize,
    tick_size: f64,
    multiplier: f64,
    infer_aggressor: bool,
    event_interest: EventInterest,
    progress: Option<ProgressHandle>,
    sent_contract: bool,
    pending_events: Vec<OrderFlowEvent>,
    pending_index: usize,
    prev_item: Option<ParsedL2Item>,
}

impl NumCatL2Iter {
    pub fn new(
        items: Vec<ParsedL2Item>,
        tick_size: f64,
        multiplier: f64,
        infer_aggressor: bool,
        event_interest: EventInterest,
        progress: Option<ProgressHandle>,
    ) -> Self {
        if let Some(ref p) = progress {
            p.set_total_bytes(items.len() as u64);
        }
        Self {
            items,
            index: 0,
            tick_size,
            multiplier,
            infer_aggressor,
            event_interest,
            progress,
            sent_contract: false,
            pending_events: Vec::new(),
            pending_index: 0,
            prev_item: None,
        }
    }

    fn prepare_next_item_events(&mut self) -> bool {
        if self.index >= self.items.len() {
            return false;
        }

        let curr = self.items[self.index].clone();
        self.index += 1;
        if let Some(ref p) = self.progress {
            p.add_bytes(1);
        }

        self.pending_events.clear();
        self.pending_index = 0;

        // Trade volume delta event
        if self.event_interest.contains(EventInterest::TRADE) && curr.price > 0.0 {
            let delta_vol = if let Some(ref prev) = self.prev_item {
                let same_day = (curr.ts_ns / 86_400_000_000_000) == (prev.ts_ns / 86_400_000_000_000);
                if same_day && curr.vol > prev.vol {
                    curr.vol - prev.vol
                } else if !same_day && curr.vol > 0.0 {
                    curr.vol
                } else {
                    0.0
                }
            } else {
                curr.vol
            };

            if delta_vol > 0.0 {
                let aggressor = if self.infer_aggressor {
                    infer_side(&curr, self.prev_item.as_ref())
                } else {
                    Side::Unknown
                };

                self.pending_events.push(OrderFlowEvent::Trade {
                    ts_ns: curr.ts_ns,
                    price: curr.price,
                    size: delta_vol,
                    aggressor,
                });
            }
        }


        // L2 Depth snapshot events
        if self.event_interest.contains(EventInterest::L2) {
            // Each row is the full top-5 book, so a level from the previous row that is absent
            // now is gone (or fell out of the visible depth). Clear it first; otherwise stale
            // sizes pile up behind the market and corrupt the heatmap and the derived BBO.
            if let Some(prev) = self.prev_item.as_ref() {
                let half_tick = self.tick_size.abs().max(1e-9) * 0.5;
                let gone = |levels: &[(f64, f64); 5], price: f64| {
                    price > 0.0 && !levels.iter().any(|(p, _)| (p - price).abs() < half_tick)
                };
                for (price, _) in prev.bids {
                    if gone(&curr.bids, price) {
                        self.pending_events.push(OrderFlowEvent::SetLevel {
                            ts_ns: curr.ts_ns,
                            price,
                            size: 0.0,
                            side: Side::Buy,
                        });
                    }
                }
                for (price, _) in prev.asks {
                    if gone(&curr.asks, price) {
                        self.pending_events.push(OrderFlowEvent::SetLevel {
                            ts_ns: curr.ts_ns,
                            price,
                            size: 0.0,
                            side: Side::Sell,
                        });
                    }
                }
            }
            for (price, size) in curr.bids {
                if price > 0.0 {
                    self.pending_events.push(OrderFlowEvent::SetLevel {
                        ts_ns: curr.ts_ns,
                        price,
                        size,
                        side: Side::Buy,
                    });
                }
            }
            for (price, size) in curr.asks {
                if price > 0.0 {
                    self.pending_events.push(OrderFlowEvent::SetLevel {
                        ts_ns: curr.ts_ns,
                        price,
                        size,
                        side: Side::Sell,
                    });
                }
            }
        }

        self.prev_item = Some(curr);
        true
    }

}

impl Iterator for NumCatL2Iter {
    type Item = OrderFlowEvent;

    fn next(&mut self) -> Option<OrderFlowEvent> {
        if !self.sent_contract {
            self.sent_contract = true;
            if self.event_interest.contains(EventInterest::CONTRACT) {
                return Some(OrderFlowEvent::Contract {
                    tick_size: self.tick_size,
                    multiplier: self.multiplier,
                });
            }
        }

        loop {
            if self.pending_index < self.pending_events.len() {
                let ev = self.pending_events[self.pending_index];
                self.pending_index += 1;
                return Some(ev);
            }

            if !self.prepare_next_item_events() {
                return None;
            }
        }
    }
}

fn infer_side(curr: &ParsedL2Item, prev: Option<&ParsedL2Item>) -> Side {
    if let Some(prev) = prev {
        if curr.price > prev.price {
            return Side::Buy;
        }
        if curr.price < prev.price {
            return Side::Sell;
        }
    }

    let bid1 = curr.bids[0].0;
    let ask1 = curr.asks[0].0;
    if bid1 > 0.0 && ask1 > 0.0 {
        let mid = (bid1 + ask1) / 2.0;
        if curr.price >= ask1 {
            return Side::Buy;
        }
        if curr.price <= bid1 {
            return Side::Sell;
        }
        if curr.price > mid {
            return Side::Buy;
        }
        if curr.price < mid {
            return Side::Sell;
        }
    }

    Side::Unknown
}

fn parse_json_file(path: &str) -> Result<Vec<ParsedL2Item>, String> {
    let file = File::open(path).map_err(|e| format!("open json file '{path}': {e}"))?;
    let reader = BufReader::new(file);
    let json: Value = serde_json::from_reader(reader)
        .map_err(|e| format!("parse json file '{path}': {e}"))?;
    parse_value_to_items(&json)
}

fn fetch_numcat_api(
    api_key: &str,
    symbol: &str,
    tradedate: &str,
) -> Result<Vec<ParsedL2Item>, String> {
    let url = "https://numcat.net/api";
    let payload = serde_json::json!({
        "apiname": "tick",
        "apikey": api_key,
        "fields": "symbol,time,high,open,close,low,vol,amount,pct_chg,bid1,bid2,bid3,bid4,bid5,bid_vol1,bid_vol2,bid_vol3,bid_vol4,bid_vol5,ask1,ask2,ask3,ask4,ask5,ask_vol1,ask_vol2,ask_vol3,ask_vol4,ask_vol5",
        "params": {
            "type": 1,
            "asset": "etf",
            "symbols": symbol,
            "tradedate": tradedate
        }
    });

    let payload_str = payload.to_string();
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .send_string(&payload_str)
        .map_err(|e| format!("numcat api request failed: {e}"))?;

    let json: Value = serde_json::from_reader(resp.into_reader())
        .map_err(|e| format!("numcat api json decode failed: {e}"))?;


    parse_value_to_items(&json)
}

fn parse_value_to_items(val: &Value) -> Result<Vec<ParsedL2Item>, String> {
    let mut items = Vec::new();

    if let Some(data) = val.get("data") {
        if let (Some(fields_arr), Some(items_arr)) = (
            data.get("fields").and_then(Value::as_array),
            data.get("items").and_then(Value::as_array),
        ) {
            let fields: Vec<String> = fields_arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            for item in items_arr {
                if let Some(row) = item.as_array() {
                    if let Some(parsed) = parse_2d_row(&fields, row) {
                        items.push(parsed);
                    }
                }
            }
            return Ok(items);
        }
    }

    if let Some(arr) = val.as_array() {
        for obj in arr {
            if let Some(parsed) = parse_item_object(obj) {
                items.push(parsed);
            }
        }
        return Ok(items);
    }

    if let Some(items_arr) = val.get("items").and_then(Value::as_array) {
        for obj in items_arr {
            if let Some(parsed) = parse_item_object(obj) {
                items.push(parsed);
            }
        }
        return Ok(items);
    }

    Err("invalid NumCat JSON format".to_string())
}

fn parse_2d_row(fields: &[String], row: &[Value]) -> Option<ParsedL2Item> {
    let get_num = |field: &str| -> f64 {
        fields
            .iter()
            .position(|f| f == field)
            .and_then(|idx| row.get(idx))
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
            .unwrap_or(0.0)
    };

    let time_str = fields
        .iter()
        .position(|f| f == "time")
        .and_then(|idx| row.get(idx))
        .and_then(Value::as_str)
        .unwrap_or("");

    let date_str = fields
        .iter()
        .position(|f| f == "tradedate")
        .and_then(|idx| row.get(idx))
        .and_then(Value::as_str)
        .unwrap_or("");

    let ts_ns = parse_time_ns(date_str, time_str);
    let price = get_num("close");
    let vol = get_num("vol");
    let amount = get_num("amount");

    let bids = [
        (get_num("bid1"), get_num("bid_vol1")),
        (get_num("bid2"), get_num("bid_vol2")),
        (get_num("bid3"), get_num("bid_vol3")),
        (get_num("bid4"), get_num("bid_vol4")),
        (get_num("bid5"), get_num("bid_vol5")),
    ];

    let asks = [
        (get_num("ask1"), get_num("ask_vol1")),
        (get_num("ask2"), get_num("ask_vol2")),
        (get_num("ask3"), get_num("ask_vol3")),
        (get_num("ask4"), get_num("ask_vol4")),
        (get_num("ask5"), get_num("ask_vol5")),
    ];

    Some(ParsedL2Item {
        ts_ns,
        price,
        vol,
        amount,
        bids,
        asks,
    })
}

fn parse_item_object(obj: &Value) -> Option<ParsedL2Item> {
    let get_num = |key: &str| -> f64 {
        obj.get(key)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
            .unwrap_or(0.0)
    };

    let time_str = obj.get("time").and_then(Value::as_str).unwrap_or("");
    let date_str = obj.get("tradedate").and_then(Value::as_str).unwrap_or("");
    let ts_ns = parse_time_ns(date_str, time_str);
    let price = get_num("close");
    let vol = get_num("vol");
    let amount = get_num("amount");

    let bids = [
        (get_num("bid1"), get_num("bid_vol1")),
        (get_num("bid2"), get_num("bid_vol2")),
        (get_num("bid3"), get_num("bid_vol3")),
        (get_num("bid4"), get_num("bid_vol4")),
        (get_num("bid5"), get_num("bid_vol5")),
    ];

    let asks = [
        (get_num("ask1"), get_num("ask_vol1")),
        (get_num("ask2"), get_num("ask_vol2")),
        (get_num("ask3"), get_num("ask_vol3")),
        (get_num("ask4"), get_num("ask_vol4")),
        (get_num("ask5"), get_num("ask_vol5")),
    ];

    Some(ParsedL2Item {
        ts_ns,
        price,
        vol,
        amount,
        bids,
        asks,
    })
}

fn parse_time_ns(date_str: &str, time_str: &str) -> i64 {
    if time_str.is_empty() {
        return 0;
    }

    if let Ok(val) = time_str.parse::<i64>() {
        if val > 1_000_000_000_000_000_000 {
            return val;
        }
        if val > 1_000_000_000_000 {
            return val * 1_000_000;
        }
        if val > 1_000_000_000 {
            return val * 1_000_000_000;
        }
    }

    let parts: Vec<&str> = time_str.split_whitespace().collect();
    let (extracted_date, t_part) = if parts.len() == 2 {
        (Some(parts[0]), parts[1])
    } else {
        (None, parts[0])
    };

    let t_split: Vec<&str> = t_part.split(':').collect();
    if t_split.len() == 3 {
        let hour: i64 = t_split[0].parse().unwrap_or(0);
        let min: i64 = t_split[1].parse().unwrap_or(0);
        let sec_parts: Vec<&str> = t_split[2].split('.').collect();
        let sec: i64 = sec_parts[0].parse().unwrap_or(0);
        let millis: i64 = if sec_parts.len() > 1 {
            let m_str = sec_parts[1];
            if m_str.len() == 3 {
                m_str.parse().unwrap_or(0)
            } else if m_str.len() == 2 {
                m_str.parse::<i64>().unwrap_or(0) * 10
            } else if m_str.len() == 1 {
                m_str.parse::<i64>().unwrap_or(0) * 100
            } else {
                m_str[..3].parse().unwrap_or(0)
            }
        } else {
            0
        };

        let day_secs = hour * 3600 + min * 60 + sec;

        let date_source = extracted_date.or(if !date_str.is_empty() { Some(date_str) } else { None });
        let total_secs = if let Some(d_str) = date_source {
            let clean_d = d_str.replace('-', "");
            if clean_d.len() >= 8 {
                let year: i32 = clean_d[0..4].parse().unwrap_or(1970);
                let month: i32 = clean_d[4..6].parse().unwrap_or(1);
                let day: i32 = clean_d[6..8].parse().unwrap_or(1);
                let utc_secs = days_from_civil(year, month, day) * 86_400 + day_secs - 8 * 3600;
                utc_secs
            } else {
                day_secs - 8 * 3600
            }
        } else {
            day_secs - 8 * 3600
        };

        return (total_secs * 1_000 + millis) * 1_000_000;

    }

    0
}

fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146097 + doe - 719468)
}


#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::Params;

    #[test]
    fn test_numcat_l2_parsing_and_events() {
        let json_str = r#"{
            "code": 200,
            "message": "success",
            "data": {
                "fields": ["symbol", "time", "close", "vol", "amount", "bid1", "bid_vol1", "ask1", "ask_vol1"],
                "items": [
                    ["510300.SH", "2026-07-24 14:30:15.100", 4.200, 1000, 4200.0, 4.199, 500, 4.201, 600],
                    ["510300.SH", "2026-07-24 14:30:15.600", 4.201, 1500, 6300.5, 4.200, 400, 4.202, 700]
                ]
            }
        }"#;

        let temp_file = std::env::temp_dir().join(format!("test_numcat_l2_{}.json", std::process::id()));
        std::fs::write(&temp_file, json_str).unwrap();

        let mut p = Params::default();
        p.set("path", accelerando_core::ParamValue::Str(temp_file.to_string_lossy().into()));
        p.set("tick_size", accelerando_core::ParamValue::Float(0.001));
        p.set("multiplier", accelerando_core::ParamValue::Float(100.0));

        let source = Box::new(NumCatL2Source::from_params(&p));
        let events: Vec<OrderFlowEvent> = source.events().collect();
        let _ = std::fs::remove_file(temp_file);

        assert!(!events.is_empty());
        assert!(matches!(events[0], OrderFlowEvent::Contract { tick_size: 0.001, multiplier: 100.0 }));

        // Verify trade volume delta (1500 - 1000 = 500) and inferred aggressor Buy
        let trades: Vec<&OrderFlowEvent> = events.iter().filter(|e| matches!(e, OrderFlowEvent::Trade { .. })).collect();
        assert_eq!(trades.len(), 2);

        if let OrderFlowEvent::Trade { size, aggressor, price, .. } = trades[1] {
            assert_eq!(*size, 500.0);
            assert_eq!(*price, 4.201);
            assert_eq!(*aggressor, Side::Buy);
        } else {
            panic!("Expected Trade event");
        }

        // The second row's book no longer lists bid 4.199 / ask 4.201, so both must be cleared
        // before the new levels are set.
        let cleared = |price: f64, side: Side| {
            events.iter().any(|e| {
                matches!(*e, OrderFlowEvent::SetLevel { price: p, size, side: s, .. }
                    if (p - price).abs() < 1e-9 && size == 0.0 && s == side)
            })
        };
        assert!(cleared(4.199, Side::Buy));
        assert!(cleared(4.201, Side::Sell));
        assert!(!cleared(4.200, Side::Buy));
    }

    #[test]
    fn test_parse_real_cached_file() {
        let p = "data/numcat_688184_2months_cached.json";
        if std::path::Path::new(p).exists() {
            let items = parse_json_file(p).unwrap();
            println!("Parsed {} items from real cached file", items.len());
            assert!(!items.is_empty());
            let mut params = Params::default();
            params.set("path", accelerando_core::ParamValue::Str(p.into()));
            let source = Box::new(NumCatL2Source::from_params(&params));
            let events: Vec<_> = source.events().collect();
            println!("Yielded {} events", events.len());
            assert!(events.len() > 10);
        }
    }
}

