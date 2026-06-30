//! High-resolution, time-bucketed order-book heatmap storage.
//!
//! Buckets are appended to a binary cache file while a feed is prepared. The server keeps only a
//! lightweight timestamp/offset index in memory and reads visible windows back from disk on demand.

use std::fs::{create_dir_all, remove_file, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Default directory, relative to the process current directory, for heatmap bucket cache files.
pub const DEFAULT_HEATMAP_CACHE_DIR: &str = "heatmap-cache";

/// Shared handle a recording indicator writes into and a web handler reads from.
pub type SharedHeatmap = Arc<Mutex<HiresHeatmap>>;

/// One resting-book level, stored compactly (price as a tick key, sizes as f32).
#[derive(Clone, Copy)]
pub struct CompactLevel {
    pub key: i32,
    pub bid: f32,
    pub ask: f32,
}

/// One time bucket: the book snapshot plus the trade activity that occurred during it.
#[derive(Default)]
pub struct HeatmapBucket {
    pub ts_ns: i64,
    pub last_px: f64,
    pub buy_vol: f64,
    pub sell_vol: f64,
    pub best_bid: f64,
    pub best_ask: f64,
    pub levels: Vec<CompactLevel>,
}

#[derive(Clone, Copy)]
struct BucketIndex {
    ts_ns: i64,
    offset: u64,
}

/// The full high-resolution heatmap history backed by an on-disk bucket cache.
pub struct HiresHeatmap {
    pub interval_ns: i64,
    pub tick_size: f64,
    cache_path: PathBuf,
    remove_on_drop: bool,
    writer: Option<BufWriter<File>>,
    index: Vec<BucketIndex>,
    next_offset: u64,
}

impl Default for HiresHeatmap {
    fn default() -> Self {
        Self::new(default_cache_dir())
    }
}

impl HiresHeatmap {
    /// Create a heatmap cache under `cache_dir`.
    ///
    /// The directory is created lazily before the first bucket is written. The file is removed on a
    /// clean drop by default; if the process is killed, any leftover file remains in `cache_dir`.
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        let cache_dir = cache_dir.into();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cache_path = cache_dir.join(format!(
            "accelerando-heatmap-{}-{stamp}.bin",
            std::process::id()
        ));
        Self {
            interval_ns: 0,
            tick_size: 0.25,
            cache_path,
            remove_on_drop: true,
            writer: None,
            index: Vec::new(),
            next_offset: 0,
        }
    }

    /// Keep the cache file on drop. Useful for debugging or inspecting generated cache size.
    pub fn keep_cache_on_drop(mut self) -> Self {
        self.remove_on_drop = false;
        self
    }

    pub fn bucket_count(&self) -> usize {
        self.index.len()
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    pub fn push_bucket(&mut self, bucket: HeatmapBucket) -> std::io::Result<()> {
        if self.writer.is_none() {
            if let Some(parent) = self.cache_path.parent() {
                create_dir_all(parent)?;
            }
            self.writer = Some(BufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&self.cache_path)?,
            ));
        }
        let encoded = encode_bucket(&bucket);
        let offset = self.next_offset;
        let writer = self.writer.as_mut().expect("heatmap cache writer opened");
        writer.write_all(&encoded)?;
        self.next_offset = self.next_offset.saturating_add(encoded.len() as u64);
        self.index.push(BucketIndex {
            ts_ns: bucket.ts_ns,
            offset,
        });
        Ok(())
    }

    /// Downsample the requested window into a `cols x rows` intensity raster plus per-column traces.
    pub fn window(&mut self, q: &HeatmapQuery) -> HeatmapWindow {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
        let cols = q.cols.clamp(1, 4000);
        let rows = q.rows.clamp(1, 2000);
        let (t0, t1) = (q.t0_ns, q.t1_ns.max(q.t0_ns + 1));
        let (pmin, pmax) = if q.pmax > q.pmin {
            (q.pmin, q.pmax)
        } else {
            (q.pmin, q.pmin + self.tick_size.max(0.25))
        };
        let span_t = (t1 - t0) as f64;
        let bucket_ns = q.bucket_ns.max(self.interval_ns.max(1));
        let span_p = pmax - pmin;

        let mut grid = vec![0f32; cols * rows];
        let mut col_last = vec![-1f32; cols];
        let mut col_bid = vec![-1f32; cols];
        let mut col_ask = vec![-1f32; cols];
        let mut col_buy = vec![0f32; cols];
        let mut col_sell = vec![0f32; cols];

        let lo = self
            .index
            .partition_point(|b| b.ts_ns.saturating_add(bucket_ns) < t0);
        let hi = self.index.partition_point(|b| b.ts_ns <= t1);
        let mut reader = File::open(&self.cache_path)
            .ok()
            .map(|f| BufReader::with_capacity(1 << 20, f));
        if let (Some(reader), Some(first)) = (reader.as_mut(), self.index.get(lo)) {
            let _ = reader.seek(SeekFrom::Start(first.offset));
        }
        let mut used = 0usize;
        for _idx in &self.index[lo..hi] {
            let Some(reader) = reader.as_mut() else {
                break;
            };
            let Some(b) = read_bucket(reader) else {
                continue;
            };
            let start = b.ts_ns.max(t0);
            let end = b.ts_ns.saturating_add(bucket_ns).min(t1);
            if end <= start {
                continue;
            }
            let c0 = (((start - t0) as f64 / span_t) * cols as f64).floor() as isize;
            let c1 = (((end - t0) as f64 / span_t) * cols as f64).ceil() as isize;
            let c0 = c0.clamp(0, cols as isize - 1) as usize;
            let c1 = c1.clamp(c0 as isize + 1, cols as isize) as usize;
            let col_width = (c1 - c0).max(1) as f32;
            for col in c0..c1 {
                if b.last_px > 0.0 {
                    col_last[col] = b.last_px as f32;
                }
                if b.best_bid > 0.0 {
                    col_bid[col] = b.best_bid as f32;
                }
                if b.best_ask > 0.0 {
                    col_ask[col] = b.best_ask as f32;
                }
                col_buy[col] += b.buy_vol as f32 / col_width;
                col_sell[col] += b.sell_vol as f32 / col_width;
            }
            for lv in &b.levels {
                let price = lv.key as f64 * self.tick_size;
                if price < pmin || price > pmax {
                    continue;
                }
                let row = (((pmax - price) / span_p) * rows as f64) as isize;
                let row = row.clamp(0, rows as isize - 1) as usize;
                let v = q.metric.of(lv);
                for col in c0..c1 {
                    let cell = &mut grid[row * cols + col];
                    if v > *cell {
                        *cell = v;
                    }
                }
            }
            used += 1;
        }

        let maxv = grid.iter().copied().fold(0f32, f32::max);
        let denom = (1.0 + maxv).ln();
        let bytes: Vec<u8> = grid
            .iter()
            .map(|&v| {
                if maxv > 0.0 && v > 0.0 {
                    (255.0 * (1.0 + v).ln() / denom).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                }
            })
            .collect();

        HeatmapWindow {
            cols,
            rows,
            t0_ns: t0,
            t1_ns: t1,
            pmin,
            pmax,
            tick: self.tick_size,
            buckets: used,
            bucket_ns,
            grid_b64: base64_encode(&bytes),
            col_last,
            col_bid,
            col_ask,
            col_buy,
            col_sell,
        }
    }
}

