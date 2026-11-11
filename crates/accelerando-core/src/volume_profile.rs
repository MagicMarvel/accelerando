//! Volume-profile helpers shared by strategies and viewers: build a profile from a run of
//! footprints, Gaussian-smooth it along price, and read off POC / value area / peak area.
//!
//! All functions take and return per-tick [`VpLevel`]s sorted by ascending price.

use crate::footprint::{Footprint, VpLevel};
use std::collections::BTreeMap;

/// Point of control and value area of a profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProfileStats {
    pub poc: f64,
    /// Value area low / high.
    pub val: f64,
    pub vah: f64,
    pub total: f64,
}

/// Sum the ladders of `footprints` into one per-tick profile (ascending price).
pub fn profile_from_footprints<'a>(
    footprints: impl IntoIterator<Item = &'a Footprint>,
    tick: f64,
) -> Vec<VpLevel> {
    let tick = tick.abs().max(f64::EPSILON);
    let mut acc: BTreeMap<i64, (f64, f64)> = BTreeMap::new();
    for fp in footprints {
        for lv in &fp.ladder {
            let e = acc.entry((lv.price / tick).round() as i64).or_insert((0.0, 0.0));
            e.0 += lv.buy_vol;
            e.1 += lv.sell_vol;
        }
    }
    acc.into_iter()
        .map(|(k, (buy_vol, sell_vol))| VpLevel { price: k as f64 * tick, buy_vol, sell_vol })
        .collect()
}

/// Gaussian-smooth a profile along price (σ = `sigma_ticks` ticks, kernel cut at ±3σ). Buy and
/// sell volume are smoothed separately. The result covers every tick between the lowest and
/// highest input level; near the edges the kernel is renormalised over the levels that exist,
/// so the ends are not pulled down. `sigma_ticks <= 0` only fills the gaps with zero levels.
pub fn smooth_profile(levels: &[VpLevel], tick: f64, sigma_ticks: f64) -> Vec<VpLevel> {
    let tick = tick.abs().max(f64::EPSILON);
    let Some(lo) = levels.iter().map(|l| (l.price / tick).round() as i64).min() else {
        return Vec::new();
    };
    let hi = levels.iter().map(|l| (l.price / tick).round() as i64).max().unwrap_or(lo);
    let n = (hi - lo + 1) as usize;
    let (mut buy, mut sell) = (vec![0.0; n], vec![0.0; n]);
    for l in levels {
        let j = ((l.price / tick).round() as i64 - lo) as usize;
        buy[j] += l.buy_vol;
        sell[j] += l.sell_vol;
    }
    if sigma_ticks > 0.0 {
        let r = (3.0 * sigma_ticks).ceil() as i64;
        let kernel: Vec<f64> = (-r..=r)
            .map(|d| (-((d * d) as f64) / (2.0 * sigma_ticks * sigma_ticks)).exp())
            .collect();
        let conv = |src: &[f64]| -> Vec<f64> {
            (0..n as i64)
                .map(|j| {
                    let (mut acc, mut wsum) = (0.0, 0.0);
                    for d in -r..=r {
                        let k = j + d;
                        if (0..n as i64).contains(&k) {
                            let w = kernel[(d + r) as usize];
                            acc += w * src[k as usize];
                            wsum += w;
                        }
                    }
                    if wsum > 0.0 { acc / wsum } else { 0.0 }
                })
                .collect()
        };
        buy = conv(&buy);
        sell = conv(&sell);
    }
    (0..n)
        .map(|j| VpLevel {
            price: (lo + j as i64) as f64 * tick,
            buy_vol: buy[j],
            sell_vol: sell[j],
        })
        .collect()
}

fn total(l: &VpLevel) -> f64 {
    l.buy_vol + l.sell_vol
}

fn poc_index(levels: &[VpLevel]) -> Option<usize> {
    let (j, l) = levels.iter().enumerate().max_by(|a, b| total(a.1).total_cmp(&total(b.1)))?;
    (total(l) > 0.0).then_some(j)
}

