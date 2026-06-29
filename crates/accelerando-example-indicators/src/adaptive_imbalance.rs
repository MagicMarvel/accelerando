use accelerando_core::{
    Configurable, Footprint, Indicator, ParamSpec, Params, Plot,
};

pub const BUY_EDGE_PRICE: &str = "adaptive_imbalance_buy_edge_price";
pub const BUY_EDGE_DELTA: &str = "adaptive_imbalance_buy_edge_delta";
pub const SELL_EDGE_PRICE: &str = "adaptive_imbalance_sell_edge_price";
pub const SELL_EDGE_DELTA: &str = "adaptive_imbalance_sell_edge_delta";
pub const NODE_THRESHOLD: &str = "adaptive_imbalance_node_threshold";
pub const BAR_THRESHOLD: &str = "adaptive_imbalance_bar_threshold";
pub const BUY_IMBALANCE: &str = "adaptive_imbalance_buy";
pub const SELL_IMBALANCE: &str = "adaptive_imbalance_sell";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdaptiveMode {
    Fixed,
    Adaptive,
}

#[derive(Clone, Copy, Debug)]
struct PriorStats {
    bar_delta_abs: f64,
    edge_node_delta_abs: f64,
}

#[derive(Clone, Copy, Debug)]
struct EdgeNode {
    price: f64,
    delta: f64,
}

/// Marks edge delta imbalances with either fixed or history-adaptive thresholds.
pub struct AdaptiveImbalance {
    mode: AdaptiveMode,
    tick: f64,
    edge_band_ticks: f64,
    lookback_bars: usize,
    sensitivity: f64,
    min_node_delta: f64,
    max_node_delta: f64,
    min_bar_delta: f64,
    max_bar_delta: f64,
    plot_markers: bool,
    prior: Vec<PriorStats>,
}

impl Configurable for AdaptiveImbalance {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .choice("mode", "adaptive", &["adaptive", "fixed"])
            .fixed_float("tick", 0.25)
            .int("edge_band_ticks", 16, 1, 120, 1)
            .int("lookback_bars", 20, 3, 200, 1)
            .float("sensitivity", 1.0, 0.0, 4.0)
            .float("min_node_delta", 90.0, 1.0, 5000.0)
            .float("max_node_delta", 1200.0, 1.0, 10000.0)
            .float("min_bar_delta", 100.0, 1.0, 8000.0)
            .float("max_bar_delta", 2000.0, 1.0, 16000.0)
            .int("plot_markers", 1, 0, 1, 1)
    }

    fn from_params(p: &Params) -> Self {
        let min_node_delta = p.float("min_node_delta", 90.0).max(1.0);
        let min_bar_delta = p.float("min_bar_delta", 100.0).max(1.0);
        Self {
            mode: match p.str("mode", "adaptive").as_str() {
                "fixed" => AdaptiveMode::Fixed,
                _ => AdaptiveMode::Adaptive,
            },
            tick: p.float("tick", 0.25).max(f64::EPSILON),
            edge_band_ticks: p.int("edge_band_ticks", 16) as f64,
            lookback_bars: p.usize("lookback_bars", 20).max(1),
            sensitivity: p.float("sensitivity", 1.0).max(0.0),
            min_node_delta,
            max_node_delta: p.float("max_node_delta", 1200.0).max(min_node_delta),
            min_bar_delta,
            max_bar_delta: p.float("max_bar_delta", 2000.0).max(min_bar_delta),
            plot_markers: p.int("plot_markers", 1) != 0,
            prior: Vec::new(),
        }
    }
}

