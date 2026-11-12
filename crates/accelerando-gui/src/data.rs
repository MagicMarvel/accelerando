//! Pure data preparation for the viewer — no egui types, so everything here is unit-testable.
//!
//! * [`TimeAxis`] maps nanosecond timestamps onto a compressed x axis measured in seconds, with
//!   closed-market gaps (lunch break, overnight, weekends) removed.
//! * [`BookGrid`] resamples the liquidity snapshots onto that axis at one cell per second and
//!   derives the best bid / best ask per second.
//! * [`smooth`] is the zero-lag smoother applied to the bid/ask lines.
//! * [`rasterize`] turns the visible part of the grid into a heat raster.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, VecDeque};

use accelerando_core::result::LiquiditySnapshot;
use accelerando_core::{OrderFlowEvent, Side};

pub const SEC_NS: i64 = 1_000_000_000;

/// Marker in [`BookGrid::snap_at`] for "no book known in this second".
pub const NO_SNAPSHOT: u32 = u32::MAX;

/// One contiguous stretch of market activity. `x0` and the length are whole seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub t0_ns: i64,
    pub t1_ns: i64,
    pub x0: f64,
}

impl Segment {
    pub fn len_secs(&self) -> f64 {
        ((self.t1_ns - self.t0_ns) / SEC_NS) as f64
    }
}

/// Compressed time axis: x is "seconds of recorded activity since the start of the data".
#[derive(Clone, Debug, Default)]
pub struct TimeAxis {
    pub segments: Vec<Segment>,
    total: f64,
}

impl TimeAxis {
    /// Build from `[start, end]` activity intervals (any order). Intervals closer than
    /// `max_gap_ns` are merged into one segment; larger gaps are cut out of the axis.
    pub fn build(mut intervals: Vec<(i64, i64)>, max_gap_ns: i64) -> Self {
        for (a, b) in intervals.iter_mut() {
            *a = a.div_euclid(SEC_NS) * SEC_NS;
            let end = (*b).max(*a);
            *b = (end.div_euclid(SEC_NS) + 1) * SEC_NS;
        }
        intervals.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (a, b) in intervals {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(max_gap_ns) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let mut segments = Vec::with_capacity(merged.len());
        let mut x = 0.0;
        for (t0_ns, t1_ns) in merged {
            let seg = Segment { t0_ns, t1_ns, x0: x };
            x += seg.len_secs();
            segments.push(seg);
        }
        Self { segments, total: x }
    }

    /// Total axis length in seconds.
    pub fn total(&self) -> f64 {
        self.total
    }

    /// Timestamp → x. Timestamps inside a removed gap map to the gap position.
    pub fn to_x(&self, ts_ns: i64) -> f64 {
        let idx = self.segments.partition_point(|s| s.t0_ns <= ts_ns);
        let Some(seg) = idx.checked_sub(1).and_then(|i| self.segments.get(i)) else {
            return 0.0;
        };
        if ts_ns >= seg.t1_ns {
            seg.x0 + seg.len_secs()
        } else {
            seg.x0 + (ts_ns - seg.t0_ns) as f64 / SEC_NS as f64
        }
    }

    /// x → timestamp (clamped to the data range).
    pub fn to_ts(&self, x: f64) -> i64 {
        let idx = self.segments.partition_point(|s| s.x0 <= x);
        let Some(seg) = idx.checked_sub(1).and_then(|i| self.segments.get(i)) else {
            return self.segments.first().map_or(0, |s| s.t0_ns);
        };
        let ts = seg.t0_ns + ((x - seg.x0) * SEC_NS as f64) as i64;
        ts.min(seg.t1_ns)
    }

    /// `[start, end)` of every segment in whole grid seconds.
    pub fn bounds(&self) -> Vec<(usize, usize)> {
        self.segments
            .iter()
            .map(|s| {
                let start = s.x0 as usize;
                (start, start + s.len_secs() as usize)
            })
            .collect()
    }
}

/// Bar-slot axis: every footprint occupies one equal-width slot (x in bars), whatever its
/// duration. A pause between two bars longer than the gap threshold (overnight, weekend, lunch)
/// becomes an empty spacer instead of being drawn over.
#[derive(Clone, Debug, Default)]
pub struct BarAxis {
    starts: Vec<f64>,
    t0: Vec<i64>,
    t1: Vec<i64>,
    /// `(spacer start x, spacer end x, first timestamp after it)`.
    gaps: Vec<(f64, f64, i64)>,
    total: f64,
}

impl BarAxis {
    /// `bars` are `(first, last)` timestamps in chronological order.
    pub fn build(bars: &[(i64, i64)], max_gap_ns: i64, spacer: f64) -> Self {
        let mut axis = Self::default();
        let mut x = 0.0;
        for (i, &(a, b)) in bars.iter().enumerate() {
            if i > 0 && a.saturating_sub(bars[i - 1].1) > max_gap_ns {
                axis.gaps.push((x, x + spacer, a));
                x += spacer;
            }
            axis.starts.push(x);
            axis.t0.push(a);
            axis.t1.push(b.max(a));
            x += 1.0;
        }
        axis.total = x;
        axis
    }

    pub fn total(&self) -> f64 {
        self.total
    }

    /// Timestamp → x. Inside a bar the slot is filled linearly; between bars the timestamp maps
    /// to the end of the earlier bar.
    pub fn to_x(&self, ts_ns: i64) -> f64 {
        let i = self.t1.partition_point(|&t| t < ts_ns);
        if i == self.t1.len() {
            return self.total;
        }
        if ts_ns >= self.t0[i] {
            let len = (self.t1[i] - self.t0[i]).max(1) as f64;
            self.starts[i] + ((ts_ns - self.t0[i]) as f64 / len).clamp(0.0, 1.0)
        } else if i == 0 {
            0.0
        } else {
            self.starts[i - 1] + 1.0
        }
    }

