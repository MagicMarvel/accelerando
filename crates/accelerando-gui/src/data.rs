//! Pure data preparation for the viewer — no egui types, so everything here is unit-testable.
//!
//! * [`TimeAxis`] maps nanosecond timestamps onto a compressed x axis measured in seconds, with
//!   closed-market gaps (lunch break, overnight, weekends) removed.
//! * [`BookGrid`] resamples the liquidity snapshots onto that axis at one cell per second and
//!   derives the best bid / best ask per second.
//! * [`smooth`] is the zero-lag smoother applied to the bid/ask lines.
//! * [`rasterize`] turns the visible part of the grid into a heat raster.

use accelerando_core::result::LiquiditySnapshot;

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

/// Format a nanosecond timestamp as local wall-clock time (`utc_offset_secs` east of UTC).
pub fn format_ts(ts_ns: i64, utc_offset_secs: i64, with_date: bool, with_secs: bool) -> String {
    let secs = ts_ns.div_euclid(SEC_NS) + utc_offset_secs;
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

/// Full `YYYY-MM-DD HH:MM:SS` form of [`format_ts`].
pub fn format_ts_full(ts_ns: i64, utc_offset_secs: i64) -> String {
    let secs = ts_ns.div_euclid(SEC_NS) + utc_offset_secs;
    let (year, _, _) = civil_from_days(secs.div_euclid(86_400));
    format!("{year}-{}", format_ts(ts_ns, utc_offset_secs, true, true))
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
        assert_eq!(format_ts_full(ts, 8 * 3600), "2026-07-01 09:30:15");
        assert_eq!(format_ts(ts, 0, false, false), "01:30");
    }
}
