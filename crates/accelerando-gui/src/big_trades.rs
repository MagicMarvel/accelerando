use accelerando_core::{Footprint, OrderFlowEvent, Plot, Side};
use accelerando_example_indicators::big_trades::{
    CumulativeTrade, CumulativeTrades, DEFAULT_CUMULATIVE_WINDOW_MS, DEFAULT_MIN_TRADE_SIZE,
};
use eframe::egui::{self, Color32};

use crate::{market_history::TradeTape, overlay::parse_color};

const BUY_COLOR: Color32 = Color32::from_rgb(109, 109, 255);
const SELL_COLOR: Color32 = Color32::from_rgb(255, 109, 109);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MarkerShape {
    #[default]
    Circle,
    Square,
    Triangle,
}

impl MarkerShape {
    fn label(self) -> &'static str {
        match self {
            Self::Circle => "Circle",
            Self::Square => "Square",
            Self::Triangle => "Triangle",
        }
    }
}

pub(crate) struct BigTradeSettings {
    pub visible: bool,
    pub settings_open: bool,
    pub min_volume: f64,
    pub max_volume: f64,
    pub shape: MarkerShape,
    pub show_value: bool,
    fixed_size: bool,
    base_size: f32,
    min_size: f32,
    max_size: f32,
    buy_color: Color32,
    sell_color: Color32,
    transparency: u8,
}

impl Default for BigTradeSettings {
    fn default() -> Self {
        Self {
            visible: true,
            settings_open: false,
            min_volume: DEFAULT_MIN_TRADE_SIZE,
            max_volume: 0.0,
            fixed_size: false,
            base_size: 10.0,
            min_size: 8.0,
            max_size: 50.0,
            shape: MarkerShape::Circle,
            show_value: true,
            buy_color: BUY_COLOR,
            sell_color: SELL_COLOR,
            transparency: 57,
        }
    }
}

impl BigTradeSettings {
    pub fn radius(&self, volume: f64) -> f32 {
        let size = if self.fixed_size {
            f64::from(self.base_size)
        } else {
            let min_volume = if self.min_volume == 0.0 {
                1.0
            } else {
                self.min_volume
            };
            (volume / min_volume * f64::from(self.base_size)).trunc()
        };
        (size as f32).clamp(self.min_size, self.max_size) * 0.5
    }

    pub fn color(&self, marker: &Marker) -> Color32 {
        match marker.side {
            Side::Buy => self.buy_color,
            Side::Sell => self.sell_color,
            Side::Unknown => marker.saved_color,
        }
    }

    pub fn opacity(&self) -> u8 {
        ((100 - u16::from(self.transparency)) * 255 / 100) as u8
    }

    pub fn controls(&mut self, ui: &mut egui::Ui, overlay: &BigTradeOverlay) {
        ui.set_min_width(270.0);
        egui::Grid::new("big_trade_settings")
            .num_columns(2)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.label("Min volume");
                let max = if self.max_volume > 0.0 {
                    self.max_volume
                } else {
                    f64::MAX
                };
                ui.add(egui::DragValue::new(&mut self.min_volume).range(0.0..=max))
                    .on_hover_text(overlay.threshold_hint());
                ui.end_row();

                ui.label("Max volume");
                let max = ui
                    .add(egui::DragValue::new(&mut self.max_volume).range(0.0..=f64::MAX))
                    .on_hover_text("0 = unlimited");
                if max.changed() && self.max_volume > 0.0 {
                    self.max_volume = self.max_volume.max(self.min_volume);
                }
                ui.end_row();

                ui.label("Base size");
                ui.add(
                    egui::DragValue::new(&mut self.base_size)
                        .range(1.0..=100.0)
                        .suffix(" px"),
                )
                .on_hover_text(
                    "Size at the minimum volume. Double the volume doubles the size until it reaches Max size.",
                );
                ui.end_row();
                ui.label("Min size");
                ui.add(
                    egui::DragValue::new(&mut self.min_size)
                        .range(1.0..=self.max_size)
                        .suffix(" px"),
                );
                ui.end_row();
                ui.label("Max size");
                ui.add(
                    egui::DragValue::new(&mut self.max_size)
                        .range(self.min_size..=100.0)
                        .suffix(" px"),
                );
                ui.end_row();
                ui.label("Sizing");
                ui.checkbox(&mut self.fixed_size, "Fixed size");
                ui.end_row();

                ui.label("Shape");
                egui::ComboBox::from_id_salt("marker_shape")
                    .selected_text(self.shape.label())
                    .show_ui(ui, |ui| {
                        for shape in [MarkerShape::Circle, MarkerShape::Square, MarkerShape::Triangle] {
                            ui.selectable_value(&mut self.shape, shape, shape.label());
                        }
                    });
                ui.end_row();
                ui.label("Volume label");
                ui.checkbox(&mut self.show_value, "Inside marker");
                ui.end_row();

                ui.label("Buy color");
                egui::color_picker::color_edit_button_srgba(
                    ui,
                    &mut self.buy_color,
                    egui::color_picker::Alpha::Opaque,
                );
                ui.end_row();
                ui.label("Sell color");
                egui::color_picker::color_edit_button_srgba(
                    ui,
                    &mut self.sell_color,
                    egui::color_picker::Alpha::Opaque,
                );
                ui.end_row();
                ui.label("Transparency");
                ui.add(egui::Slider::new(&mut self.transparency, 0..=100).suffix("%"));
                ui.end_row();
            });
    }
}

