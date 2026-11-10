//! Causal ZigZag pivot detection.
//!
//! A pivot's `bar` is where the extreme occurred. `confirmed_bar` is the later bar whose reversal
//! first made that extreme knowable without looking into the future.


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
