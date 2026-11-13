//! Chunked, disk-backed order-book history for the native viewer.
//!
//! A full-depth file can hold hundreds of millions of level changes (ESU6, five weeks: 234 M,
//! about 6 GB as one [`DepthHistory`]). [`DepthStore::build`] reads the source once and keeps
//! only three things:
//!
//! * the exact best bid / ask changes (small, always in memory);
//! * an overview: every level's time-weighted mean size per [`DEPTH_OVERVIEW_NS`] bucket, as a
//!   step function, so zoomed-out heatmaps integrate exactly over bucket-aligned spans;
//! * the exact level changes, written to disk in [`DEPTH_CHUNK_NS`] chunks. Each chunk starts
//!   with the full book at its start time, so any run of consecutive chunks loads on its own
//!   ([`DepthStore::load_window`]) and matches the single-pass history exactly.
//!
//! [`DepthStore::open`] reuses a finished store from disk. Nothing here is fed to strategies:
//! backtests keep reading the source directly.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::depth_history::{add_area, append_at_time, DepthHistory, DepthLevelHistory};
use crate::{DataSource, EventInterest, OrderFlowEvent, PreparedEventData, Side};

const SEC_NS: i64 = 1_000_000_000;
/// Length of one exact on-disk chunk.
pub const DEPTH_CHUNK_NS: i64 = 4 * 3600 * SEC_NS;
/// Bucket of the in-memory overview.
pub const DEPTH_OVERVIEW_NS: i64 = 30 * SEC_NS;

const INDEX_FILE: &str = "index.bin";
const INDEX_MAGIC: &[u8; 8] = b"ACDIX001";
const CHUNK_MAGIC: &[u8; 8] = b"ACDCH001";