pub(crate) struct Marker {
    pub ts_ns: i64,
    pub price: f64,
    pub volume: f64,
    side: Side,
    saved_color: Color32,
}

pub(crate) struct BigTradeOverlay {
    raw_trades: bool,
    markers: Vec<Marker>,
}

impl BigTradeOverlay {
    pub fn build(tape: Option<&TradeTape>, bars: &[Footprint]) -> Self {
        if let Some(tape) = tape {
            return Self::from_events(tape.events());
        }
        let mut markers = Vec::new();
        for bar in bars {
            for plot in &bar.plots {
                if let Plot::Marker {
                    price,
                    color,
                    text,
                    group,
                    ..
                } = plot
                {
                    if group.as_deref() == Some("big_trades") {
                        if let Ok(volume) = text.parse::<f64>() {
                            let saved_color = parse_color(color).unwrap_or(Color32::WHITE);
                            markers.push(Marker {
                                ts_ns: bar.ts_last_ns,
                                price: *price,
                                volume,
                                side: match saved_color {
                                    BUY_COLOR => Side::Buy,
                                    SELL_COLOR => Side::Sell,
                                    _ => Side::Unknown,
                                },
                                saved_color,
                            });
                        }
                    }
                }
            }
        }
        markers.sort_by_key(|marker| marker.ts_ns);
        Self {
            raw_trades: false,
            markers,
        }
    }

    fn from_events(events: &[OrderFlowEvent]) -> Self {
        let mut cumulative = CumulativeTrades::default();
        let mut markers = Vec::new();
        let mut add = |trade: CumulativeTrade| {
            markers.push(Marker {
                ts_ns: trade.last_ns,
                price: trade.midpoint_price(),
                volume: trade.total,
                side: trade.side,
                saved_color: Color32::WHITE,
            });
        };
        for event in events {
            let completed = match *event {
                OrderFlowEvent::Trade {
                    ts_ns,
                    price,
                    size,
                    aggressor,
                } => cumulative.push(ts_ns, price, size, aggressor, DEFAULT_CUMULATIVE_WINDOW_MS),
                OrderFlowEvent::Contract { .. } => cumulative.take(),
                _ => None,
            };
            if let Some(trade) = completed {
                add(trade);
            }
        }
        if let Some(trade) = cumulative.take() {
            add(trade);
        }
        Self {
            raw_trades: true,
            markers,
        }
    }

    pub fn available(&self) -> bool {
        self.raw_trades || !self.markers.is_empty()
    }