/// POC and the classic value area: starting at the POC, repeatedly add the larger neighbouring
/// level until `fraction` of the volume is covered. `levels` should be contiguous ticks (as
/// returned by [`smooth_profile`]); gaps are simply skipped over.
pub fn value_area(levels: &[VpLevel], fraction: f64) -> Option<ProfileStats> {
    let sum: f64 = levels.iter().map(total).sum();
    let p = poc_index(levels)?;
    let (mut a, mut b) = (p, p);
    let mut acc = total(&levels[p]);
    while acc < fraction * sum && (a > 0 || b + 1 < levels.len()) {
        let up = if b + 1 < levels.len() { total(&levels[b + 1]) } else { -1.0 };
        let down = if a > 0 { total(&levels[a - 1]) } else { -1.0 };
        if up >= down {
            b += 1;
            acc += up;
        } else {
            a -= 1;
            acc += down;
        }
    }
    Some(ProfileStats { poc: levels[p].price, val: levels[a].price, vah: levels[b].price, total: sum })
}

/// The "mountain top" around the POC: the contiguous run of levels whose volume stays at or
/// above `fraction` × the POC volume. Returns `(poc, low, high)`.
pub fn peak_area(levels: &[VpLevel], fraction: f64) -> Option<(f64, f64, f64)> {
    let p = poc_index(levels)?;
    let limit = total(&levels[p]) * fraction;
    let (mut a, mut b) = (p, p);
    while a > 0 && total(&levels[a - 1]) >= limit {
        a -= 1;
    }
    while b + 1 < levels.len() && total(&levels[b + 1]) >= limit {
        b += 1;
    }
    Some((levels[p].price, levels[a].price, levels[b].price))
}

/// One mountain of a (usually smoothed) profile, see [`find_peaks`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Peak {
    /// Price of the local maximum.
    pub poc: f64,
    /// Peak area: contiguous levels with at least `area_fraction` × the peak volume.
    pub low: f64,
    pub high: f64,
    /// Volume at the peak.
    pub volume: f64,
    /// Height above the higher of the two valleys that separate it from taller ground
    /// (topographic prominence); the tallest peak's prominence is its height above the lower
    /// profile end.
    pub prominence: f64,
}