    /// x → timestamp (inside a spacer: the end of the bar before it).
    pub fn to_ts(&self, x: f64) -> i64 {
        let Some(i) = self.starts.partition_point(|&s| s <= x).checked_sub(1) else {
            return self.t0.first().copied().unwrap_or(0);
        };
        let f = (x - self.starts[i]).clamp(0.0, 1.0);
        self.t0[i] + ((self.t1[i] - self.t0[i]) as f64 * f) as i64
    }
}

/// Which mapping the chart's x axis uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisMode {
    /// x = seconds of market activity (gaps cut); needed for the 1-second heatmap.
    Time,
    /// x = bar slots; every K-line is the same width.
    Bars,
}

/// The chart x axis in either mode.
#[derive(Clone, Debug)]
pub enum XMap {
    Time(TimeAxis),
    Bars(BarAxis),
}

impl XMap {
    pub fn mode(&self) -> AxisMode {
        match self {
            XMap::Time(_) => AxisMode::Time,
            XMap::Bars(_) => AxisMode::Bars,
        }
    }
    pub fn total(&self) -> f64 {
        match self {
            XMap::Time(a) => a.total(),
            XMap::Bars(a) => a.total(),
        }
    }
    pub fn to_x(&self, ts_ns: i64) -> f64 {
        match self {
            XMap::Time(a) => a.to_x(ts_ns),
            XMap::Bars(a) => a.to_x(ts_ns),
        }
    }
    pub fn to_ts(&self, x: f64) -> i64 {
        match self {
            XMap::Time(a) => a.to_ts(x),
            XMap::Bars(a) => a.to_ts(x),
        }
    }
    /// Session starts: `(empty-space start x, session start x, session start timestamp)`.
    /// In time mode the empty space has zero width (the gap is cut).
    pub fn sessions(&self) -> Vec<(f64, f64, i64)> {
        match self {
            XMap::Time(a) => a.segments.iter().map(|s| (s.x0, s.x0, s.t0_ns)).collect(),
            XMap::Bars(a) => {
                let mut out: Vec<(f64, f64, i64)> =
                    a.t0.first().map(|&t| (0.0, 0.0, t)).into_iter().collect();
                out.extend(a.gaps.iter().copied());
                out
            }
        }
    }
}

/// True when a session start lies in `(xa, xb]` — a line between the two points would be drawn
/// across closed-market time. `starts` is the sorted session start x of [`XMap::sessions`].
pub fn crosses_session(starts: &[f64], xa: f64, xb: f64) -> bool {
    let i = starts.partition_point(|&s| s <= xa);
    starts.get(i).is_some_and(|&s| s <= xb)
}

/// Which resting size the heat color encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeatMetric {
    Total,
    Bid,
    Ask,
}

/// Directed trades aggregated to a one-second bucket.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TradeQuote {
    pub ts_ns: i64,
    bid: f32,
    ask: f32,
    volume: TradeVolume,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TradeVolume {
    pub buy: f32,
    pub sell: f32,
    notional: f64,
}

impl TradeVolume {
    fn total(self) -> f64 {
        f64::from(self.buy) + f64::from(self.sell)
    }
}

/// Estimate ask from the highest aggressive buy and bid from the lowest aggressive sell.
pub(crate) fn trade_quotes(events: &[OrderFlowEvent]) -> Vec<TradeQuote> {
    let mut quotes: Vec<TradeQuote> = Vec::new();
    for event in events {
        let OrderFlowEvent::Trade { ts_ns, price, size, aggressor } = *event else { continue };
        if aggressor == Side::Unknown || !price.is_finite() || !size.is_finite() || size <= 0.0 {
            continue;
        }
        let second = ts_ns.div_euclid(SEC_NS) * SEC_NS;
        if quotes.last().is_none_or(|last| last.ts_ns != second) {
            quotes.push(TradeQuote {
                ts_ns: second,
                bid: f32::NAN,
                ask: f32::NAN,
                volume: TradeVolume::default(),
            });
        }
        let quote = quotes.last_mut().expect("current trade bucket");
        quote.volume.notional += price * size;
        match aggressor {
            Side::Buy => {
                quote.ask = quote.ask.max(price as f32);
                quote.volume.buy += size as f32;
            }
            Side::Sell => {
                quote.bid = quote.bid.min(price as f32);
                quote.volume.sell += size as f32;
            }
            Side::Unknown => unreachable!("unknown trades are skipped"),
        }
    }
    quotes
}

/// One visible column shared by the bid/ask lines and trade bubbles.
pub(crate) struct QuoteColumn {
    pub x: f64,
    pub prices: [f64; 2],
    pub bubble: Option<TradeBubble>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TradeBubble {
    pub x: f64,
    pub price: f64,
    pub buy: f64,
    pub sell: f64,
}

impl TradeBubble {
    pub fn total(self) -> f64 {
        self.buy + self.sell
    }

    pub fn clustering_volume(self, delta: bool) -> f64 {
        if delta { (self.buy - self.sell).abs() } else { self.total() }
    }

    fn merge(self, other: Self) -> Self {
        let weight = other.total() / (self.total() + other.total());
        Self {
            x: self.x + (other.x - self.x) * weight,
            price: self.price + (other.price - self.price) * weight,
            buy: self.buy + other.buy,
            sell: self.sell + other.sell,
        }
    }
}

/// The Bookmap Smart slider maps to a nonlinear multiplier, with extra reach at the top.
pub(crate) fn smart_factor(strength: u32) -> f64 {
    let x = f64::from(strength) / 100.0 * 2.1465;
    (1.0 + ((x - 1.0) * 1.37).tan() / 5.0) * x.powf(0.3)
        + 10.0 * (x - 1.8).max(0.0).powi(2)
}

pub(crate) fn bubble_size_target(size: u32) -> f64 {
    ((f64::from(size) / 100.0 * 0.8 + 0.2).powi(2) * 100.0).trunc()
}

fn volume_power(volume: f64, spherical: bool) -> f64 {
    if spherical { volume.cbrt() } else { volume.sqrt() }
}

pub(crate) fn bubble_radius(volume: f64, largest: f64, target: f64, spherical: bool) -> f64 {
    (target * volume_power(volume / largest, spherical)).max(3.0)
}

pub(crate) fn bubble_alpha(radius: f64, transparency: u32) -> u8 {
    let fade = (f64::from(transparency) * 0.01).powf(1.5);
    let fraction = ((2.0 * radius).min(40.0) - 6.0) / 34.0;
    (255.0 * (1.0 - fade * fraction)).round().clamp(0.0, 255.0) as u8
}

/// The original algorithm scores nearby pairs in two passes. Use stable pair IDs and an
/// x-axis spatial index to keep the same order at a given zoom without shuffling the input.
struct BubbleIndex {
    points: Vec<Option<TradeBubble>>,
    buckets: HashMap<i64, Vec<usize>>,
    pixels: [f64; 2],
    origin: f64,
    reach: f64,
}

impl BubbleIndex {
    fn new(pixels: [f64; 2], origin: f64, reach: f64) -> Self {
        Self { points: Vec::new(), buckets: HashMap::new(), pixels, origin, reach }
    }

