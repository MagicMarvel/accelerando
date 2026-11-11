//! Fixed-volume footprint aggregator: closes a footprint once it has traded `volume` contracts.

use accelerando_core::{
    Configurable, EventInterest, Footprint, FootprintAggregator, OrderFlowEvent, ParamSpec, Params,
    Side,
};

/// Aggregates trades into footprints of (at least) `volume` traded contracts each. The trade that
/// reaches the target belongs to the closing bar; it is not split across two bars.
pub struct VolumeAggregator {
    volume: f64,
    tick_size: f64,
    current: Option<Footprint>,
}

impl Configurable for VolumeAggregator {
    fn param_spec() -> ParamSpec {
        ParamSpec::new().int("volume", 1000, 10, 1_000_000, 10)
    }

    fn from_params(params: &Params) -> Self {
        Self {
            volume: params.int("volume", 1000).max(1) as f64,
            tick_size: 0.25,
            current: None,
        }
    }
}

impl FootprintAggregator for VolumeAggregator {
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
                let fp = self
                    .current
                    .get_or_insert_with(|| Footprint::seed(ts_ns, price));
                if aggressor == Side::Unknown {
                    fp.add_unknown_trade(ts_ns, price, size);
                } else {
                    fp.add_trade(ts_ns, price, size, aggressor == Side::Buy, self.tick_size);
                }
                if fp.volume >= self.volume {
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
    use accelerando_core::ParamValue;

    #[test]
    fn closes_each_bar_at_the_volume_target() {
        let mut params = Params::default();
        params.set("volume", ParamValue::Int(10));
        let mut agg = VolumeAggregator::from_params(&params);
        let trade = |ts: i64, size: f64| OrderFlowEvent::Trade {
            ts_ns: ts,
            price: 100.0,
            size,
            aggressor: Side::Buy,
        };
        assert!(agg.on_event(&trade(1, 4.0)).is_none());
        let bar = agg.on_event(&trade(2, 7.0)).expect("target reached");
        assert_eq!(bar.volume, 11.0);
        assert!(agg.on_event(&trade(3, 3.0)).is_none());
        assert_eq!(agg.flush().map(|f| f.volume), Some(3.0));
    }
}
