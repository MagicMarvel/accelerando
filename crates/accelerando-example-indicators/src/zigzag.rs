//! Causal ZigZag pivot detection and a ready-to-use chart indicator.
//!
//! A pivot's `bar` is where the extreme occurred. `confirmed_bar` is the later bar whose reversal
//! first made that extreme knowable without looking into the future.

use accelerando_core::{Configurable, Footprint, Indicator, ParamSpec, Params, Plot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PivotKind {
    High,
    Low,
}

impl PivotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pivot {
    pub kind: PivotKind,
    pub price: f64,
    pub bar: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PivotConfirmation {
    pub pivot: Pivot,
    pub previous: Option<Pivot>,
    pub confirmed_bar: usize,
}

impl PivotConfirmation {
    pub fn lag(self) -> usize {
        self.confirmed_bar.saturating_sub(self.pivot.bar)
    }
}

/// Causal ZigZag detector. It never emits a pivot until price reverses by `reversal`.
pub struct ZigZag {
    reversal: f64,
    min_leg: f64,
    min_leg_bars: usize,
    direction: i8,
    extreme_price: f64,
    extreme_bar: usize,
    last_pivot: Option<Pivot>,
    seed_high: f64,
    seed_high_bar: usize,
    seed_low: f64,
    seed_low_bar: usize,
    seeded: bool,
}

impl ZigZag {
    pub fn new(reversal: f64) -> Self {
        Self::with_filters(reversal, 0.0, 0)
    }

    pub fn with_filters(reversal: f64, min_leg: f64, min_leg_bars: usize) -> Self {
        Self {
            reversal: reversal.max(f64::EPSILON),
            min_leg: min_leg.max(0.0),
            min_leg_bars,
            direction: 0,
            extreme_price: 0.0,
            extreme_bar: 0,
            last_pivot: None,
            seed_high: 0.0,
            seed_high_bar: 0,
            seed_low: 0.0,
            seed_low_bar: 0,
            seeded: false,
        }
    }

    pub fn last_pivot(&self) -> Option<Pivot> {
        self.last_pivot
    }

    /// Feed one completed bar and return a newly confirmed pivot, including its confirmation bar.
    pub fn update(&mut self, bar: usize, high: f64, low: f64) -> Option<PivotConfirmation> {
        if self.direction == 0 {
            if !self.seeded {
                self.seed_high = high;
                self.seed_high_bar = bar;
                self.seed_low = low;
                self.seed_low_bar = bar;
                self.seeded = true;
                return None;
            }
            if high > self.seed_high {
                self.seed_high = high;
                self.seed_high_bar = bar;
            }
            if low < self.seed_low {
                self.seed_low = low;
                self.seed_low_bar = bar;
            }
            let seed_move = self.reversal.max(self.min_leg);
            if high >= self.seed_low + seed_move {
                self.direction = 1;
                self.extreme_price = high;
                self.extreme_bar = bar;
                return Some(self.confirm(
                    Pivot {
                        kind: PivotKind::Low,
                        price: self.seed_low,
                        bar: self.seed_low_bar,
                    },
                    bar,
                ));
            }
            if low <= self.seed_high - seed_move {
                self.direction = -1;
                self.extreme_price = low;
                self.extreme_bar = bar;
                return Some(self.confirm(
                    Pivot {
                        kind: PivotKind::High,
                        price: self.seed_high,
                        bar: self.seed_high_bar,
                    },
                    bar,
                ));
            }
            return None;
        }

        if self.direction == 1 {
            // A wide time bar can both make a new extreme and reverse far enough to confirm it.
            if high > self.extreme_price {
                self.extreme_price = high;
                self.extreme_bar = bar;
            }
            if low <= self.extreme_price - self.reversal {
                let pivot = Pivot {
                    kind: PivotKind::High,
                    price: self.extreme_price,
                    bar: self.extreme_bar,
                };
                self.direction = -1;
                self.extreme_price = low;
                self.extreme_bar = bar;
                if self.leg_is_major(pivot) && self.last_pivot.map(|p| p.kind) != Some(pivot.kind) {
                    return Some(self.confirm(pivot, bar));
                }
            }
        } else {
            if low < self.extreme_price {
                self.extreme_price = low;
                self.extreme_bar = bar;
            }
            if high >= self.extreme_price + self.reversal {
                let pivot = Pivot {
                    kind: PivotKind::Low,
                    price: self.extreme_price,
                    bar: self.extreme_bar,
                };
                self.direction = 1;
                self.extreme_price = high;
                self.extreme_bar = bar;
                if self.leg_is_major(pivot) && self.last_pivot.map(|p| p.kind) != Some(pivot.kind) {
                    return Some(self.confirm(pivot, bar));
                }
            }
        }
        None
    }

    fn leg_is_major(&self, pivot: Pivot) -> bool {
        let Some(previous) = self.last_pivot else {
            return true;
        };
        (pivot.price - previous.price).abs() + f64::EPSILON >= self.min_leg
            && pivot.bar.saturating_sub(previous.bar) >= self.min_leg_bars
    }

    fn confirm(&mut self, pivot: Pivot, confirmed_bar: usize) -> PivotConfirmation {
        let event = PivotConfirmation {
            pivot,
            previous: self.last_pivot,
            confirmed_bar,
        };
        self.last_pivot = Some(pivot);
        event
    }
}

/// Framework indicator wrapper around [`ZigZag`]. Prices are configured in instrument points.
pub struct ZigZagIndicator {
    detector: ZigZag,
    bar: usize,
}

impl Configurable for ZigZagIndicator {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .float("reversal", 4.0, 0.000001, 10000.0)
            .float("min_leg", 0.0, 0.0, 10000.0)
            .int("min_leg_bars", 0, 0, 10000, 1)
    }

    fn from_params(p: &Params) -> Self {
        Self {
            detector: ZigZag::with_filters(
                p.float("reversal", 4.0),
                p.float("min_leg", 0.0),
                p.usize("min_leg_bars", 0),
            ),
            bar: 0,
        }
    }
}

impl Indicator for ZigZagIndicator {
    fn name(&self) -> &str {
        "zigzag"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        if let Some(event) = self.detector.update(self.bar, fp.high, fp.low) {
            fp.values
                .insert("zigzag_pivot_price".into(), event.pivot.price);
            fp.values
                .insert("zigzag_pivot_bar".into(), event.pivot.bar as f64);
            fp.values
                .insert("zigzag_confirmation_lag".into(), event.lag() as f64);
            fp.tags
                .insert("zigzag_pivot_kind".into(), event.pivot.kind.as_str().into());
            add_plots(&mut fp.plots, event, fp.close, "zigzag");
        }
        self.bar += 1;
    }
}

/// Append standard ZigZag visuals: pivot line, pivot marker, and causal confirmation link.
pub fn add_plots(
    plots: &mut Vec<Plot>,
    event: PivotConfirmation,
    confirmation_price: f64,
    group: &str,
) {
    let color = match event.pivot.kind {
        PivotKind::High => "#d97706",
        PivotKind::Low => "#2563eb",
    };
    if let Some(previous) = event.previous {
        plots.push(Plot::LineSegment {
            from_bars_back: event.confirmed_bar.saturating_sub(previous.bar),
            from_price: previous.price,
            to_bars_back: event.lag(),
            to_price: event.pivot.price,
            color: "#7c3aed".into(),
            dashed: false,
            text: String::new(),
            group: Some(group.into()),
        });
    }
    plots.push(Plot::MarkerAt {
        bars_back: event.lag(),
        price: event.pivot.price,
        shape: "diamond".into(),
        color: color.into(),
        text: format!("zz {}", event.pivot.kind.as_str()),
        text_dx: None,
        text_dy: None,
        group: Some(group.into()),
    });
    if event.lag() > 0 {
        plots.push(Plot::LineSegment {
            from_bars_back: event.lag(),
            from_price: event.pivot.price,
            to_bars_back: 0,
            to_price: confirmation_price,
            color: color.into(),
            dashed: true,
            text: format!(
                "{} confirmed +{} bars",
                event.pivot.kind.as_str(),
                event.lag()
            ),
            group: Some(group.into()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_pivot_and_confirmation_bars() {
        let mut zz = ZigZag::new(5.0);
        assert!(zz.update(0, 100.0, 100.0).is_none());
        let first = zz.update(1, 106.0, 101.0).unwrap();
        assert_eq!(
            first.pivot,
            Pivot {
                kind: PivotKind::Low,
                price: 100.0,
                bar: 0
            }
        );
        assert_eq!((first.confirmed_bar, first.lag()), (1, 1));
        assert!(zz.update(2, 110.0, 108.0).is_none());
        let high = zz.update(3, 109.0, 104.0).unwrap();
        assert_eq!(
            high.pivot,
            Pivot {
                kind: PivotKind::High,
                price: 110.0,
                bar: 2
            }
        );
        assert_eq!(high.confirmed_bar, 3);
    }

    #[test]
    fn wide_bar_can_confirm_its_new_extreme() {
        let mut zz = ZigZag::new(5.0);
        zz.update(0, 100.0, 100.0);
        zz.update(1, 106.0, 101.0).unwrap();
        let high = zz.update(2, 112.0, 106.0).unwrap();
        assert_eq!(
            (high.pivot.price, high.pivot.bar, high.confirmed_bar),
            (112.0, 2, 2)
        );
    }

    #[test]
    fn filtered_public_pivots_always_alternate() {
        let mut zz = ZigZag::with_filters(5.0, 0.0, 3);
        let bars = [
            (0, 100.0, 100.0),
            (1, 106.0, 100.0),
            (4, 110.0, 109.0),
            (5, 110.0, 104.0),
            (6, 108.0, 103.0),
            (7, 109.0, 103.0),
            (8, 106.0, 102.0),
            (9, 108.0, 102.0),
            (11, 114.0, 113.0),
            (12, 114.0, 109.0),
        ];
        let pivots: Vec<_> = bars
            .into_iter()
            .filter_map(|(bar, high, low)| zz.update(bar, high, low))
            .collect();
        assert!(pivots
            .windows(2)
            .all(|pair| pair[0].pivot.kind != pair[1].pivot.kind));
    }
}
