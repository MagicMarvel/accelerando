//! Liquidity filters and color mapping in resting-size units.

use std::borrow::Cow;

use crate::data::{RasterSpec, TimeAxis};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PriceFilter {
    Off,
    Auto,
    Gaussian,
}

impl PriceFilter {
    pub fn sigma(self, tick_pixels: f64, row_pixels: f64, manual_px: f32) -> f32 {
        if !(tick_pixels > 0.0 && row_pixels > 0.0) {
            return 0.0;
        }
        let pixels = match self {
            Self::Auto => (1.0 / (tick_pixels * 0.050_044_1) - 1.0).clamp(0.0, 10.0),
            Self::Gaussian => f64::from(manual_px),
            Self::Off => 0.0,
        };
        (pixels / row_pixels) as f32
    }
}

/// A column belongs to a continuous trading segment. Gaps and the part beyond the
/// replay cursor have no group and do not contribute to neighboring columns.
pub(crate) fn column_groups(
    spec: &RasterSpec,
    axis: &TimeAxis,
    replay_x: Option<f64>,
) -> Vec<Option<usize>> {
    (0..spec.cols)
        .map(|col| {
            let left = (spec.c_lo + col as i64) as f64 * spec.col_w;
            let right = (left + spec.col_w).min(replay_x.unwrap_or(f64::INFINITY));
            if right <= left {
                return None;
            }
            let segment = axis
                .segments
                .partition_point(|seg| seg.x0 < right)
                .checked_sub(1)?;
            let seg = &axis.segments[segment];
            (left < seg.x0 + seg.len_secs()).then_some(segment)
        })
        .collect()
}

pub(crate) fn smooth_time(cells: &mut [f32], columns: usize, groups: &[Option<usize>]) {
    let mut scratch = vec![0.0; columns];
    for row in cells.chunks_exact_mut(columns) {
        scratch.copy_from_slice(row);
        for (col, value) in row.iter_mut().enumerate() {
            *value = time_filtered_cell(&scratch, col, time_neighbors(groups, col));
        }
    }
}

/// Expand price bands to texture rows, then spread each band from its nearest edge.
/// Return the remaining row repetition so callers can still replicate unsmoothed colors cheaply.
pub(crate) fn price_pixels(
    cells: &[f32], columns: usize, repeat: usize, sigma_rows: f32,
) -> (Cow<'_, [f32]>, usize) {
    if sigma_rows <= 0.0 {
        return (Cow::Borrowed(cells), repeat);
    }

    let sigma = sigma_rows * repeat as f32;
    let radius = (sigma * 3.0).ceil().min(64.0) as usize;
    let weights: Vec<_> = (1..=radius)
        .map(|distance| (-0.5 * (distance as f32 / sigma.max(0.1)).powi(2)).exp())
        .collect();
    let height = cells.len() / columns * repeat;
    let mut pixels = Vec::with_capacity(height * columns);
    for band in cells.chunks_exact(columns) {
        for _ in 0..repeat {
            pixels.extend_from_slice(band);
        }
    }
    // Every pixel within a band starts with the same depth. Only the nearest pixel
    // of that band can win the weighted maximum, so iterate just outside its edges.
    for (row, band) in cells.chunks_exact(columns).enumerate() {
        let start = row * repeat;
        let end = start + repeat;
        for (offset, &weight) in weights.iter().enumerate() {
            let above = start.checked_sub(offset + 1);
            let below = (end + offset < height).then_some(end + offset);
            for target in [above, below].into_iter().flatten() {
                let dest = &mut pixels[target * columns..][..columns];
                for (value, &sample) in dest.iter_mut().zip(band) {
                    *value = value.max(sample * weight);
                }
            }
        }
    }
    (Cow::Owned(pixels), 1)
}

pub(crate) fn time_neighbors(groups: &[Option<usize>], col: usize) -> [Option<usize>; 2] {
    let Some(group) = groups[col] else { return [None; 2] };
    [col.checked_sub(1), (col + 1 < groups.len()).then_some(col + 1)]
        .map(|neighbor| neighbor.filter(|&i| groups[i] == Some(group)))
}

