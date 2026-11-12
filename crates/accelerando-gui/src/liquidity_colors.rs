//! Liquidity filters and color mapping in resting-size units.

use crate::data::{RasterSpec, TimeAxis};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PriceFilter {
    Off,
    Auto,
    Gaussian,
}

impl PriceFilter {
    pub fn sigma(self, tick_pixels: f64, row_pixels: f64, manual_px: f32) -> f32 {
        if !(row_pixels > 0.0) {
            return 0.0;
        }
        let pixels = match self {
            Self::Auto if tick_pixels < 15.0 => 5.0,
            Self::Gaussian => f64::from(manual_px),
            _ => 0.0,
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

pub(crate) fn filter_cells(
    cells: &mut [f32],
    columns: usize,
    time_groups: Option<&[Option<usize>]>,
    sigma_rows: f32,
) {
    if let Some(groups) = time_groups {
        let mut scratch = vec![0.0; columns];
        for row in cells.chunks_exact_mut(columns) {
            scratch.copy_from_slice(row);
            for (col, value) in row.iter_mut().enumerate() {
                let Some(group) = groups[col] else { continue };
                let mut sum = 2.0 * scratch[col];
                let mut weight = 2.0;
                for neighbor in [col.checked_sub(1), (col + 1 < columns).then_some(col + 1)]
                    .into_iter()
                    .flatten()
                {
                    if groups[neighbor] == Some(group) {
                        sum += scratch[neighbor];
                        weight += 1.0;
                    }
                }
                *value = sum / weight;
            }
        }
    }
    if sigma_rows < 0.01 {
        return;
    }

    let radius = (sigma_rows * 3.0).ceil().min(32.0) as usize;
    let weights: Vec<_> = (0..=radius)
        .map(|distance| (-0.5 * (distance as f32 / sigma_rows).powi(2)).exp())
        .collect();
    let rows = cells.len() / columns;
    let input = cells.to_vec();
    for row in 0..rows {
        let first = row.saturating_sub(radius);
        let end = (row + radius + 1).min(rows);
        let dest = &mut cells[row * columns..][..columns];
        dest.fill(0.0);
        let mut total_weight = 0.0;
        for neighbor in first..end {
            let weight = weights[row.abs_diff(neighbor)];
            total_weight += weight;
            for (value, &sample) in dest.iter_mut().zip(&input[neighbor * columns..][..columns]) {
                *value += sample * weight;
            }
        }
        for value in dest {
            *value /= total_weight;
        }
    }
}

pub(crate) struct ColorScale {
    low: f32,
    high: f32,
    unit: f32,
    log_span: f32,
    contrast: f32,
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
        filter_cells(
            &mut depth,
            4,
            Some(&[Some(0), Some(0), Some(0), Some(1)]),
            0.0,
        );
        assert_eq!(depth, vec![8.0 / 3.0, 4.0, 8.0 / 3.0, 0.0]);

        let mut depth = vec![0.0, 10.0, 0.0];
        filter_cells(&mut depth, 1, None, 0.5);
        assert!(depth[0] > 0.0 && depth[1] < 10.0);
        assert_eq!(depth[0], depth[2]);
        assert_eq!(PriceFilter::Auto.sigma(10.0, 10.0, 5.0), 0.5);
        assert_eq!(PriceFilter::Auto.sigma(15.0, 15.0, 5.0), 0.0);
    }

    #[test]
    fn percentile_scale_preserves_headroom_above_the_old_cutoff() {
        let values: Vec<_> = (1..=200).map(|value| value as f32).collect();
        let scale = ColorScale::from_cells(&values, 2.0, 99.5, 1.2);
        assert!(scale.intensity(150.0) < scale.intensity(190.0));
        assert!(scale.intensity(190.0) < 1.0);
        assert_eq!(palette(scale.intensity(200.0)), [235, 54, 36]);
    }
}