    fn bucket(&self, x: f64) -> i64 {
        (x * self.pixels[0] / self.reach).floor() as i64
    }

    fn insert(&mut self, point: TradeBubble) -> usize {
        let id = self.points.len();
        self.points.push(Some(point));
        self.buckets.entry(self.bucket(point.x)).or_default().push(id);
        id
    }

    fn remove(&mut self, id: usize) -> TradeBubble {
        let point = self.points[id].take().expect("active bubble");
        let bucket = self.bucket(point.x);
        let ids = self.buckets.get_mut(&bucket).expect("indexed bubble");
        let slot = ids.iter().position(|&other| other == id).expect("indexed bubble ID");
        ids.swap_remove(slot);
        point
    }

    fn merge(&mut self, a: usize, b: usize) -> Option<usize> {
        self.points[a]?;
        self.points[b]?;
        let mut merged = self.remove(a).merge(self.remove(b));
        // Bookmap rounds a merged center to the nearest horizontal screen pixel.
        let pixel = ((merged.x - self.origin) * self.pixels[0] + 0.5).floor();
        merged.x = self.origin + pixel / self.pixels[0];
        Some(self.insert(merged))
    }

    fn nearest(&self, id: usize, limit: usize, radius: f64) -> Vec<(f64, usize)> {
        let point = self.points[id].expect("active bubble");
        let bucket = self.bucket(point.x);
        let mut candidates = Vec::new();
        for key in bucket - 1..=bucket + 1 {
            let Some(ids) = self.buckets.get(&key) else { continue };
            for &other in ids {
                if other == id {
                    continue;
                }
                let candidate = self.points[other].expect("indexed bubble");
                let dx = (point.x - candidate.x) * self.pixels[0];
                let dy = (point.price - candidate.price) * self.pixels[1];
                let distance_sq = dx * dx + dy * dy;
                if distance_sq < radius * radius {
                    candidates.push((distance_sq, other));
                }
            }
        }
        let order = |a: &(f64, usize), b: &(f64, usize)| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1));
        if candidates.len() > limit {
            candidates.select_nth_unstable_by(limit, order);
            candidates.truncate(limit);
        }
        candidates.sort_unstable_by(order);
        candidates
    }
}

#[derive(Clone, Copy)]
struct Scored<T> {
    score: f64,
    value: T,
}

impl<T: Ord> Ord for Scored<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score).then(self.value.cmp(&other.value))
    }
}
impl<T: Ord> PartialOrd for Scored<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl<T: Ord> PartialEq for Scored<T> {
    fn eq(&self, other: &Self) -> bool { self.cmp(other).is_eq() }
}
impl<T: Ord> Eq for Scored<T> {}

#[derive(Clone, Copy)]
pub(crate) struct BubbleClustering {
    pub strength: u32,
    pub target: f64,
    pub spherical: bool,
    pub delta: bool,
}

fn overlap_score(distance: f64, a: f64, b: f64, factor: f64) -> f64 {
    let larger = (factor * a.max(b)).max(0.1);
    let smaller = (factor * a.min(b)).max(0.1);
    let min_radius = 3.0 * factor;
    if distance < 2.0 * min_radius {
        -1.0 / distance
    } else if distance < min_radius * (larger + smaller) / smaller {
        (distance - min_radius) / larger
    } else {
        distance / (larger + smaller)
    }
}

fn score_neighbors(
    index: &BubbleIndex,
    id: usize,
    factor: f64,
    options: BubbleClustering,
    pairs: &mut BinaryHeap<Reverse<Scored<(usize, usize)>>>,
) {
    let radius = |id: usize| {
        let volume = index.points[id].expect("active bubble").clustering_volume(options.delta);
        volume_power(volume, options.spherical) / 2.0
    };
    for (distance_sq, other) in index.nearest(id, 10, index.reach) {
        let score = overlap_score(distance_sq.sqrt(), radius(id), radius(other), factor);
        pairs.push(Reverse(Scored { score, value: (id, other) }));
    }
}

pub(crate) fn cluster_bubbles(
    bubbles: impl Iterator<Item = TradeBubble>,
    pixels_per_unit: [f64; 2],
    origin: f64,
    options: BubbleClustering,
) -> Vec<TradeBubble> {
    let factor = smart_factor(options.strength);
    if factor == 0.0 {
        return bubbles.collect();
    }

    let close = 6.0 * factor;
    let mut index = BubbleIndex::new(pixels_per_unit, origin, factor * (2.0 * options.target + 6.0));
    for bubble in bubbles {
        index.insert(bubble);
    }
    let mut close_pairs = VecDeque::new();
    let nearest_close = |index: &BubbleIndex, id| {
        index.nearest(id, 1, close).first().map(|&(_, other)| (id, other))
    };
    close_pairs.extend((0..index.points.len()).filter_map(|id| nearest_close(&index, id)));
    while let Some((a, b)) = close_pairs.pop_front() {
        if let Some(id) = index.merge(a, b) {
            close_pairs.extend(nearest_close(&index, id));
        }
    }

    let mut pairs = BinaryHeap::new();
    let mut volumes = BinaryHeap::new();
    for (id, point) in index.points.iter().enumerate() {
        if let Some(bubble) = point {
            volumes.push(Scored { score: bubble.clustering_volume(options.delta), value: id });
            score_neighbors(&index, id, factor, options, &mut pairs);
        }
    }
    while let Some(Reverse(pair)) = pairs.pop() {
        while volumes.peek().is_some_and(|entry| index.points[entry.value].is_none()) {
            volumes.pop();
        }
        let Some(largest) = volumes.peek() else { break };
        let scale = 2.0 * options.target / volume_power(largest.score, options.spherical);
        if pair.score > scale {
            break;
        }
        if let Some(id) = index.merge(pair.value.0, pair.value.1) {
            let bubble = index.points[id].expect("merged bubble");
            volumes.push(Scored { score: bubble.clustering_volume(options.delta), value: id });
            score_neighbors(&index, id, factor, options, &mut pairs);
        }
    }
    let mut result: Vec<_> = index.points.into_iter().flatten().collect();
    result.sort_by(|a, b| a.x.total_cmp(&b.x));
    result
}