pub(crate) fn time_filtered_cell(row: &[f32], col: usize, neighbors: [Option<usize>; 2]) -> f32 {
    if neighbors == [None; 2] {
        return row[col];
    }
    let mut sum = 2.0 * row[col];
    let mut weight = 2.0;
    for neighbor in neighbors.into_iter().flatten() {
        sum += row[neighbor];
        weight += 1.0;
    }
    sum / weight
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct ColorScale {
    low: f32,
    high: f32,
    unit: f32,
    log_span: f32,
    contrast: f32,
}

const PALETTE_BUCKET_SHIFT: u32 = 19;

/// Thresholds for the 8-bit palette, with a small index that narrows the search
/// using the exponent and the leading mantissa bits of each positive size.
pub(crate) struct PaletteBins {
    thresholds: [u32; 255],
    buckets: Vec<(u8, u8)>,
}

impl PaletteBins {
    pub fn index(&self, value: f32) -> usize {
        let bits = value.to_bits();
        let (start, end) = self.buckets[(bits >> PALETTE_BUCKET_SHIFT) as usize];
        let start = usize::from(start);
        start + self.thresholds[start..usize::from(end)].partition_point(|&limit| limit <= bits)
    }
}

impl ColorScale {
    pub fn from_cells(cells: &[f32], lower_pct: f32, upper_pct: f32, contrast: f32) -> Self {
        // Stratify the sample across the tile without repeatedly selecting the same column.
        let stride = cells.len().div_ceil(32_768).max(1);
        let mut sizes: Vec<_> = cells
            .chunks(stride)
            .enumerate()
            .map(|(i, chunk)| chunk[i % chunk.len()])
            .filter(|&size| size.is_finite() && size > 0.0)
            .collect();
        sizes.sort_unstable_by(f32::total_cmp);
        let quantile = |pct: f32| {
            let index = ((sizes.len().saturating_sub(1)) as f32 * pct * 0.01).round() as usize;
            sizes.get(index).copied().unwrap_or(0.0)
        };
        let high = quantile(upper_pct).max(1e-6);
        // Uniform books still get a useful color instead of collapsing to the low cutoff.
        let low = quantile(lower_pct).min(high * 0.75);
        // Compress the long tail using a viewport-relative unit. This keeps ordinary
        // depth visible even when a few large resting orders determine the upper cutoff.
        let span = high - low;
        let unit = ((quantile(50.0) - low) * 0.25)
            .max(span * 0.0001)
            .max(f32::EPSILON);
        let log_span = (span / unit).ln_1p();
        Self {
            low,
            high,
            unit,
            log_span,
            contrast,
        }
    }

    pub fn intensity(&self, value: f32) -> f32 {
        let size = value.clamp(self.low, self.high) - self.low;
        ((size / self.unit).ln_1p() / self.log_span)
            .clamp(0.0, 1.0)
            .powf(self.contrast)
    }

    pub fn palette_bins(&self) -> PaletteBins {
        let mut lower = self.low.to_bits();
        let thresholds = std::array::from_fn(|i| {
            // Positive finite f32 values are ordered by their bits. Use the same
            // forward color mapping to preserve rounding at palette boundaries.
            let mut upper = self.high.to_bits();
            while lower < upper {
                let mid = lower + (upper - lower) / 2;
                if (self.intensity(f32::from_bits(mid)) * 255.0) as usize > i {
                    upper = mid;
                } else {
                    lower = mid + 1;
                }
            }
            lower
        });
        let mut next = 0;
        let buckets = (0..=(f32::MAX.to_bits() >> PALETTE_BUCKET_SHIFT))
            .map(|bucket| {
                let start = next;
                let end = (bucket + 1) << PALETTE_BUCKET_SHIFT;
                next += thresholds[start..].partition_point(|&limit| limit < end);
                (start as u8, next as u8)
            })
            .collect();
        PaletteBins { thresholds, buckets }
    }
}

/// Dark blue through teal, gold and red. The brightest liquidity remains distinguishable
/// against bright overlays because the ramp stops at a saturated red instead of white.
pub(crate) fn palette(intensity: f32) -> [u8; 3] {
    const STOPS: [(f32, [u8; 3]); 8] = [
        (0.00, [8, 13, 26]),
        (0.12, [12, 28, 57]),
        (0.28, [16, 54, 105]),
        (0.49, [22, 105, 159]),
        (0.65, [30, 151, 143]),
        (0.79, [201, 169, 66]),
        (0.91, [248, 132, 36]),
        (1.00, [235, 54, 36]),
    ];
    for pair in STOPS.windows(2) {
        let [(a, low), (b, high)] = [pair[0], pair[1]];
        if intensity <= b {
            let w = ((intensity - a) / (b - a)).clamp(0.0, 1.0);
            return std::array::from_fn(|i| {
                (f32::from(low[i]) * (1.0 - w) + f32::from(high[i]) * w).round() as u8
            });
        }
    }
    STOPS[STOPS.len() - 1].1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_spreads_in_price_and_time_but_respects_segment_boundaries() {
        let mut depth = vec![0.0, 8.0, 0.0, 0.0];
        smooth_time(
            &mut depth,
            4,
            &[Some(0), Some(0), Some(0), Some(1)],
        );
        assert_eq!(depth, vec![8.0 / 3.0, 4.0, 8.0 / 3.0, 0.0]);

        let (pixels, repeat) = price_pixels(&[0.0, 100.0, 0.0], 1, 21, 0.5 / 21.0);
        assert_eq!(repeat, 1);
        assert_eq!(pixels.len(), 63);
        assert!((pixels[20] - 100.0 * (-2.0_f32).exp()).abs() < 1e-5);
        assert_eq!(&pixels[21..42], &[100.0; 21]);
        assert_eq!(pixels[20], pixels[42]);
        assert_eq!(pixels[18], 0.0);
        assert!((PriceFilter::Auto.sigma(10.0, 10.0, 1.5) - 0.0998).abs() < 0.0001);
        assert_eq!(PriceFilter::Auto.sigma(20.0, 20.0, 1.5), 0.0);
        assert_eq!(PriceFilter::Auto.sigma(1.0, 1.0, 1.5), 10.0);
        assert!((PriceFilter::Auto.sigma(2.0, 2.0, 1.5) - 4.4956).abs() < 0.0001);
        assert_eq!(PriceFilter::Gaussian.sigma(2.0, 2.0, 1.5), 0.75);
    }

    #[test]
    fn percentile_scale_preserves_headroom_above_the_old_cutoff() {
        let values: Vec<_> = (1..=200).map(|value| value as f32).collect();
        let scale = ColorScale::from_cells(&values, 2.0, 99.5, 1.2);
        assert!(scale.intensity(150.0) < scale.intensity(190.0));
        assert!(scale.intensity(190.0) < 1.0);
        assert_eq!(palette(scale.intensity(200.0)), [235, 54, 36]);
    }

    #[test]
    fn palette_bins_match_the_original_mapping_at_float_boundaries() {
        let values: Vec<_> = (1..=4000).map(|value| value as f32 / 7.0).collect();
        for contrast in [0.25, 1.0, 4.0, 8.0] {
            for (low, high) in [(0.0, 100.0), (2.0, 99.5), (75.0, 95.0)] {
                let scale = ColorScale::from_cells(&values, low, high, contrast);
                let bins = scale.palette_bins();
                let boundaries = bins.thresholds.iter().flat_map(|&bits| {
                    [bits - 1, bits, bits + 1].map(f32::from_bits)
                });
                for value in values.iter().copied().chain(boundaries).chain([f32::MAX]) {
                    assert_eq!(bins.index(value), (scale.intensity(value) * 255.0) as usize);
                }
            }
        }
    }
}