impl Drop for HiresHeatmap {
    fn drop(&mut self) {
        let _ = self.writer.take();
        if self.remove_on_drop {
            let _ = remove_file(&self.cache_path);
        }
    }
}

/// A parsed `/api/heatmap` query describing the visible window and target raster size.
pub struct HeatmapQuery {
    pub t0_ns: i64,
    pub t1_ns: i64,
    pub pmin: f64,
    pub pmax: f64,
    pub cols: usize,
    pub rows: usize,
    pub metric: HeatmapMetric,
    /// Requested display bucket width. The server will not go below the recorded interval.
    pub bucket_ns: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HeatmapMetric {
    Total,
    Bid,
    Ask,
}

impl HeatmapMetric {
    fn of(self, lv: &CompactLevel) -> f32 {
        match self {
            HeatmapMetric::Total => lv.bid + lv.ask,
            HeatmapMetric::Bid => lv.bid,
            HeatmapMetric::Ask => lv.ask,
        }
    }
}

/// The downsampled window sent to the browser. `grid_b64` is a `cols*rows` row-major byte raster.
#[derive(Serialize)]
pub struct HeatmapWindow {
    pub cols: usize,
    pub rows: usize,
    pub t0_ns: i64,
    pub t1_ns: i64,
    pub pmin: f64,
    pub pmax: f64,
    pub tick: f64,
    pub buckets: usize,
    pub bucket_ns: i64,
    pub grid_b64: String,
    /// Per-column market traces (length `cols`; `-1` where the column had no data).
    pub col_last: Vec<f32>,
    pub col_bid: Vec<f32>,
    pub col_ask: Vec<f32>,
    pub col_buy: Vec<f32>,
    pub col_sell: Vec<f32>,
}

/// Parse the `/api/heatmap` query string. Returns `None` if required window params are missing.
pub fn parse_heatmap_query(query: &str) -> Option<HeatmapQuery> {
    let mut t0 = None;
    let mut t1 = None;
    let mut pmin = None;
    let mut pmax = None;
    let mut cols = 1200usize;
    let mut rows = 600usize;
    let mut metric = HeatmapMetric::Total;
    let mut bucket_ns = 1_000_000_000i64;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "t0" => t0 = v.parse::<i64>().ok(),
            "t1" => t1 = v.parse::<i64>().ok(),
            "pmin" => pmin = v.parse::<f64>().ok(),
            "pmax" => pmax = v.parse::<f64>().ok(),
            "cols" => {
                if let Ok(n) = v.parse::<usize>() {
                    cols = n;
                }
            }
            "rows" => {
                if let Ok(n) = v.parse::<usize>() {
                    rows = n;
                }
            }
            "bucket_ms" => {
                if let Ok(n) = v.parse::<i64>() {
                    bucket_ns = n.max(1) * 1_000_000;
                }
            }
            "bucket_ns" => {
                if let Ok(n) = v.parse::<i64>() {
                    bucket_ns = n.max(1);
                }
            }
            "metric" => {
                metric = match v {
                    "bid" => HeatmapMetric::Bid,
                    "ask" => HeatmapMetric::Ask,
                    _ => HeatmapMetric::Total,
                }
            }
            _ => {}
        }
    }
    Some(HeatmapQuery {
        t0_ns: t0?,
        t1_ns: t1?,
        pmin: pmin?,
        pmax: pmax?,
        cols,
        rows,
        metric,
        bucket_ns,
    })
}