/// Merge completed seconds into fixed-width columns anchored at each time segment's start.
/// Include one extra column at either edge so lines reach the visible boundaries.
pub(crate) fn quote_columns(
    bounds: &[(usize, usize)],
    prices: [&[f32]; 2],
    trade_volume: &[TradeVolume],
    visible: std::ops::Range<usize>,
    step: usize,
) -> Vec<Vec<QuoteColumn>> {
    let n = prices[0].len().min(prices[1].len());
    let start = visible.start.saturating_sub(step);
    let stop = visible.end.saturating_add(step).min(n);
    let first = bounds.partition_point(|&(_, end)| end <= start);
    bounds[first..]
        .iter()
        .take_while(|&&(a, _)| a < stop)
        .map(|&(a, b)| {
            let first_column = a + start.saturating_sub(a) / step * step;
            let end = b.min(n);
            (first_column..end.min(stop))
                .step_by(step)
                .map(|cell| {
                    let end = cell.saturating_add(step).min(end);
                    let mut sums = [0.0; 2];
                    let mut counts = [0; 2];
                    let (mut buy, mut sell, mut time_sum, mut notional) = (0.0, 0.0, 0.0, 0.0);
                    for i in cell..end {
                        for (side, price) in prices.iter().map(|side| side[i]).enumerate() {
                            if price.is_finite() {
                                sums[side] += price as f64;
                                counts[side] += 1;
                            }
                        }
                        if let Some(volume) = trade_volume.get(i) {
                            buy += f64::from(volume.buy);
                            sell += f64::from(volume.sell);
                            time_sum += (i as f64 + 0.5) * volume.total();
                            notional += volume.notional;
                        }
                    }
                    let total = buy + sell;
                    QuoteColumn {
                        x: cell as f64 + (end - cell) as f64 * 0.5,
                        prices: std::array::from_fn(|side| sums[side] / counts[side] as f64),
                        bubble: (total > 0.0).then(|| TradeBubble {
                            x: time_sum / total,
                            price: notional / total,
                            buy,
                            sell,
                        }),
                    }
                })
                .collect()
        })
        .collect()
}

/// The order book resampled to one cell per second of the compressed axis.
#[derive(Clone, Debug, Default)]
pub struct BookGrid {
    /// Per second: index of the snapshot in force, or [`NO_SNAPSHOT`].
    pub snap_at: Vec<u32>,
    /// Per second: best bid / best ask (`NaN` where the side is empty or unknown).
    pub bid: Vec<f32>,
    pub ask: Vec<f32>,
}

impl BookGrid {
    /// Forward-fill `snapshots` (sorted by time) onto the 1-second grid of `axis`. A snapshot is
    /// never carried across a removed gap into the next segment.
    pub fn build(axis: &TimeAxis, snapshots: &[LiquiditySnapshot]) -> Self {
        let n = axis.total() as usize;
        let mut grid = Self {
            snap_at: vec![NO_SNAPSHOT; n],
            bid: vec![f32::NAN; n],
            ask: vec![f32::NAN; n],
        };
        let bbo: Vec<(f32, f32)> = snapshots.iter().map(best_bid_ask).collect();
        let mut next = 0usize;
        for seg in &axis.segments {
            let start = seg.x0 as usize;
            let len = seg.len_secs() as usize;
            while next < snapshots.len() && snapshots[next].ts_ns < seg.t0_ns {
                next += 1;
            }
            let mut current: Option<usize> = None;
            for i in 0..len {
                let cell_end = seg.t0_ns + (i as i64 + 1) * SEC_NS;
                while next < snapshots.len() && snapshots[next].ts_ns < cell_end {
                    current = Some(next);
                    next += 1;
                }
                let Some(idx) = current else { continue };
                if snapshots[idx].levels.is_empty() {
                    continue;
                }
                let cell = start + i;
                if cell >= n {
                    break;
                }
                grid.snap_at[cell] = idx as u32;
                grid.bid[cell] = bbo[idx].0;
                grid.ask[cell] = bbo[idx].1;
            }
        }
        grid
    }

    /// Align trade volumes to the time grid. Where depth is absent, independently carry each
    /// estimated side through empty seconds within the same segment.
    pub(crate) fn apply_trade_samples(
        &mut self,
        axis: &TimeAxis,
        bounds: &[(usize, usize)],
        quotes: &[TradeQuote],
        estimate: bool,
    ) -> Vec<TradeVolume> {
        if quotes.is_empty() {
            return Vec::new();
        }
        let mut volumes = vec![TradeVolume::default(); self.bid.len()];
        for quote in quotes {
            let cell = axis.to_x(quote.ts_ns) as usize;
            volumes[cell] = quote.volume;
            if estimate {
                self.bid[cell] = quote.bid;
                self.ask[cell] = quote.ask;
            }
        }
        if estimate {
            for &(start, end) in bounds {
                carry_quotes(&mut self.bid[start..end]);
                carry_quotes(&mut self.ask[start..end]);
            }
        }
        volumes
    }
}

fn carry_quotes(prices: &mut [f32]) {
    let mut previous = f32::NAN;
    for price in prices {
        if price.is_finite() {
            previous = *price;
        } else {
            *price = previous;
        }
    }
}

