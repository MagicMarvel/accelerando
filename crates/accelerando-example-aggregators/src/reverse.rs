//! Directional reversal footprints. A retracement from the running extreme
//! closes the current footprint and starts the opposite one with the trigger trade.

use accelerando_core::{
    Configurable, EventInterest, Footprint, FootprintAggregator, OrderFlowEvent,
    Side,
};

#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct ReverseAggregator {
    #[param(default = 16, min = 1, max = 400)]
    reversal_ticks: i64,
    tick_size: f64,
    current: Option<Footprint>,
    direction: Option<Direction>,
}

impl ReverseAggregator {
    fn init(&mut self) {
        self.reversal_ticks = self.reversal_ticks.max(1);
        self.tick_size = 0.25;
    }
}

impl FootprintAggregator for ReverseAggregator {
    fn event_interest(&self) -> EventInterest {
        EventInterest::CONTRACT.union(EventInterest::TRADE)
    }

    fn on_event(&mut self, event: &OrderFlowEvent) -> Option<Footprint> {
        match *event {
            OrderFlowEvent::Contract { tick_size, .. } => {
                let previous = self.flush();
                if tick_size.is_finite() && tick_size > 0.0 {
                    self.tick_size = tick_size;
                }
                previous
            }
            OrderFlowEvent::Trade { ts_ns, price, size, aggressor } => {
                let tick_size = self.tick_size;
                let distance_ticks = |distance: f64| (distance / tick_size).round() as i64;
                let reversal = self.current.as_ref().and_then(|bar| match self.direction {
                    Some(Direction::Up)
                        if distance_ticks(bar.high - price) >= self.reversal_ticks =>
                    {
                        Some(Direction::Down)
                    }
                    Some(Direction::Down)
                        if distance_ticks(price - bar.low) >= self.reversal_ticks =>
                    {
                        Some(Direction::Up)
                    }
                    _ => None,
                });
                let completed = reversal.and_then(|direction| {
                    let mut bar = self.flush()?;
                    bar.tags.insert("reverse_complete".into(), "true".into());
                    self.direction = Some(direction);
                    Some(bar)
                });

                let bar = self.current.get_or_insert_with(|| Footprint::seed(ts_ns, price));
                match aggressor {
                    Side::Unknown => bar.add_unknown_trade(ts_ns, price, size),
                    side => bar.add_trade(ts_ns, price, size, side == Side::Buy, self.tick_size),
                }
                if self.direction.is_none() && distance_ticks(bar.high - bar.low) >= self.reversal_ticks {
                    self.direction = Some(if price == bar.high {
                        Direction::Up
                    } else {
                        Direction::Down
                    });
                }
                completed
            }
            _ => None,
        }
    }

    fn flush(&mut self) -> Option<Footprint> {
        self.direction = None;
        self.current.take().map(|mut bar| {
            bar.finalize_ladder();
            bar
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerando_core::Params;

    #[test]
    fn reverses_from_the_running_extreme_and_assigns_the_trigger_once() {
        let mut agg = ReverseAggregator::from_params(&Params::default());
        let mut trade = |price| agg.on_event(&OrderFlowEvent::Trade {
            ts_ns: 1,
            price,
            size: 1.0,
            aggressor: Side::Buy,
        });
        for price in [100.0, 104.0, 106.0, 102.25] {
            assert!(trade(price).is_none());
        }
        let up = trade(102.0).unwrap();
        assert_eq!((up.high, up.close, up.trades), (106.0, 102.25, 4));
        assert!(trade(99.0).is_none());
        assert!(trade(102.75).is_none());
        let down = trade(103.0).unwrap();
        assert_eq!((down.open, down.low, down.trades), (102.0, 99.0, 3));
        let tail = agg.flush().unwrap();
        assert_eq!((tail.open, tail.trades), (103.0, 1));
        assert!(!tail.tags.contains_key("reverse_complete"));
    }
}
