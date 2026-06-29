//! Fixed-range footprint aggregator: closes a footprint once its high-low span reaches
//! `range_ticks` minimum price increments.

use accelerando_core::{
    Configurable, EventInterest, Footprint, FootprintAggregator, OrderFlowEvent, ParamSpec, Params,
    Side,
};

/// Aggregates trades into footprints bounded by price range rather than wall-clock time.
pub struct RangeAggregator {
    range_ticks: i64,
    tick_size: f64,
    current: Option<Footprint>,
}

impl Configurable for RangeAggregator {
    fn param_spec() -> ParamSpec {
        ParamSpec::new().int("range_ticks", 30, 1, 400, 1)
    }

    fn from_params(params: &Params) -> Self {
        Self {
            range_ticks: params.int("range_ticks", 30).max(1),
            tick_size: 0.25,
            current: None,
        }
    }
}

impl FootprintAggregator for RangeAggregator {
    fn event_interest(&self) -> EventInterest {
        EventInterest::CONTRACT.union(EventInterest::TRADE)
    }

    fn on_event(&mut self, ev: &OrderFlowEvent) -> Option<Footprint> {
        match *ev {
            OrderFlowEvent::Contract { tick_size, .. } => {
                if tick_size > 0.0 && tick_size.is_finite() {
                    self.tick_size = tick_size;
                }
                None
            }
            OrderFlowEvent::Trade {
                ts_ns,
                price,
                size,
                aggressor,
            } => {
                if self.current.is_none() {
                    self.current = Some(Footprint::seed(ts_ns, price));
                }

                let target = self.range_ticks as f64 * self.tick_size.max(f64::EPSILON);
                let buy = matches!(aggressor, Side::Buy);
                let fp = self.current.as_mut().unwrap();
                fp.add_trade(ts_ns, price, size, buy, self.tick_size);

                if fp.high - fp.low >= target {
                    let mut completed = self.current.take().unwrap();
                    completed.finalize_ladder();
                    Some(completed)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn flush(&mut self) -> Option<Footprint> {
        self.current.take().map(|mut fp| {
            fp.finalize_ladder();
            fp
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(ts_ns: i64, price: f64) -> OrderFlowEvent {
        OrderFlowEvent::Trade {
            ts_ns,
            price,
            size: 1.0,
            aggressor: Side::Buy,
        }
    }

    #[test]
    fn closes_when_range_ticks_are_reached() {
        let mut agg = RangeAggregator::from_params(&Params::default());
        agg.on_event(&OrderFlowEvent::Contract {
            tick_size: 0.25,
            multiplier: 1.0,
        });

        assert!(agg.on_event(&trade(1, 100.0)).is_none());
        assert!(agg.on_event(&trade(2, 107.25)).is_none());
        let fp = agg.on_event(&trade(3, 107.5)).unwrap();

        assert_eq!(fp.open, 100.0);
        assert_eq!(fp.close, 107.5);
        assert_eq!(fp.trades, 3);
        assert!(agg.flush().is_none());
    }

    #[test]
    fn flushes_partial_range() {
        let mut agg = RangeAggregator::from_params(&Params::default());
        agg.on_event(&OrderFlowEvent::Contract {
            tick_size: 0.25,
            multiplier: 1.0,
        });
        assert!(agg.on_event(&trade(1, 100.0)).is_none());

        let fp = agg.flush().unwrap();

        assert_eq!(fp.open, 100.0);
        assert_eq!(fp.close, 100.0);
        assert_eq!(fp.poc, 100.0);
    }
}