/// Local maxima of `levels` (contiguous ticks, e.g. from [`smooth_profile`]) that are real
/// mountains: prominence ≥ `min_prominence` × own height and height ≥ `min_share` × the
/// tallest level. Each peak's area is grown with `area_fraction` as in [`peak_area`], but never
/// past the valley to a neighbouring level that is higher. Sorted by price.
pub fn find_peaks(
    levels: &[VpLevel],
    min_prominence: f64,
    min_share: f64,
    area_fraction: f64,
) -> Vec<Peak> {
    let v: Vec<f64> = levels.iter().map(total).collect();
    let n = v.len();
    let max = v.iter().cloned().fold(0.0, f64::max);
    if n == 0 || max <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut j = 0;
    while j < n {
        // A plateau [j, k] counts once, at its middle.
        let mut k = j;
        while k + 1 < n && v[k + 1] == v[j] {
            k += 1;
        }
        let left_lower = j == 0 || v[j - 1] < v[j];
        let right_lower = k + 1 == n || v[k + 1] < v[j];
        if left_lower && right_lower && v[j] > 0.0 && v[j] >= min_share * max {
            let h = v[j];
            // Lowest point on each side before reaching higher ground (or the profile end).
            let mut left_min = h;
            let mut a = j;
            while a > 0 && v[a - 1] <= h {
                a -= 1;
                left_min = left_min.min(v[a]);
            }
            let left_blocked = a > 0;
            let mut right_min = h;
            let mut b = k;
            while b + 1 < n && v[b + 1] <= h {
                b += 1;
                right_min = right_min.min(v[b]);
            }
            let right_blocked = b + 1 < n;
            let base = match (left_blocked, right_blocked) {
                (true, true) => left_min.max(right_min),
                (true, false) => left_min,
                (false, true) => right_min,
                (false, false) => left_min.min(right_min),
            };
            let prominence = h - base;
            if prominence >= min_prominence * h {
                let mid = (j + k) / 2;
                let limit = h * area_fraction;
                let (mut lo, mut hi) = (j, k);
                while lo > 0 && v[lo - 1] >= limit && v[lo - 1] <= h {
                    lo -= 1;
                }
                while hi + 1 < n && v[hi + 1] >= limit && v[hi + 1] <= h {
                    hi += 1;
                }
                out.push(Peak {
                    poc: levels[mid].price,
                    low: levels[lo].price,
                    high: levels[hi].price,
                    volume: h,
                    prominence,
                });
            }
        }
        j = k + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv(price: f64, v: f64) -> VpLevel {
        VpLevel { price, buy_vol: v, sell_vol: 0.0 }
    }

    #[test]
    fn smoothing_prefers_broad_mountain_over_spike() {
        let mut levels = Vec::new();
        let mut p: f64 = 99.0;
        while p <= 105.0 + 1e-9 {
            let v = if (100.0..=102.0).contains(&p) {
                50.0
            } else if (p - 104.0).abs() < 1e-9 {
                120.0
            } else {
                5.0
            };
            levels.push(lv(p, v));
            p += 0.25;
        }
        assert_eq!(value_area(&levels, 0.7).unwrap().poc, 104.0);
        let sm = smooth_profile(&levels, 0.25, 3.0);
        let (poc, lo, hi) = peak_area(&sm, 0.7).unwrap();
        assert!((100.0..=102.0).contains(&poc), "{poc}");
        assert!(lo <= poc && poc <= hi && lo >= 99.5 && hi <= 102.5);
    }

    #[test]
    fn smoothing_fills_gaps_symmetrically() {
        let levels = vec![lv(100.0, 10.0), lv(101.0, 10.0)];
        let sm = smooth_profile(&levels, 0.25, 0.0);
        assert_eq!(sm.len(), 5);
        assert_eq!(total(&sm[2]), 0.0);
        let sm = smooth_profile(&levels, 0.25, 1.0);
        assert!(total(&sm[2]) > 0.0, "gap between the two levels is filled");
        assert!((total(&sm[0]) - total(&sm[4])).abs() < 1e-9, "symmetric");
    }

    #[test]
    fn finds_two_mountains_and_ignores_ripples() {
        // Mountain A around 100 (height 100), valley 20, mountain B around 105 (height 60),
        // plus a small bump in the valley that is too small (share filter) to count.
        let mut levels = Vec::new();
        for k in 0..=40 {
            let p = 98.0 + k as f64 * 0.25;
            let a = 100.0 * (-((p - 100.0) / 0.75).powi(2)).exp();
            let b = 60.0 * (-((p - 105.0) / 0.75).powi(2)).exp();
            let ripple = if (p - 102.5).abs() < 1e-9 { 3.0 } else { 0.0 };
            levels.push(lv(p, a + b + ripple + 1.0));
        }
        let peaks = find_peaks(&levels, 0.3, 0.1, 0.7);
        let pocs: Vec<f64> = peaks.iter().map(|p| p.poc).collect();
        assert_eq!(pocs, vec![100.0, 105.0]);
        assert!(peaks[0].low < 100.0 && peaks[0].high > 100.0 && peaks[0].high < 102.5);
        assert!(peaks[1].low > 102.5);
        // Only the tallest survives a 0.7 share filter.
        assert_eq!(find_peaks(&levels, 0.3, 0.7, 0.7).len(), 1);
    }

    #[test]
    fn value_area_expands_from_poc() {
        let levels: Vec<_> = [1.0, 5.0, 10.0, 5.0, 1.0]
            .iter()
            .enumerate()
            .map(|(k, v)| lv(k as f64 * 0.25, *v))
            .collect();
        let s = value_area(&levels, 0.7).unwrap();
        assert_eq!((s.poc, s.val, s.vah), (0.5, 0.25, 0.75));
    }
}