    pub fn threshold_hint(&self) -> &'static str {
        if self.raw_trades {
            "Minimum cumulative trade volume to show. Changes apply to the chart immediately."
        } else {
            "Filters saved Big Trades markers. Raw trades are needed to recover smaller executions."
        }
    }

    pub fn visible(
        &self,
        from_ns: i64,
        until_ns: i64,
        replay_ns: Option<i64>,
        settings: &BigTradeSettings,
    ) -> impl Iterator<Item = &Marker> {
        let start = self
            .markers
            .partition_point(|marker| marker.ts_ns < from_ns);
        let cutoff = replay_ns.map_or(until_ns, |clock| clock.min(until_ns));
        let end = self
            .markers
            .partition_point(|marker| marker.ts_ns <= cutoff);
        self.markers[start..end.max(start)].iter().filter({
            let (min, max) = (settings.min_volume, settings.max_volume);
            move |marker| marker.volume >= min && (max == 0.0 || marker.volume <= max)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_use_the_swept_price_midpoint_for_buys_and_sells() {
        let print = |ts_ns, price, size, aggressor| OrderFlowEvent::Trade {
            ts_ns,
            price,
            size,
            aggressor,
        };
        let overlay = BigTradeOverlay::from_events(&[
            print(0, 100.0, 10.0, Side::Buy),
            print(100_000, 100.25, 30.0, Side::Buy),
            print(200_000, 101.0, 40.0, Side::Sell),
            print(300_000, 100.5, 10.0, Side::Sell),
            print(400_000, 101.5, 50.0, Side::Buy),
        ]);
        let settings = BigTradeSettings::default();
        let markers: Vec<_> = overlay
            .visible(0, 400_000, None, &settings)
            .map(|marker| (marker.ts_ns, marker.price))
            .collect();
        assert_eq!(markers, vec![(100_000, 100.125), (300_000, 100.75), (400_000, 101.5)]);
    }

    #[test]
    fn threshold_can_reveal_smaller_sweeps_without_rebuilding_and_respects_replay() {
        let print = |ts_ns, price, size| OrderFlowEvent::Trade {
            ts_ns,
            price,
            size,
            aggressor: Side::Buy,
        };
        let overlay = BigTradeOverlay::from_events(&[
            print(0, 100.0, 12.0),
            print(500_000, 100.25, 18.0),
            print(2_000_000, 100.50, 10.0),
        ]);
        let volumes = |threshold, replay| {
            let settings = BigTradeSettings {
                min_volume: threshold,
                ..Default::default()
            };
            overlay
                .visible(0, 2_000_000, replay, &settings)
                .map(|marker| marker.volume)
                .collect::<Vec<_>>()
        };
        assert_eq!(volumes(25.0, None), vec![30.0]);
        assert_eq!(volumes(10.0, None), vec![30.0, 10.0]);
        assert!(volumes(10.0, Some(0)).is_empty());
        assert_eq!(volumes(10.0, Some(500_000)), vec![30.0]);
    }

    #[test]
    fn saved_marker_volume_is_filterable_without_the_tape() {
        let mut bar = Footprint::seed(1, 100.0);
        bar.plots.push(Plot::Marker {
            price: 100.0,
            shape: "circle".into(),
            color: "#6d6dff".into(),
            text: "27.5".into(),
            text_dx: None,
            text_dy: None,
            group: Some("big_trades".into()),
        });
        let overlay = BigTradeOverlay::build(None, &[bar]);
        assert!(overlay.available());
        let mut settings = BigTradeSettings {
            min_volume: 27.5,
            max_volume: 27.5,
            ..Default::default()
        };
        let marker = overlay.visible(0, 2, None, &settings).next().unwrap();
        settings.buy_color = Color32::GREEN;
        assert_eq!(settings.color(marker), Color32::GREEN);
        settings.max_volume = 27.0;
        assert_eq!(overlay.visible(0, 2, None, &settings).count(), 0);
    }

    #[test]
    fn bubble_size_scales_within_bounds_or_uses_the_fixed_diameter() {
        let mut settings = BigTradeSettings::default();
        assert_eq!(settings.radius(25.0), 5.0);
        assert_eq!(settings.radius(50.0), 10.0);
        assert_eq!(settings.radius(100.0), 20.0);
        assert_eq!(settings.radius(1.0), 4.0);
        assert_eq!(settings.radius(f64::MAX), 25.0);
        settings.fixed_size = true;
        assert_eq!(settings.radius(f64::MAX), 5.0);
        settings.min_size = 12.0;
        assert_eq!(settings.radius(f64::MAX), 6.0);
        settings.transparency = 100;
        assert_eq!(settings.opacity(), 0);
    }
}