/// Depth for the viewer: an overview everywhere plus exact chunks loaded on demand.
/// Cheap to clone (shared), so a loader thread can hold one.
#[derive(Clone)]
pub struct DepthStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// Exact quotes; levels are the overview (or the exact history when `chunks` is empty).
    overview: DepthHistory,
    bucket_ns: i64,
    chunks: Vec<Chunk>,
    dir: Option<PathBuf>,
    /// Remove `dir` when the store is dropped (no cache directory was given).
    temporary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Chunk {
    t0: i64,
    t1: i64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let (true, Some(dir)) = (self.temporary, &self.dir) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Default for DepthStore {
    fn default() -> Self {
        DepthHistory::default().into()
    }
}

/// An in-memory history is its own exact overview.
impl From<DepthHistory> for DepthStore {
    fn from(history: DepthHistory) -> Self {
        Self {
            inner: Arc::new(Inner {
                overview: history,
                bucket_ns: 0,
                chunks: Vec::new(),
                dir: None,
                temporary: false,
            }),
        }
    }
}

/// Exact depth for `[from, until)`, loaded from consecutive chunks.
pub struct DepthWindow {
    pub from: i64,
    pub until: i64,
    pub history: DepthHistory,
}

impl DepthWindow {
    pub fn covers(&self, from: i64, until: i64) -> bool {
        self.from <= from && until <= self.until
    }
}

impl DepthStore {
    pub fn is_empty(&self) -> bool {
        self.inner.overview.is_empty()
    }

    /// Exact best bid / ask changes.
    pub fn quote_changes(&self) -> &[(i64, [f64; 2])] {
        self.inner.overview.quote_changes()
    }

    /// Bucket means (exact when [`DepthStore::is_exact`]).
    pub fn overview(&self) -> &DepthHistory {
        &self.inner.overview
    }

    /// Overview bucket length; 0 for an exact in-memory store.
    pub fn overview_bucket_ns(&self) -> i64 {
        self.inner.bucket_ns
    }

    /// True when the overview already is the exact history (nothing to load).
    pub fn is_exact(&self) -> bool {
        self.inner.chunks.is_empty()
    }

    /// Read the chunks needed for `[from, until)`. The window may cover more than asked.
    pub fn load_window(&self, from: i64, until: i64) -> Result<DepthWindow, String> {
        let chunks = &self.inner.chunks;
        let Some(dir) = self.inner.dir.as_ref().filter(|_| !chunks.is_empty()) else {
            return Err("depth store has no chunks to load".into());
        };
        // The last chunk starting at or before `from` holds the book as of `from`: no level
        // changed between its end and the next chunk.
        let first = chunks.partition_point(|c| c.t0 <= from).saturating_sub(1);
        let last = chunks.partition_point(|c| c.t0 < until).saturating_sub(1).max(first);
        let mut history = DepthHistory {
            tick: self.inner.overview.tick,
            levels: BTreeMap::new(),
            quotes: Vec::new(),
        };
        for chunk in &chunks[first..=last] {
            read_chunk(&chunk_path(dir, chunk.t0), &mut history.levels)?;
        }
        Ok(DepthWindow {
            from: if first == 0 { i64::MIN } else { chunks[first].t0 },
            until: chunks.get(last + 1).map_or(i64::MAX, |c| c.t0),
            history,
        })
    }

    /// One pass over `source`: trades for the tape, depth into `dir` (a fresh temporary
    /// directory, removed with the store, when `None`). A source error ends the build.
    pub fn build(
        source: Box<dyn DataSource>,
        dir: Option<PathBuf>,
    ) -> Result<(Result<PreparedEventData, String>, DepthStore), String> {
        Self::build_with(source, dir, DEPTH_CHUNK_NS, DEPTH_OVERVIEW_NS)
    }

    pub fn build_with(
        mut source: Box<dyn DataSource>,
        dir: Option<PathBuf>,
        chunk_ns: i64,
        bucket_ns: i64,
    ) -> Result<(Result<PreparedEventData, String>, DepthStore), String> {
        assert!(chunk_ns > 0 && bucket_ns > 0);
        let temporary = dir.is_none();
        let dir = dir.unwrap_or_else(temp_dir);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("clear {}: {e}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let mut builder = StoreBuilder::new(dir.clone(), chunk_ns, bucket_ns);
        // Remove a half-built temporary directory on any error below.
        let cleanup = |error: String| {
            if temporary {
                let _ = std::fs::remove_dir_all(&dir);
            }
            error
        };
        source.set_event_interest(EventInterest::ALL);
        let mut trades = Vec::new();
        for event in source.try_events() {
            let event = event.map_err(cleanup)?;
            match event {
                OrderFlowEvent::Contract { tick_size, .. } => {
                    if builder.book.is_empty() && tick_size.is_finite() && tick_size > 0.0 {
                        builder.tick = tick_size;
                    }
                    trades.push(event);
                }
                OrderFlowEvent::Trade { .. } => trades.push(event),
                _ => builder.update(event).map_err(cleanup)?,
            }
        }
        let inner = builder.finish(temporary).map_err(cleanup)?;
        Ok((
            PreparedEventData::from_events(trades),
            DepthStore {
                inner: Arc::new(inner),
            },
        ))
    }

    /// Reuse a store [`DepthStore::build`] finished in `dir`. `None` if absent or unreadable.
    pub fn open(dir: &Path) -> Option<DepthStore> {
        let bytes = std::fs::read(dir.join(INDEX_FILE)).ok()?;
        let mut r = Reader::new(&bytes);
        if r.bytes(8)? != INDEX_MAGIC {
            return None;
        }
        let tick = r.f64()?;
        let bucket_ns = r.i64()?;
        let chunks = (0..r.len()?)
            .map(|_| Some(Chunk { t0: r.i64()?, t1: r.i64()? }))
            .collect::<Option<Vec<_>>>()?;
        let mut levels = BTreeMap::new();
        for _ in 0..r.len()? {
            let key = r.i64()?;
            let mut level = DepthLevelHistory::default();
            for _ in 0..r.len()? {
                level.push(r.i64()?, [r.f64()?, r.f64()?]);
            }
            levels.insert(key, level);
        }
        let quotes = (0..r.len()?)
            .map(|_| Some((r.i64()?, [r.f64()?, r.f64()?])))
            .collect::<Option<Vec<_>>>()?;
        if !r.done() || chunks.iter().any(|c| !chunk_path(dir, c.t0).is_file()) {
            return None;
        }
        Some(DepthStore {
            inner: Arc::new(Inner {
                overview: DepthHistory {
                    tick,
                    levels,
                    quotes,
                },
                bucket_ns,
                chunks,
                dir: Some(dir.to_path_buf()),
                temporary: false,
            }),
        })
    }
}

fn temp_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "accelerando-depth-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn chunk_path(dir: &Path, t0: i64) -> PathBuf {
    dir.join(format!("chunk_{t0}.bin"))
}

type ChunkLevels = BTreeMap<i64, Vec<(i64, [f32; 2])>>;

struct StoreBuilder {
    dir: PathBuf,
    chunk_ns: i64,
    bucket_ns: i64,
    tick: f64,
    /// Current resting [bid, ask] per price key (levels with both sides zero are absent).
    book: BTreeMap<i64, [f64; 2]>,
    active: [BTreeSet<i64>; 2],
    quotes: Vec<(i64, [f64; 2])>,
    /// Open chunk: index and its exact changes per price key.
    chunk: Option<(i64, ChunkLevels)>,
    chunks: Vec<Chunk>,
    overview: BTreeMap<i64, DepthLevelHistory>,
    bucket: Option<i64>,
    /// Levels changed in the open bucket: area so far and time it was integrated up to.
    acc: HashMap<i64, ([f64; 2], i64)>,
}

impl StoreBuilder {
    fn new(dir: PathBuf, chunk_ns: i64, bucket_ns: i64) -> Self {
        Self {
            dir,
            chunk_ns,
            bucket_ns,
            tick: 0.25,
            book: BTreeMap::new(),
            active: Default::default(),
            quotes: Vec::new(),
            chunk: None,
            chunks: Vec::new(),
            overview: BTreeMap::new(),
            bucket: None,
            acc: HashMap::new(),
        }
    }

    /// Same level semantics as the single-pass history: absolute or delta size per side,
    /// clamped at zero, unchanged sizes ignored.
    fn update(&mut self, event: OrderFlowEvent) -> Result<(), String> {
        let (ts, price, size, side, absolute) = match event {
            OrderFlowEvent::SetLevel { ts_ns, price, size, side } => (ts_ns, price, size, side, true),
            OrderFlowEvent::AddLimit { ts_ns, price, size, side } => (ts_ns, price, size, side, false),
            OrderFlowEvent::ReduceLimit { ts_ns, price, size, side } => {
                (ts_ns, price, -size, side, false)
            }
            OrderFlowEvent::Contract { .. } | OrderFlowEvent::Trade { .. } => return Ok(()),
        };
        let side = match side {
            Side::Buy => 0,
            Side::Sell => 1,
            Side::Unknown => return Ok(()),
        };
        if !price.is_finite() || !size.is_finite() {
            return Ok(());
        }
        let key = (price / self.tick).round() as i64;
        let previous = self.book.get(&key).copied().unwrap_or([0.0; 2]);
        let mut current = previous;
        current[side] = if absolute { size } else { previous[side] + size }.max(0.0);
        if current == previous {
            return Ok(());
        }
        self.roll(ts)?;

        let bucket_start = self.bucket.unwrap_or(0) * self.bucket_ns;
        let acc = self.acc.entry(key).or_insert(([0.0; 2], bucket_start));
        add_area(&mut acc.0, previous, ts - acc.1);
        acc.1 = ts;

        if current == [0.0; 2] {
            self.book.remove(&key);
        } else {
            self.book.insert(key, current);
        }
        if let Some((_, levels)) = &mut self.chunk {
            let changes = levels.entry(key).or_default();
            let value = current.map(|v| v as f32);
            match changes.last_mut() {
                Some(last) if last.0 == ts => last.1 = value,
                _ => changes.push((ts, value)),
            }
        }

        if current[side] > 0.0 {
            self.active[side].insert(key);
        } else {
            self.active[side].remove(&key);
        }
        let tick = self.tick;
        let prices = [
            self.active[0].last().map_or(f64::NAN, |&k| k as f64 * tick),
            self.active[1].first().map_or(f64::NAN, |&k| k as f64 * tick),
        ];
        if self.quotes.last().is_none_or(|(_, prev)| {
            prev.iter().zip(prices).any(|(&a, b)| a.to_bits() != b.to_bits())
        }) {
            append_at_time(&mut self.quotes, ts, prices);
        }
        Ok(())
    }

    /// Close the chunk and bucket `ts` has moved past, using the book before the update.
    fn roll(&mut self, ts: i64) -> Result<(), String> {
        let chunk = ts.div_euclid(self.chunk_ns);
        if self.chunk.as_ref().is_none_or(|(open, _)| chunk > *open) {
            self.flush_chunk()?;
            let t0 = chunk * self.chunk_ns;
            let seed = self
                .book
                .iter()
                .map(|(&key, size)| (key, vec![(t0, size.map(|v| v as f32))]))
                .collect();
            self.chunk = Some((chunk, seed));
        }
        let bucket = ts.div_euclid(self.bucket_ns);
        match self.bucket {
            Some(open) if bucket > open => {
                self.close_bucket(open);
                self.bucket = Some(bucket);
            }
            None => self.bucket = Some(bucket),
            _ => {}
        }
        Ok(())
    }

    fn close_bucket(&mut self, bucket: i64) {
        let (start, end) = (bucket * self.bucket_ns, (bucket + 1) * self.bucket_ns);
        for (key, (mut area, since)) in self.acc.drain() {
            let now = self.book.get(&key).copied().unwrap_or([0.0; 2]);
            add_area(&mut area, now, end - since);
            let mean = area.map(|a| a / self.bucket_ns as f64);
            let level = self.overview.entry(key).or_default();
            push_value(level, start, mean);
            // From the next bucket on the level holds `now` until it changes again; a change in
            // that bucket replaces this point with the bucket's mean.
            push_value(level, end, now);
        }
    }

    fn flush_chunk(&mut self) -> Result<(), String> {
        let Some((index, levels)) = self.chunk.take() else {
            return Ok(());
        };
        let t0 = index * self.chunk_ns;
        let mut out = Vec::new();
        out.extend_from_slice(CHUNK_MAGIC);
        put_len(&mut out, levels.len());
        for (key, changes) in &levels {
            out.extend_from_slice(&key.to_le_bytes());
            put_len(&mut out, changes.len());
            for (ts, [bid, ask]) in changes {
                out.extend_from_slice(&ts.to_le_bytes());
                out.extend_from_slice(&bid.to_le_bytes());
                out.extend_from_slice(&ask.to_le_bytes());
            }
        }
        write_file(&chunk_path(&self.dir, t0), &out)?;
        self.chunks.push(Chunk {
            t0,
            t1: t0 + self.chunk_ns,
        });
        Ok(())
    }

    fn finish(mut self, temporary: bool) -> Result<Inner, String> {
        if let Some(open) = self.bucket {
            self.close_bucket(open);
        }
        self.flush_chunk()?;
        let mut out = Vec::new();
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&self.tick.to_le_bytes());
        out.extend_from_slice(&self.bucket_ns.to_le_bytes());
        put_len(&mut out, self.chunks.len());
        for chunk in &self.chunks {
            out.extend_from_slice(&chunk.t0.to_le_bytes());
            out.extend_from_slice(&chunk.t1.to_le_bytes());
        }
        put_len(&mut out, self.overview.len());
        for (key, level) in &self.overview {
            out.extend_from_slice(&key.to_le_bytes());
            put_len(&mut out, level.changes().len());
            for (ts, [bid, ask]) in level.changes() {
                out.extend_from_slice(&ts.to_le_bytes());
                out.extend_from_slice(&bid.to_le_bytes());
                out.extend_from_slice(&ask.to_le_bytes());
            }
        }
        put_len(&mut out, self.quotes.len());
        for (ts, [bid, ask]) in &self.quotes {
            out.extend_from_slice(&ts.to_le_bytes());
            out.extend_from_slice(&bid.to_le_bytes());
            out.extend_from_slice(&ask.to_le_bytes());
        }
        // Written last: its presence marks a complete store.
        write_file(&self.dir.join(INDEX_FILE), &out)?;
        Ok(Inner {
            overview: DepthHistory {
                tick: self.tick,
                levels: self.overview,
                quotes: self.quotes,
            },
            bucket_ns: self.bucket_ns,
            chunks: self.chunks,
            dir: Some(self.dir),
            temporary,
        })
    }
}