/// Smooth estimates with a trailing window so replay only uses completed, past seconds.
pub(crate) fn smooth_estimates(raw: &[f32], bounds: &[(usize, usize)], window: usize) -> Vec<f32> {
    if window <= 1 {
        return raw.to_vec();
    }
    let mut smoothed = vec![f32::NAN; raw.len()];
    for &(start, end) in bounds {
        let (mut sum, mut count) = (0.0, 0);
        for i in start..end {
            if i - start >= window && raw[i - window].is_finite() {
                sum -= raw[i - window] as f64;
                count -= 1;
            }
            if raw[i].is_finite() {
                sum += raw[i] as f64;
                count += 1;
                smoothed[i] = (sum / count as f64) as f32;
            }
        }
    }
    smoothed
}

/// Best bid (highest price with resting bid size) and best ask (lowest price with ask size).
pub fn best_bid_ask(snapshot: &LiquiditySnapshot) -> (f32, f32) {
    let mut bid = f64::NAN;
    let mut ask = f64::NAN;
    for lv in &snapshot.levels {
        if lv.bid_size > 0.0 && !(lv.price <= bid) {
            bid = lv.price;
        }
        if lv.ask_size > 0.0 && !(lv.price >= ask) {
            ask = lv.price;
        }
    }
    (bid as f32, ask as f32)
}

/// Zero-lag smoothing of a per-second series: two centered moving-average passes (a triangular
/// kernel about `window` seconds wide). `NaN` cells stay `NaN` and are ignored as neighbours, and
/// the kernel never reaches across a segment boundary. `window <= 1` returns the input unchanged.
pub fn smooth(raw: &[f32], bounds: &[(usize, usize)], window: usize) -> Vec<f32> {
    if window <= 1 {
        return raw.to_vec();
    }
    let half = (window / 4).max(1);
    let mut out = raw.to_vec();
    let mut scratch = vec![0f32; raw.len()];
    for &(start, end) in bounds {
        let end = end.min(raw.len());
        if start >= end {
            continue;
        }
        box_pass(&raw[start..end], &mut scratch[start..end], half);
        box_pass(&scratch[start..end], &mut out[start..end], half);
    }
    out
}

fn box_pass(src: &[f32], dst: &mut [f32], half: usize) {
    let n = src.len();
    let mut sum = vec![0f64; n + 1];
    let mut cnt = vec![0u32; n + 1];
    for (i, &v) in src.iter().enumerate() {
        let valid = v.is_finite();
        sum[i + 1] = sum[i] + if valid { v as f64 } else { 0.0 };
        cnt[i + 1] = cnt[i] + u32::from(valid);
    }
    for i in 0..n {
        if !src[i].is_finite() {
            dst[i] = f32::NAN;
            continue;
        }
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        let c = cnt[hi] - cnt[lo];
        dst[i] = ((sum[hi] - sum[lo]) / c as f64) as f32;
    }
}

/// A robust "large resting size" used to normalize heat colors, so colors do not flicker while
/// panning: the 99th percentile of per-level total size over an even sample of the history.
pub fn reference_size(snapshots: &[LiquiditySnapshot]) -> f32 {
    const MAX_SAMPLES: usize = 200_000;
    let total: usize = snapshots.iter().map(|s| s.levels.len()).sum();
    if total == 0 {
        return 1.0;
    }
    let stride = total.div_ceil(MAX_SAMPLES).max(1);
    let mut sample: Vec<f32> = snapshots
        .iter()
        .flat_map(|s| s.levels.iter())
        .step_by(stride)
        .map(|lv| (lv.bid_size + lv.ask_size) as f32)
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if sample.is_empty() {
        return 1.0;
    }
    let k = ((sample.len() - 1) as f64 * 0.99) as usize;
    let (_, v, _) = sample.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    v.max(1.0)
}

/// Geometry of a heat raster. Columns and rows are aligned to multiples of `col_w` / `row_p`,
/// so the raster does not shimmer while panning; at close zoom `col_w == 1` (one column per
/// second) and `row_p == tick` (one row per price tick).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterSpec {
    /// Index of the first column; column `j` covers x in `[(c_lo + j) * col_w, +col_w)`.
    pub c_lo: i64,
    pub cols: usize,
    pub col_w: f64,
    /// Index of the top row; row `r` is centered on price `(k_hi - r) * row_p`.
    pub k_hi: i64,
    pub rows: usize,
    pub row_p: f64,
}

impl RasterSpec {
    pub fn new(x0: f64, x1: f64, width_px: f32, p0: f64, p1: f64, height_px: f32, tick: f64) -> Self {
        let width = (width_px as f64).clamp(16.0, 2048.0);
        let height = (height_px as f64).clamp(16.0, 1024.0);
        let col_w = ((x1 - x0) / width).max(1.0);
        let c_lo = (x0 / col_w).floor() as i64;
        let c_hi = (x1 / col_w).ceil() as i64;
        let tick = if tick > 0.0 && tick.is_finite() { tick } else { 1e-6 };
        let row_p = ((p1 - p0) / height).max(tick);
        let k_hi = (p1 / row_p).ceil() as i64;
        let k_lo = (p0 / row_p).floor() as i64;
        Self {
            c_lo,
            cols: ((c_hi - c_lo).max(1) as usize).min(2048),
            col_w,
            k_hi,
            rows: ((k_hi - k_lo + 1).max(1) as usize).min(2048),
            row_p,
        }
    }

    /// x range covered by the raster.
    pub fn x_range(&self) -> (f64, f64) {
        let x0 = self.c_lo as f64 * self.col_w;
        (x0, x0 + self.cols as f64 * self.col_w)
    }

    /// Price range `(top, bottom)` covered by the raster.
    pub fn price_range(&self) -> (f64, f64) {
        let top = (self.k_hi as f64 + 0.5) * self.row_p;
        (top, top - self.rows as f64 * self.row_p)
    }
}

