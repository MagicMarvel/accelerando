//! Causal ZigZag chart indicator. The detector itself lives in `accelerando_core::ta::zigzag`
//! (re-exported here for compatibility).

use accelerando_core::{Configurable, Footprint, Indicator, Plot};

pub use accelerando_core::ta::zigzag::{Pivot, PivotConfirmation, PivotKind, ZigZag};

/// Framework indicator wrapper around [`ZigZag`]. Prices are configured in instrument points.
#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct ZigZagIndicator {
    #[param(default = 4.0, min = 0.000001, max = 10000.0)]
    reversal: f64,
    #[param(default = 0.0, min = 0.0, max = 10000.0)]
    min_leg: f64,
    #[param(default = 0, min = 0, max = 10000)]
    min_leg_bars: usize,
    detector: Option<ZigZag>,
    bar: usize,
}

impl ZigZagIndicator {
    fn init(&mut self) {
        self.detector = Some(ZigZag::with_filters(self.reversal, self.min_leg, self.min_leg_bars));
    }
}

impl Indicator for ZigZagIndicator {
    fn name(&self) -> &str {
        "zigzag"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        if let Some(event) = self.detector.as_mut().expect("built in init").update(self.bar, fp.high, fp.low) {
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
