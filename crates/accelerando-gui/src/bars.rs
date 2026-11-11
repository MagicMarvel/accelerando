//! Chart bar types. The viewer can rebuild the displayed bars from raw trades in any of these
//! types without rerunning the backtest: trades, equity and strategy overlays are timestamped and
//! map onto whatever bars are shown.

use std::sync::Arc;

use accelerando_core::{
    Configurable, Footprint, FootprintAggregator, OrderFlowEvent, ParamValue, Params,
};
use accelerando_example_aggregators::{
    RangeAggregator, ReverseAggregator, TimeAggregator, VolumeAggregator,
};

/// How bars are cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BarKind {
    /// Fixed wall-clock duration; size in seconds.
    Time,
    /// Fixed high-low range; size in ticks.
    Range,
    /// Closes on a reversal of `size` ticks from the bar's extreme.
    Reverse,
    /// Fixed traded volume; size in contracts.
    Volume,
}

impl BarKind {
    pub const ALL: [BarKind; 4] = [BarKind::Time, BarKind::Range, BarKind::Reverse, BarKind::Volume];

    pub fn name(self) -> &'static str {
        match self {
            BarKind::Time => "Time",
            BarKind::Range => "Range",
            BarKind::Reverse => "Reverse",
            BarKind::Volume => "Volume",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            BarKind::Time => "seconds",
            BarKind::Range | BarKind::Reverse => "ticks",
            BarKind::Volume => "contracts",
        }
    }

    /// Common sizes offered as one-click presets.
    pub fn presets(self) -> &'static [u32] {
        match self {
            BarKind::Time => &[30, 60, 300, 900, 3600],
            BarKind::Range => &[4, 8, 16, 30, 60],
            BarKind::Reverse => &[4, 8, 16, 30, 60],
            BarKind::Volume => &[500, 1000, 2500, 5000, 10_000],
        }
    }

    fn agg_name(self) -> &'static str {
        match self {
            BarKind::Time => "time",
            BarKind::Range => "range",
            BarKind::Reverse => "reverse",
            BarKind::Volume => "volume",
        }
    }
}

/// One bar type and size, e.g. `reverse:16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BarSpec {
    pub kind: BarKind,
    pub size: u32,
}

impl BarSpec {
    pub fn new(kind: BarKind, size: u32) -> Self {
        Self { kind, size: size.max(1) }
    }

    /// Parse the CLI aggregator form: `time:60`, `range:30`, `reverse:16`, `volume:1000`.
    pub fn parse(spec: &str) -> Option<Self> {
        let (name, size) = spec.split_once(':')?;
        let kind = BarKind::ALL.into_iter().find(|k| k.agg_name() == name.trim())?;
        Some(Self::new(kind, size.trim().parse().ok()?))
    }

    /// Short human label: `1m`, `Range 30`, `Reverse 16`, `Vol 1000`.
    pub fn label(&self) -> String {
        match self.kind {
            BarKind::Time if self.size % 3600 == 0 => format!("{}h", self.size / 3600),
            BarKind::Time if self.size % 60 == 0 => format!("{}m", self.size / 60),
            BarKind::Time => format!("{}s", self.size),
            BarKind::Range => format!("Range {}", self.size),
            BarKind::Reverse => format!("Reverse {}", self.size),
            BarKind::Volume => format!("Vol {}", self.size),
        }
    }

    /// A fresh aggregator for this spec.
    pub fn aggregator(&self) -> Box<dyn FootprintAggregator> {
        let mut params = Params::default();
        let size = ParamValue::Int(self.size as i64);
        match self.kind {
            BarKind::Time => {
                params.set("bar_secs", size);
                Box::new(TimeAggregator::from_params(&params))
            }
            BarKind::Range => {
                params.set("range_ticks", size);
                Box::new(RangeAggregator::from_params(&params))
            }
            BarKind::Reverse => {
                params.set("reversal_ticks", size);
                Box::new(ReverseAggregator::from_params(&params))
            }
            BarKind::Volume => {
                params.set("volume", size);
                Box::new(VolumeAggregator::from_params(&params))
            }
        }
    }
}

/// Fold raw order-flow events (contract + trades) into footprints of type `spec`.
pub fn aggregate(events: &[OrderFlowEvent], spec: &BarSpec) -> Vec<Footprint> {
    let mut agg = spec.aggregator();
    let mut out = Vec::new();
    for ev in events {
        if let Some(fp) = agg.on_event(ev) {
            out.push(fp);
        }
    }
    out.extend(agg.flush());
    out
}

/// Host-supplied bar builder: returns the footprints for a spec (typically [`aggregate`] over raw
/// events the host already holds). Runs on a background thread.
pub type Rebar = Arc<dyn Fn(&BarSpec) -> Result<Vec<Footprint>, String> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_and_label() {
        assert_eq!(BarSpec::parse("reverse:16"), Some(BarSpec::new(BarKind::Reverse, 16)));
        assert_eq!(BarSpec::parse("time:60").map(|s| s.label()), Some("1m".to_string()));
        assert_eq!(BarSpec::parse("volume:500").map(|s| s.label()), Some("Vol 500".to_string()));
        assert_eq!(BarSpec::parse("tick:5"), None);
    }
}