impl Indicator for AdaptiveImbalance {
    fn name(&self) -> &str {
        "adaptive_imbalance"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        let node_threshold = self.node_threshold();
        let bar_threshold = self.bar_threshold();
        let buy_edge = self.best_buy_edge(fp);
        let sell_edge = self.best_sell_edge(fp);

        fp.values
            .insert(NODE_THRESHOLD.to_string(), node_threshold);
        fp.values.insert(BAR_THRESHOLD.to_string(), bar_threshold);
        fp.values.insert(
            BUY_EDGE_PRICE.to_string(),
            buy_edge.map(|node| node.price).unwrap_or(0.0),
        );
        fp.values.insert(
            BUY_EDGE_DELTA.to_string(),
            buy_edge.map(|node| node.delta).unwrap_or(0.0),
        );
        fp.values.insert(
            SELL_EDGE_PRICE.to_string(),
            sell_edge.map(|node| node.price).unwrap_or(0.0),
        );
        fp.values.insert(
            SELL_EDGE_DELTA.to_string(),
            sell_edge.map(|node| node.delta).unwrap_or(0.0),
        );

        let buy_imbalance = buy_edge
            .is_some_and(|node| node.delta >= node_threshold && fp.delta.abs() >= bar_threshold);
        let sell_imbalance = sell_edge
            .is_some_and(|node| -node.delta >= node_threshold && fp.delta.abs() >= bar_threshold);
        fp.values
            .insert(BUY_IMBALANCE.to_string(), if buy_imbalance { 1.0 } else { 0.0 });
        fp.values.insert(
            SELL_IMBALANCE.to_string(),
            if sell_imbalance { 1.0 } else { 0.0 },
        );

        if self.plot_markers {
            if buy_imbalance {
                if let Some(node) = buy_edge {
                    self.plot_marker(fp, node, true, node_threshold);
                }
            }
            if sell_imbalance {
                if let Some(node) = sell_edge {
                    self.plot_marker(fp, node, false, node_threshold);
                }
            }
        }

        self.push_prior(fp);
    }
}

impl AdaptiveImbalance {
    fn node_threshold(&self) -> f64 {
        self.threshold(
            |stats| stats.edge_node_delta_abs,
            self.min_node_delta,
            self.max_node_delta,
        )
    }

    fn bar_threshold(&self) -> f64 {
        self.threshold(
            |stats| stats.bar_delta_abs,
            self.min_bar_delta,
            self.max_bar_delta,
        )
    }

    fn threshold(&self, sample: impl Fn(&PriorStats) -> f64, min: f64, max: f64) -> f64 {
        if self.mode == AdaptiveMode::Fixed || self.prior.is_empty() {
            return min;
        }
        let values: Vec<f64> = self
            .prior
            .iter()
            .rev()
            .take(self.lookback_bars)
            .map(sample)
            .filter(|v| v.is_finite())
            .collect();
        if values.is_empty() {
            return min;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let var = values
            .iter()
            .map(|v| {
                let diff = *v - mean;
                diff * diff
            })
            .sum::<f64>()
            / values.len() as f64;
        (mean + self.sensitivity * var.sqrt()).clamp(min, max)
    }

    fn best_buy_edge(&self, fp: &Footprint) -> Option<EdgeNode> {
        fp.ladder
            .iter()
            .filter(|level| level.price >= fp.high - self.edge_band_ticks * self.tick)
            .map(|level| EdgeNode {
                price: level.price,
                delta: level.buy_vol - level.sell_vol,
            })
            .max_by(|a, b| a.delta.total_cmp(&b.delta))
    }

    fn best_sell_edge(&self, fp: &Footprint) -> Option<EdgeNode> {
        fp.ladder
            .iter()
            .filter(|level| level.price <= fp.low + self.edge_band_ticks * self.tick)
            .map(|level| EdgeNode {
                price: level.price,
                delta: level.buy_vol - level.sell_vol,
            })
            .min_by(|a, b| a.delta.total_cmp(&b.delta))
    }

    fn push_prior(&mut self, fp: &Footprint) {
        let buy = self
            .best_buy_edge(fp)
            .map(|node| node.delta.abs())
            .unwrap_or(0.0);
        let sell = self
            .best_sell_edge(fp)
            .map(|node| node.delta.abs())
            .unwrap_or(0.0);
        self.prior.push(PriorStats {
            bar_delta_abs: fp.delta.abs(),
            edge_node_delta_abs: buy.max(sell),
        });
        let max_keep = self.lookback_bars.saturating_mul(4).max(self.lookback_bars + 1);
        if self.prior.len() > max_keep {
            let remove = self.prior.len() - max_keep;
            self.prior.drain(0..remove);
        }
    }

    fn plot_marker(&self, fp: &mut Footprint, node: EdgeNode, is_buy: bool, threshold: f64) {
        let (color, text) = if is_buy {
            (
                "#6d6dff",
                format!("upper imbalance {:.0}/{:.0}", node.delta, threshold),
            )
        } else {
            (
                "#ff6d6d",
                format!("lower imbalance {:.0}/{:.0}", -node.delta, threshold),
            )
        };
        fp.plots.push(Plot::Marker {
            price: node.price,
            shape: "circle".to_string(),
            color: color.to_string(),
            text,
            text_dx: Some(16.0),
            text_dy: None,
            group: Some("imbalance".to_string()),
        });
    }
}
