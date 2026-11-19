use accelerando_core::{
    Configurable, Footprint, Indicator, IndicatorRequest, OrderFlowEvent, Params, Side,
    StrategyRequirements,
};
use accelerando_example_indicators::{
    adaptive_imbalance::AdaptiveMode,
    big_trades::{BigTradeMode, ExecutionPrice, PriceLocation, ThresholdMode},
    economic_calendar::ImpactFilter,
    stacked_imbalance::VolumeMode,
    AdaptiveImbalanceRequest, BigTrades, BigTradesRequest, EconomicCalendarRequest, RegimeRequest,
    StackedImbalanceRequest, TechnicalIndicatorsRequest, ZigZagIndicatorRequest,
};

#[test]
fn typed_and_legacy_big_trades_produce_identical_results() {
    for price in [
        ExecutionPrice::Last,
        ExecutionPrice::Start,
        ExecutionPrice::WeightedAverage,
    ] {
        for location in [
            PriceLocation::Any,
            PriceLocation::AtHigh,
            PriceLocation::AtLow,
            PriceLocation::Body,
            PriceLocation::UpperWick,
            PriceLocation::LowerWick,
            PriceLocation::AtHighOrLow,
        ] {
            let mut typed = BigTradesRequest::new()
                .mode(BigTradeMode::Cumulative)
                .execution_price(price)
                .price_location(location)
                .threshold_mode(ThresholdMode::Fixed)
                .min_trade_size(25.0)
                .build();
            let mut legacy = BigTrades::build(
                &Params::default()
                    .with("mode", "cumulative")
                    .with("execution_price", price.as_str())
                    .with("price_location", location.as_str())
                    .with("threshold_mode", "fixed")
                    .with("min_trade_size", 25.0),
            );
            for (ts_ns, price, size) in [(0, 100.0, 10.0), (1, 101.0, 30.0)] {
                let event = OrderFlowEvent::Trade {
                    ts_ns,
                    price,
                    size,
                    aggressor: Side::Buy,
                };
                typed.on_event(&event);
                legacy.on_event(&event);
            }
            let mut a = Footprint::seed(0, 100.0);
            a.high = 101.0;
            a.close = 100.75;
            let mut b = a.clone();
            typed.on_footprint(&mut a, &[]);
            legacy.on_footprint(&mut b, &[]);
            assert_eq!(
                serde_json::to_value(a).unwrap(),
                serde_json::to_value(b).unwrap()
            );
        }
    }
}

#[test]
fn registry_boundary_preserves_names_and_option_values() {
    let requests = StrategyRequirements::new()
        .indicator(AdaptiveImbalanceRequest::new().mode(AdaptiveMode::Fixed))
        .indicator(StackedImbalanceRequest::new().volume_mode(VolumeMode::Percentile))
        .indicator(
            BigTradesRequest::new()
                .mode(BigTradeMode::Single)
                .execution_price(ExecutionPrice::Start),
        )
        .indicator(EconomicCalendarRequest::new().impact(ImpactFilter::All))
        .indicator(RegimeRequest::new())
        .indicator(TechnicalIndicatorsRequest::new())
        .indicator(ZigZagIndicatorRequest::new());
    let mut registry = accelerando_core::Registry::new();
    accelerando_example_indicators::register_all(&mut registry);
    for request in &requests.indicators {
        // Validation must accept the same serialized choices used by CLI/hyperopt.
        let json = serde_json::to_string(request).unwrap();
        let restored: IndicatorRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(*request, restored);
        if request.name != "economic_calendar" {
            assert_eq!(
                registry
                    .build_indicator(&request.name, &request.params)
                    .unwrap()
                    .name(),
                request.name
            );
        }
    }
    assert_eq!(requests.indicators[2].params.str("mode", ""), "single");
    assert_eq!(
        requests.indicators[2].params.str("execution_price", ""),
        "start"
    );
    let _ = AdaptiveImbalanceRequest::new()
        .edge_band_ticks(16)
        .mode(AdaptiveMode::Adaptive)
        .build();
    let _ = StackedImbalanceRequest::new()
        .volume_mode(VolumeMode::Fixed)
        .build();
    let _ = RegimeRequest::new().build();
    let _ = TechnicalIndicatorsRequest::new().build();
    let _ = ZigZagIndicatorRequest::new().build();
}

#[test]
#[should_panic(expected = "invalid params")]
fn typed_build_rejects_invalid_numeric_options() {
    BigTradesRequest::new().cumulative_window_ms(0).build();
}

#[test]
fn choices_reject_unknown_text_at_the_boundary() {
    assert!("weighted_averag".parse::<ExecutionPrice>().is_err());
    assert!("invalid".parse::<PriceLocation>().is_err());
    assert!("invalid".parse::<ImpactFilter>().is_err());
    assert_eq!(
        "weighted_average".parse::<ExecutionPrice>(),
        Ok(ExecutionPrice::WeightedAverage)
    );
}

// These conversions deliberately panic: direct construction must use the enum as-is.
enum DirectMode {
    On,
}
impl std::str::FromStr for DirectMode {
    type Err = String;
    fn from_str(_: &str) -> Result<Self, Self::Err> {
        panic!("unexpected enum parsing")
    }
}
impl From<DirectMode> for accelerando_core::ParamValue {
    fn from(_: DirectMode) -> Self {
        panic!("unexpected enum serialization")
    }
}
#[derive(Configurable)]
#[configurable(indicator = "direct", init = Self::init)]
struct Direct {
    #[param(default = "on", choice = ["on"])]
    mode: DirectMode,
    #[param(name = "count", default = 3, min = 1, max = 10)]
    count_value: u32,
    initialized: bool,
}
impl Direct {
    fn init(&mut self) {
        self.initialized = matches!(self.mode, DirectMode::On) && self.count_value == 5;
    }
}
#[test]
fn direct_build_uses_enums_without_parsing_or_serialization_and_runs_init() {
    let direct = DirectRequest::new().mode(DirectMode::On).count(5).build();
    assert!(direct.initialized);
}
