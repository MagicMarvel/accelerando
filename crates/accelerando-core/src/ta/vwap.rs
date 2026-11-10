//! Session-anchored VWAP.

use crate::footprint::Footprint;

/// VWAP that resets whenever the session key (usually the Eastern trading day from
/// [`crate::session::SessionClock`]) changes. Tracks a volume-weighted σ for bands.
#[derive(Clone, Debug)]
pub struct SessionVwap {
    session: i64,
    pv: f64,
    p2v: f64,
    volume: f64,
    last_price: f64,
}

impl Default for SessionVwap {
    fn default() -> Self {
        Self {
            session: i64::MIN,
            pv: 0.0,
            p2v: 0.0,
            volume: 0.0,
            last_price: f64::NAN,
        }
    }
}

impl SessionVwap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `volume` traded at `price` in `session`. A new session key resets the accumulators.
    pub fn add(&mut self, session: i64, price: f64, volume: f64) {
        if session != self.session {
            self.session = session;
            self.pv = 0.0;
            self.p2v = 0.0;
            self.volume = 0.0;
        }
        self.last_price = price;
        if volume > 0.0 {
            self.pv += price * volume;
            self.p2v += price * price * volume;
            self.volume += volume;
        }
    }

    /// Bar-level update using the typical price `(H+L+C)/3` × bar volume.
    pub fn update_bar(&mut self, session: i64, fp: &Footprint) -> f64 {
        self.add(session, (fp.high + fp.low + fp.close) / 3.0, fp.volume);
        self.last_price = fp.close;
        self.value()
    }

    /// Exact update from every ladder level of the footprint.
    pub fn update_ladder(&mut self, session: i64, fp: &Footprint) -> f64 {
        self.add(session, fp.close, 0.0);
        for level in &fp.ladder {
            self.add(session, level.price, level.buy_vol + level.sell_vol);
        }
        self.last_price = fp.close;
        self.value()
    }

    /// Current VWAP; falls back to the last price while the session has no volume.
    pub fn value(&self) -> f64 {
        if self.volume > 0.0 {
            self.pv / self.volume
        } else {
            self.last_price
        }
    }

    /// Volume-weighted standard deviation of price around the VWAP.
    pub fn std(&self) -> f64 {
        if self.volume <= 0.0 {
            return 0.0;
        }
        let mean = self.pv / self.volume;
        (self.p2v / self.volume - mean * mean).max(0.0).sqrt()
    }

    /// `vwap + k * σ` (use a negative `k` for lower bands).
    pub fn band(&self, k: f64) -> f64 {
        self.value() + k * self.std()
    }

    pub fn session_volume(&self) -> f64 {
        self.volume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resets_on_new_session_and_weights_by_volume() {
        let mut v = SessionVwap::new();
        v.add(1, 100.0, 1.0);
        v.add(1, 104.0, 3.0);
        assert_eq!(v.value(), 103.0);
        assert!((v.std() - 3.0f64.sqrt()).abs() < 1e-12);
        v.add(2, 50.0, 2.0);
        assert_eq!(v.value(), 50.0);
    }

    #[test]
    fn no_volume_falls_back_to_last_price() {
        let mut v = SessionVwap::new();
        let mut fp = Footprint::seed(0, 10.0);
        fp.high = 11.0;
        fp.low = 9.0;
        fp.close = 10.5;
        fp.volume = 0.0;
        assert_eq!(v.update_bar(1, &fp), 10.5);
    }
}
