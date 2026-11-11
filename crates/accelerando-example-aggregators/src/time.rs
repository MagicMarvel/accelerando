//! Fixed-time footprint aggregator: every `bar_secs` of wall-clock time closes one footprint.

use accelerando_core::{
    Configurable, EventInterest, Footprint, FootprintAggregator, OrderFlowEvent,
    Side,
};

/// Aggregates trades into fixed-duration footprints with a full bid/ask ladder.
#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct TimeAggregator {
    #[param(default = 300, min = 5, max = 3600, step = 5)]
    bar_secs: i64,
    tick_size: f64,
    current: Option<(i64, Footprint)>, // (bucket, footprint)
}

impl TimeAggregator {
    fn init(&mut self) {
        self.bar_secs = self.bar_secs.max(1);
        self.tick_size = 0.25;
    }
}

impl FootprintAggregator for TimeAggregator {
    fn event_interest(&self) -> EventInterest {
        EventInterest::CONTRACT.union(EventInterest::TRADE)
    }

    fn on_event(&mut self, ev: &OrderFlowEvent) -> Option<Footprint> {
        match *ev {
            OrderFlowEvent::Contract { tick_size, .. } => {
                if tick_size > 0.0 {
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
                let bucket = (ts_ns / 1_000_000_000) / self.bar_secs;
                let buy = matches!(aggressor, Side::Buy);
                let mut completed = None;
                match self.current.as_mut() {
                    Some((b, _)) if *b == bucket => {}
                    Some(_) => {
                        let (_, mut fp) = self.current.take().unwrap();
                        fp.finalize_ladder();
                        completed = Some(fp);
                        self.current = Some((bucket, Footprint::seed(ts_ns, price)));
                    }
                    None => {
                        self.current = Some((bucket, Footprint::seed(ts_ns, price)));
                    }
                }
                let (_, fp) = self.current.as_mut().unwrap();
                if aggressor == Side::Unknown {
                    fp.add_unknown_trade(ts_ns, price, size);
                } else {
                    fp.add_trade(ts_ns, price, size, buy, self.tick_size);
                }
                completed
            }
            // L2 events do not affect time-footprint construction in this MVP.
            _ => None,
        }
    }

    fn flush(&mut self) -> Option<Footprint> {
        self.current.take().map(|(_, mut fp)| {
            fp.finalize_ladder();
            fp
        })
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use accelerando_core::Params;
    #[test]
    fn boundary_print_and_unknown_volume_are_not_prior_minute_delta() {
        let mut p = Params::default();
        p.set("bar_secs", accelerando_core::ParamValue::Int(60));
        let mut a = TimeAggregator::from_params(&p);
        let t = |ts, price, side| OrderFlowEvent::Trade {
            ts_ns: ts,
            price,
            size: 10.0,
            aggressor: side,
        };
        a.on_event(&t(0, 100.0, Side::Buy));
        a.on_event(&t(1, 101.0, Side::Unknown));
        let f = a.on_event(&t(60_000_000_000, 102.0, Side::Sell)).unwrap();
        assert_eq!(f.delta, 10.0);
        assert_eq!(f.volume, 20.0);
        assert_eq!(f.close, 101.0);
        assert_eq!(f.trades, 2);
        assert_eq!(a.flush().unwrap().delta, -10.0);
    }
}