/// Row-major `cols * rows` resting sizes (max within each cell) for the given raster.
pub fn rasterize(
    spec: &RasterSpec,
    grid: &BookGrid,
    snapshots: &[LiquiditySnapshot],
    metric: HeatMetric,
) -> Vec<f32> {
    /// Upper bound on snapshots merged into one column when zoomed far out.
    const MAX_SNAPSHOTS_PER_COLUMN: usize = 8;
    let mut cells = vec![0f32; spec.cols * spec.rows];
    let n = grid.snap_at.len() as i64;
    for col in 0..spec.cols {
        let xa = (spec.c_lo + col as i64) as f64 * spec.col_w;
        let ia = (xa.floor() as i64).max(0);
        let ib = ((xa + spec.col_w).ceil() as i64).min(n);
        if ia >= ib {
            continue;
        }
        let (ia, ib) = (ia as usize, ib as usize);
        let stride = ((ib - ia) / MAX_SNAPSHOTS_PER_COLUMN).max(1);
        let mut last = NO_SNAPSHOT;
        for sec in (ia..ib).step_by(stride) {
            let idx = grid.snap_at[sec];
            if idx == NO_SNAPSHOT || idx == last {
                continue;
            }
            last = idx;
            for lv in &snapshots[idx as usize].levels {
                let v = match metric {
                    HeatMetric::Total => lv.bid_size + lv.ask_size,
                    HeatMetric::Bid => lv.bid_size,
                    HeatMetric::Ask => lv.ask_size,
                } as f32;
                if !(v > 0.0) {
                    continue;
                }
                let row = spec.k_hi - (lv.price / spec.row_p).round() as i64;
                if row < 0 || row >= spec.rows as i64 {
                    continue;
                }
                let cell = &mut cells[row as usize * spec.cols + col];
                if v > *cell {
                    *cell = v;
                }
            }
        }
    }
    cells
}

/// Time zone of the wall-clock times the viewer shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimeZone {
    /// The computer's own time zone, with its daylight-saving rules applied per timestamp.
    Local,
    /// A fixed offset, in seconds east of UTC.
    Fixed(i64),
}

impl TimeZone {
    /// `Some(hours)` = fixed offset, `None` = the computer's time zone.
    pub fn from_hours(hours: Option<f64>) -> Self {
        match hours {
            Some(h) => Self::Fixed((h * 3600.0).round() as i64),
            None => Self::Local,
        }
    }

    /// Offset from UTC in seconds at `ts_ns`.
    pub fn offset_secs(self, ts_ns: i64) -> i64 {
        match self {
            Self::Fixed(secs) => secs,
            Self::Local => {
                use chrono::TimeZone as _;
                chrono::Local
                    .timestamp_opt(ts_ns.div_euclid(SEC_NS), 0)
                    .single()
                    .map_or(0, |dt| dt.offset().local_minus_utc() as i64)
            }
        }
    }

    /// `UTC+8`, `UTC-4`, `UTC+5:30` at `ts_ns`.
    pub fn label(self, ts_ns: i64) -> String {
        let off = self.offset_secs(ts_ns);
        let sign = if off < 0 { '-' } else { '+' };
        let (h, m) = (off.abs() / 3600, off.abs() % 3600 / 60);
        if m == 0 {
            format!("UTC{sign}{h}")
        } else {
            format!("UTC{sign}{h}:{m:02}")
        }
    }
}

/// Format a nanosecond timestamp as wall-clock time in `tz`.
pub fn format_ts(ts_ns: i64, tz: TimeZone, with_date: bool, with_secs: bool) -> String {
    let secs = ts_ns.div_euclid(SEC_NS) + tz.offset_secs(ts_ns);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, tod % 3600 / 60, tod % 60);
    let mut out = String::new();
    if with_date {
        let (_, month, day) = civil_from_days(days);
        out.push_str(&format!("{month:02}-{day:02} "));
    }
    out.push_str(&format!("{h:02}:{m:02}"));
    if with_secs {
        out.push_str(&format!(":{s:02}"));
    }
    out
}