/// Append a step, skipping values equal to the current one.
fn push_value(level: &mut DepthLevelHistory, ts: i64, value: [f64; 2]) {
    match level.changes().last() {
        Some(&(last_ts, last)) if last_ts != ts && last == value => {}
        None if value == [0.0; 2] => {}
        _ => level.push(ts, value),
    }
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_le_bytes());
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::File::create(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn read_chunk(path: &Path, levels: &mut BTreeMap<i64, DepthLevelHistory>) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let bad = || format!("corrupt depth chunk {}", path.display());
    let mut r = Reader::new(&bytes);
    if r.bytes(8) != Some(CHUNK_MAGIC.as_slice()) {
        return Err(bad());
    }
    for _ in 0..r.len().ok_or_else(bad)? {
        let key = r.i64().ok_or_else(bad)?;
        let level = levels.entry(key).or_default();
        for _ in 0..r.len().ok_or_else(bad)? {
            let ts = r.i64().ok_or_else(bad)?;
            let bid = r.f32().ok_or_else(bad)?;
            let ask = r.f32().ok_or_else(bad)?;
            level.push(ts, [f64::from(bid), f64::from(ask)]);
        }
    }
    if r.done() {
        Ok(())
    } else {
        Err(bad())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.bytes.split_at_checked(n)?;
        self.bytes = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N).map(|b| b.try_into().unwrap())
    }

    fn i64(&mut self) -> Option<i64> {
        self.array().map(i64::from_le_bytes)
    }

    fn f64(&mut self) -> Option<f64> {
        self.array().map(f64::from_le_bytes)
    }

    fn f32(&mut self) -> Option<f32> {
        self.array().map(f32::from_le_bytes)
    }

    /// A count, rejected when it cannot fit in the remaining bytes.
    fn len(&mut self) -> Option<usize> {
        let n = usize::try_from(u64::from_le_bytes(self.array()?)).ok()?;
        (n <= self.bytes.len()).then_some(n)
    }

    fn done(&self) -> bool {
        self.bytes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepare_market_history;

    struct Events(Vec<OrderFlowEvent>);

    impl DataSource for Events {
        fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
            Box::new(self.0.into_iter())
        }
    }

    /// Deterministic pseudo-random book activity around 100.00, with gaps and level clears.
    fn feed() -> Vec<OrderFlowEvent> {
        let mut events = vec![OrderFlowEvent::Contract {
            tick_size: 0.25,
            multiplier: 50.0,
        }];
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut ts = 1_000;
        for i in 0..20_000 {
            ts += 1 + next(40) as i64 + if i % 3_000 == 0 { 5_000 } else { 0 };
            let tick = next(16) as f64;
            let price = 98.0 + tick * 0.25;
            let side = if tick < 8.0 { Side::Buy } else { Side::Sell };
            let size = if next(5) == 0 { 0.0 } else { next(50) as f64 };
            events.push(if next(9) == 0 {
                OrderFlowEvent::Trade { ts_ns: ts, price, size: 1.0, aggressor: side }
            } else {
                OrderFlowEvent::SetLevel { ts_ns: ts, price, size, side }
            });
        }
        events
    }

    fn close(a: [f64; 2], b: [f64; 2]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-9 * (1.0 + y.abs()))
    }

    #[test]
    fn chunks_reproduce_the_single_pass_history_and_overview_buckets_are_exact() {
        let (chunk_ns, bucket_ns) = (50_000, 700);
        let dir = temp_dir();
        let (_, exact) = prepare_market_history(Box::new(Events(feed())));
        let (trades, store) =
            DepthStore::build_with(Box::new(Events(feed())), Some(dir.clone()), chunk_ns, bucket_ns)
                .unwrap();
        assert!(trades.unwrap().events().len() > 1_000);
        assert!(!store.is_exact() && store.inner.chunks.len() > 5);
        assert_eq!(store.quote_changes().len(), exact.quote_changes().len());
        for (a, b) in store.quote_changes().iter().zip(exact.quote_changes()) {
            assert_eq!(a.0, b.0);
            assert!(a.1.iter().zip(b.1).all(|(x, y)| x.to_bits() == y.to_bits()));
        }

        let reopened = DepthStore::open(&dir).expect("finished store reopens");
        let end = exact.quote_changes().last().unwrap().0 + 10_000;
        for (from, until) in [(0, 60_000), (130_000, 260_000), (400_000, end), (75_000, 75_001)] {
            let window = reopened.load_window(from, until).unwrap();
            assert!(window.covers(from, until));
            let size = |depth: &DepthHistory, price: f64, t: i64| {
                depth.resting_size_at(price, t).map_or([0.0; 2], |(_, size)| size)
            };
            for (price, level) in exact.levels_between(90.0, 110.0) {
                let loaded = window.history.levels_between(price, price).next();
                for t in (from..until).step_by(997) {
                    let mean = loaded.map_or([0.0; 2], |(_, l)| l.mean_size(t, t + 1_500));
                    assert!(close(mean, level.mean_size(t, t + 1_500)), "price {price} at {t}");
                    assert_eq!(size(&window.history, price, t), size(&exact, price, t));
                }
            }
        }

        // Bucket-aligned spans of the overview integrate exactly.
        for (price, level) in exact.levels_between(90.0, 110.0) {
            let overview = reopened.overview().levels_between(price, price).next();
            for b in (0..end / bucket_ns).step_by(7) {
                let (from, until) = (b * bucket_ns, (b + 3) * bucket_ns);
                let mean = overview.map_or([0.0; 2], |(_, l)| l.mean_size(from, until));
                assert!(
                    close(mean, level.mean_size(from, until)),
                    "price {price} bucket {b}"
                );
            }
        }
        drop((store, reopened));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temporary_store_is_removed_with_the_last_clone() {
        let (_, store) = DepthStore::build(Box::new(Events(feed())), None).unwrap();
        let dir = store.inner.dir.clone().unwrap();
        let clone = store.clone();
        drop(store);
        assert!(dir.exists());
        drop(clone);
        assert!(!dir.exists());
    }
}