fn default_cache_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(DEFAULT_HEATMAP_CACHE_DIR)
}

fn encode_bucket(b: &HeatmapBucket) -> Vec<u8> {
    let n = b.levels.len().min(u32::MAX as usize) as u32;
    let mut out = Vec::with_capacity(48 + n as usize * 12);
    out.extend_from_slice(&b.ts_ns.to_le_bytes());
    out.extend_from_slice(&b.last_px.to_le_bytes());
    out.extend_from_slice(&b.buy_vol.to_le_bytes());
    out.extend_from_slice(&b.sell_vol.to_le_bytes());
    out.extend_from_slice(&b.best_bid.to_le_bytes());
    out.extend_from_slice(&b.best_ask.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    for lv in b.levels.iter().take(n as usize) {
        out.extend_from_slice(&lv.key.to_le_bytes());
        out.extend_from_slice(&lv.bid.to_le_bytes());
        out.extend_from_slice(&lv.ask.to_le_bytes());
    }
    out
}

fn read_bucket<R: Read>(r: &mut R) -> Option<HeatmapBucket> {
    let ts_ns = read_i64(r)?;
    let last_px = read_f64(r)?;
    let buy_vol = read_f64(r)?;
    let sell_vol = read_f64(r)?;
    let best_bid = read_f64(r)?;
    let best_ask = read_f64(r)?;
    let n = read_u32(r)? as usize;
    let mut levels = Vec::with_capacity(n);
    for _ in 0..n {
        levels.push(CompactLevel {
            key: read_i32(r)?,
            bid: read_f32(r)?,
            ask: read_f32(r)?,
        });
    }
    Some(HeatmapBucket {
        ts_ns,
        last_px,
        buy_vol,
        sell_vol,
        best_bid,
        best_ask,
        levels,
    })
}

fn read_i64<R: Read>(r: &mut R) -> Option<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).ok()?;
    Some(i64::from_le_bytes(buf))
}

fn read_i32<R: Read>(r: &mut R) -> Option<i32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).ok()?;
    Some(i32::from_le_bytes(buf))
}

fn read_u32<R: Read>(r: &mut R) -> Option<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).ok()?;
    Some(u32::from_le_bytes(buf))
}

fn read_f64<R: Read>(r: &mut R) -> Option<f64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).ok()?;
    Some(f64::from_le_bytes(buf))
}

fn read_f32<R: Read>(r: &mut R) -> Option<f32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).ok()?;
    Some(f32::from_le_bytes(buf))
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