/// Full `YYYY-MM-DD HH:MM:SS UTC+8` form of [`format_ts`].
pub fn format_ts_full(ts_ns: i64, tz: TimeZone) -> String {
    let secs = ts_ns.div_euclid(SEC_NS) + tz.offset_secs(ts_ns);
    let (year, _, _) = civil_from_days(secs.div_euclid(86_400));
    format!("{year}-{} {}", format_ts(ts_ns, tz, true, true), tz.label(ts_ns))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::result::LiquidityLevel;

    fn snap(ts_s: i64, levels: &[(f64, f64, f64)]) -> LiquiditySnapshot {
        LiquiditySnapshot {
            ts_ns: ts_s * SEC_NS,
            levels: levels
                .iter()
                .map(|&(price, bid_size, ask_size)| LiquidityLevel { price, bid_size, ask_size })
                .collect(),
        }
    }

    #[test]
    fn trade_extremes_carry_each_side_and_keep_volume_in_its_own_second() {
        let trade = |sec, price, size, aggressor| OrderFlowEvent::Trade {
            ts_ns: sec * SEC_NS, price, size, aggressor,
        };
        let events = [
            trade(0, 100.25, 3.0, Side::Buy),
            trade(0, 100.00, 2.0, Side::Buy),
            trade(0, 99.75, 4.0, Side::Sell),
            trade(0, 100.00, 1.0, Side::Sell),
            trade(0, 120.00, 6.0, Side::Unknown),
            trade(2, 99.50, 8.0, Side::Sell),
            trade(40, 101.00, 4.0, Side::Buy),
        ];
        let quotes = trade_quotes(&events);
        let axis = TimeAxis::build(vec![(0, 2 * SEC_NS), (40 * SEC_NS, 40 * SEC_NS)], 10 * SEC_NS);
        let mut grid = BookGrid::build(&axis, &[]);
        let volumes = grid.apply_trade_samples(&axis, &axis.bounds(), &quotes, true);

        assert_eq!(&grid.ask[..3], &[100.25, 100.25, 100.25]);
        assert_eq!(&grid.bid[..3], &[99.75, 99.75, 99.50]);
        assert_eq!(grid.ask[3], 101.0);
        assert!(grid.bid[3].is_nan());
        assert_eq!((volumes[0].buy, volumes[0].sell), (5.0, 5.0));
        assert_eq!((volumes[1].buy, volumes[1].sell), (0.0, 0.0));
        assert_eq!((volumes[2].buy, volumes[2].sell), (0.0, 8.0));
        assert_eq!(grid.snap_at, vec![NO_SNAPSHOT; 4]);

        let mut grid = BookGrid::build(&axis, &[]);
        grid.bid[0] = 98.0;
        let volumes = grid.apply_trade_samples(&axis, &axis.bounds(), &quotes, false);
        assert_eq!(grid.bid[0], 98.0);
        assert!(grid.ask[0].is_nan());
        assert_eq!(volumes[0].buy, 5.0);
    }

    #[test]
    fn estimate_smoothing_is_trailing_and_resets_at_session_boundaries() {
        let raw = [100.0, 103.0, 106.0, 109.0, f32::NAN, 120.0];
        let smoothed = smooth_estimates(&raw, &[(0, 4), (4, 6)], 3);
        assert_eq!(&smoothed[..4], &[100.0, 101.5, 103.0, 106.0]);
        assert!(smoothed[4].is_nan());
        assert_eq!(smoothed[5], 120.0);
    }

    #[test]
    fn quote_columns_share_session_aligned_buckets_at_each_zoom() {
        let bounds = [(0, 4), (4, 6)];
        let bid = [98.0, 99.0, 100.0, 101.0, f32::NAN, 108.0];
        let ask = [100.0, 101.0, 102.0, 103.0, 110.0, 111.0];
        let volumes = [
            TradeVolume { buy: 2.0, sell: 3.0, notional: 500.0 },
            TradeVolume { buy: 4.0, sell: 5.0, notional: 918.0 },
        ];

        let zoomed_in = quote_columns(&bounds, [&bid, &ask], &volumes, 0..6, 1);
        assert_eq!(zoomed_in[0][0].x, 0.5);
        assert_eq!(zoomed_in[0][0].prices, [98.0, 100.0]);
        assert_eq!(zoomed_in[0][0].bubble, Some(TradeBubble {
            x: 0.5, price: 100.0, buy: 2.0, sell: 3.0,
        }));

        let zoomed_out = quote_columns(&bounds, [&bid, &ask], &volumes, 0..6, 3);
        assert_eq!(zoomed_out[0][0].x, 1.5);
        assert_eq!(zoomed_out[0][0].prices, [99.0, 101.0]);
        assert_eq!(zoomed_out[0][0].bubble, Some(TradeBubble {
            x: 16.0 / 14.0, price: 1418.0 / 14.0, buy: 6.0, sell: 8.0,
        }));
        assert_eq!(zoomed_out[0][1].x, 3.5);
        assert_eq!(zoomed_out[1][0].prices, [108.0, 110.5]);

        let panned = quote_columns(&bounds, [&bid, &ask], &volumes, 2..6, 3);
        assert_eq!(panned[0][0].x, zoomed_out[0][0].x);
        let replay = quote_columns(&bounds, [&bid[..1], &ask[..1]], &volumes, 0..6, 3);
        assert_eq!(replay[0][0].bubble, zoomed_in[0][0].bubble);
    }

    #[test]
    fn smart_slider_and_sizing_match_bytecode_values() {
        assert_eq!(smart_factor(0), 0.0);
        assert!((smart_factor(38) - 0.8921587713210717).abs() < 1e-12);
        assert!((smart_factor(52) - 1.0667044537991437).abs() < 1e-12);
        assert_eq!(bubble_size_target(40), 27.0);
        assert_eq!(bubble_radius(8.0, 64.0, 20.0, true), 10.0);
        assert!((bubble_radius(8.0, 64.0, 20.0, false) - 20.0 / 8.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(bubble_radius(1.0, 1e12, 20.0, true), 3.0);
        assert_eq!(bubble_alpha(3.0, 41), 255);
        assert_eq!(bubble_alpha(20.0, 100), 0);
        assert_eq!(bubble_alpha(20.0, 41), 188);
    }

    #[test]
    fn smart_bubbles_merge_nearby_then_overlapping_pairs() {
        let dots = [
            TradeBubble { x: 0.5, price: 100.0, buy: 2.0, sell: 0.0 },
            TradeBubble { x: 1.5, price: 100.25, buy: 0.0, sell: 6.0 },
            TradeBubble { x: 2.5, price: 110.0, buy: 1.0, sell: 0.0 },
        ];
        let options = BubbleClustering { strength: 52, target: 27.0, spherical: true, delta: true };
        let off = BubbleClustering { strength: 0, ..options };
        assert_eq!(cluster_bubbles(dots.into_iter(), [4.0, 8.0], 0.0, off), dots);
        let merged = cluster_bubbles(dots.into_iter(), [4.0, 8.0], 0.0, options);
        assert_eq!(merged, [
            TradeBubble { x: 1.25, price: 100.1875, buy: 2.0, sell: 6.0 },
            dots[2],
        ]);
        let overlap = cluster_bubbles(dots[..2].iter().copied(), [18.0, 8.0], 0.0, options);
        assert_eq!(overlap, [TradeBubble { x: 23.0 / 18.0, ..merged[0] }]);
        assert_eq!(overlap[0].clustering_volume(true), 4.0);
        assert_eq!(overlap[0].clustering_volume(false), 8.0);
        assert_eq!(cluster_bubbles(dots.into_iter(), [100.0, 8.0], 0.0, options).len(), 3);
    }

    #[test]
    fn axis_removes_gaps_and_round_trips() {
        // Two sessions: 100s..110s and 1000s..1005s, separated by more than the 60s max gap.
        let axis = TimeAxis::build(
            vec![(1000 * SEC_NS, 1004 * SEC_NS), (100 * SEC_NS, 109 * SEC_NS)],
            60 * SEC_NS,
        );
        assert_eq!(axis.segments.len(), 2);
        assert_eq!(axis.total(), 15.0);
        assert_eq!(axis.to_x(100 * SEC_NS), 0.0);
        assert_eq!(axis.to_x(105 * SEC_NS + SEC_NS / 2), 5.5);
        // Inside the removed gap → the seam between the two segments.
        assert_eq!(axis.to_x(500 * SEC_NS), 10.0);
        assert_eq!(axis.to_x(1002 * SEC_NS), 12.0);
        assert_eq!(axis.to_ts(12.0), 1002 * SEC_NS);
        assert_eq!(axis.to_ts(5.5), 105 * SEC_NS + SEC_NS / 2);
        assert_eq!(axis.bounds(), vec![(0, 10), (10, 15)]);
    }

    #[test]
    fn bar_axis_gives_every_bar_one_slot_and_spaces_out_gaps() {
        let bars = [(0, 10 * SEC_NS), (10 * SEC_NS, 400 * SEC_NS), (5000 * SEC_NS, 5001 * SEC_NS)];
        let axis = BarAxis::build(&bars, 1800 * SEC_NS, 3.0);
        assert_eq!(axis.total(), 6.0);
        assert_eq!(axis.to_x(5 * SEC_NS), 0.5);
        assert_eq!(axis.to_x(205 * SEC_NS), 1.5);
        assert_eq!(axis.to_x(3000 * SEC_NS), 2.0);
        assert_eq!(axis.to_x(5000 * SEC_NS), 5.0);
        assert_eq!(axis.to_ts(1.5), 205 * SEC_NS);
        assert_eq!(axis.to_ts(3.5), 400 * SEC_NS);
        let map = XMap::Bars(axis);
        let starts: Vec<f64> = map.sessions().iter().map(|s| s.1).collect();
        assert_eq!(starts, vec![0.0, 5.0]);
        assert!(crosses_session(&starts, 2.0, 5.0));
        assert!(!crosses_session(&starts, 0.5, 1.5));
    }

    #[test]
    fn grid_forward_fills_within_a_segment_only() {
        let snaps = vec![
            snap(100, &[(9.99, 5.0, 0.0), (10.01, 0.0, 7.0)]),
            snap(103, &[(10.00, 4.0, 0.0), (10.02, 0.0, 3.0)]),
            snap(106, &[]),
            snap(1001, &[(11.00, 1.0, 0.0), (11.01, 0.0, 1.0)]),
        ];
        let axis = TimeAxis::build(
            vec![(100 * SEC_NS, 109 * SEC_NS), (1000 * SEC_NS, 1004 * SEC_NS)],
            60 * SEC_NS,
        );
        let grid = BookGrid::build(&axis, &snaps);
        assert_eq!(&grid.snap_at[0..6], &[0, 0, 0, 1, 1, 1]);
        // The empty snapshot clears the book for the rest of the segment.
        assert!(grid.snap_at[6..10].iter().all(|&i| i == NO_SNAPSHOT));
        // Second segment: nothing is carried over from the first one.
        assert_eq!(grid.snap_at[10], NO_SNAPSHOT);
        assert_eq!(&grid.snap_at[11..15], &[3, 3, 3, 3]);
        assert_eq!((grid.bid[0], grid.ask[0]), (9.99, 10.01));
        assert_eq!((grid.bid[4], grid.ask[4]), (10.0, 10.02));
        assert!(grid.bid[7].is_nan());
    }

    #[test]
    fn smoothing_preserves_constants_nans_and_segment_edges() {
        let mut raw = vec![10.0f32; 40];
        raw.extend(std::iter::repeat(20.0).take(40));
        raw[5] = f32::NAN;
        let bounds = [(0usize, 40usize), (40, 80)];
        let out = smooth(&raw, &bounds, 12);
        assert!(out[5].is_nan());
        // Each segment is constant, so smoothing must not leak 20s into the 10s or vice versa.
        assert!(out[..40].iter().filter(|v| v.is_finite()).all(|v| (v - 10.0).abs() < 1e-5));
        assert!(out[40..].iter().all(|v| (v - 20.0).abs() < 1e-5));

        // A step inside one segment becomes a ramp centered on the step (zero lag).
        let mut step = vec![0.0f32; 50];
        step.extend(std::iter::repeat(1.0).take(50));
        let out = smooth(&step, &[(0, 100)], 20);
        assert!(out[20] < 1e-6 && (out[80] - 1.0).abs() < 1e-6);
        assert!((out[49] + out[50] - 1.0).abs() < 1e-5);
        assert!(out.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(smooth(&step, &[(0, 100)], 1), step);
    }

    #[test]
    fn raster_is_one_column_per_second_and_one_row_per_tick_when_zoomed_in() {
        let snaps = vec![
            snap(100, &[(9.99, 5.0, 0.0), (10.01, 0.0, 7.0)]),
            snap(102, &[(10.00, 4.0, 0.0), (10.01, 0.0, 9.0)]),
        ];
        let axis = TimeAxis::build(vec![(100 * SEC_NS, 103 * SEC_NS)], 60 * SEC_NS);
        let grid = BookGrid::build(&axis, &snaps);
        let spec = RasterSpec::new(0.0, 4.0, 800.0, 9.985, 10.015, 600.0, 0.01);
        assert_eq!((spec.col_w, spec.cols), (1.0, 4));
        assert!((spec.row_p - 0.01).abs() < 1e-12);
        let cells = rasterize(&spec, &grid, &snaps, HeatMetric::Total);
        let at = |sec: usize, price: f64| {
            let row = (spec.k_hi - (price / spec.row_p).round() as i64) as usize;
            cells[row * spec.cols + sec]
        };
        assert_eq!(at(0, 9.99), 5.0);
        assert_eq!(at(1, 10.01), 7.0);
        assert_eq!(at(1, 10.00), 0.0);
        assert_eq!(at(2, 10.00), 4.0);
        assert_eq!(at(3, 10.01), 9.0);
        assert_eq!(at(3, 9.99), 0.0);
        let bids = rasterize(&spec, &grid, &snaps, HeatMetric::Bid);
        assert_eq!(bids.iter().copied().fold(0.0, f32::max), 5.0);
    }

    #[test]
    fn timestamps_format_in_local_time() {
        // 2026-07-01 01:30:15 UTC == 09:30:15 in UTC+8.
        let ts = (20_635 * 86_400 + 3600 + 30 * 60 + 15) * SEC_NS;
        assert_eq!(format_ts_full(ts, TimeZone::Fixed(8 * 3600)), "2026-07-01 09:30:15 UTC+8");
        assert_eq!(TimeZone::Fixed(-4 * 3600).label(ts), "UTC-4");
        assert_eq!(TimeZone::from_hours(Some(5.5)).label(ts), "UTC+5:30");
        assert_eq!(format_ts(ts, TimeZone::Fixed(0), false, false), "01:30");
    }
}
