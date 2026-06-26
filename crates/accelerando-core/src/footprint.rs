//! The footprint — one aggregated "bar" carrying its price/volume ladder plus whatever the
//! indicators chose to attach (values, tags, and chart overlays).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Traded volume at a single price level, split by trade aggressor.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Level {
    pub price: f64,
    /// Volume traded by buy aggressors (lifted the ask).
    pub buy_vol: f64,
    /// Volume traded by sell aggressors (hit the bid).
    pub sell_vol: f64,
}

/// One price level in a volume profile overlay.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct VpLevel {
    pub price: f64,
    pub buy_vol: f64,
    pub sell_vol: f64,
}

/// A chart overlay an indicator or strategy wants drawn on the price panel.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Plot {
    /// Color the background band behind this footprint.
    BackgroundBand { color: String, label: String },
    /// A point on a named line overlay (connected across footprints by `id`).
    Line {
        id: String,
        value: f64,
        color: String,
    },
    /// A marker at a price (shape is a free string the front-end understands, e.g. "triangle").
    Marker {
        price: f64,
        shape: String,
        color: String,
        text: String,
        #[serde(default)]
        text_dx: Option<f64>,
        #[serde(default)]
        text_dy: Option<f64>,
    },
    /// ATAS-style volume profile histogram. Left side = delta, right side = total volume.
    /// `id` groups bars into one profile; the web viewer renders only the last occurrence per id.
    /// `span` = how many bars back this profile covers (for background shading).
    VolumeProfile {
        id: String,
        levels: Vec<VpLevel>,
        span: usize,
    },
}

/// One aggregated bar flowing through the pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Footprint {
    pub ts_first_ns: i64,
    pub ts_last_ns: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub trades: u32,
    /// Net aggressor volume (buy - sell).
    pub delta: f64,
    /// Point of control: price level with the most traded volume.
    pub poc: f64,
    /// Price → buy/sell traded volume, ascending by price.
    pub ladder: Vec<Level>,
    /// Numeric indicator outputs (e.g. `"rsi" -> 42.0`).
    pub values: BTreeMap<String, f64>,
    /// Categorical indicator outputs (e.g. `"trend" -> "up"`).
    pub tags: BTreeMap<String, String>,
    /// Indicator-declared chart overlays for this footprint.
    pub plots: Vec<Plot>,
}

impl Footprint {
    /// Create an empty footprint seeded with the first print's price/time.
    pub fn seed(ts_ns: i64, price: f64) -> Self {
        Footprint {
            ts_first_ns: ts_ns,
            ts_last_ns: ts_ns,
            open: price,
            high: price,
            low: price,
            close: price,
            volume: 0.0,
            trades: 0,
            delta: 0.0,
            poc: price,
            ladder: Vec::new(),
            values: BTreeMap::new(),
            tags: BTreeMap::new(),
            plots: Vec::new(),
        }
    }

    /// Fold a trade print into this footprint, updating OHLC, ladder and delta.
    pub fn add_trade(&mut self, ts_ns: i64, price: f64, size: f64, buy: bool, tick_size: f64) {
        self.high = self.high.max(price);
        self.low = self.low.min(price);
        self.close = price;
        self.ts_last_ns = ts_ns;
        self.volume += size;
        self.trades += 1;
        if buy {
            self.delta += size;
        } else {
            self.delta -= size;
        }

        let key = if tick_size > 0.0 {
            (price / tick_size).round()
        } else {
            price
        };
        let idx = self
            .ladder
            .binary_search_by(|l| {
                let lk = if tick_size > 0.0 {
                    (l.price / tick_size).round()
                } else {
                    l.price
                };
                lk.partial_cmp(&key).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or_else(|e| {
                self.ladder.insert(
                    e,
                    Level {
                        price,
                        ..Default::default()
                    },
                );
                e
            });
        if buy {
            self.ladder[idx].buy_vol += size;
        } else {
            self.ladder[idx].sell_vol += size;
        }
    }

    /// Compute the point of control from the ladder. Call once the bar is complete.
    pub fn finalize_ladder(&mut self) {
        let mut best = self.close;
        let mut best_vol = -1.0;
        for l in &self.ladder {
            let v = l.buy_vol + l.sell_vol;
            if v > best_vol {
                best_vol = v;
                best = l.price;
            }
        }
        self.poc = best;
    }
}
