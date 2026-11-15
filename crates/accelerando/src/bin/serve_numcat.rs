use std::env;
use std::path::Path;

use accelerando_core::{
    broker::BrokerConfig, engine::run_backtest, engine::Pipeline, Configurable, Params,
};

use accelerando_example_aggregators::TimeAggregator;
use accelerando_example_indicators::BigTrades;
use accelerando_example_sources::NumCatL2Source;
use accelerando_example_strategy::IndicatorCross;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    let mut path = "data/numcat_688184_2months_cached.json".to_string();
    // `--span <secs>` opens the window zoomed in on the last N seconds.
    let mut span: Option<f64> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                if i + 1 < args.len() {
                    path = args[i + 1].clone();
                    i += 1;
                }
            }
            "--span" => {
                if i + 1 < args.len() {
                    span = args[i + 1].parse().ok();
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if !Path::new(&path).exists() {
        eprintln!("Error: Data file not found at '{path}'");
        std::process::exit(1);
    }

    println!("Loading NumCat L2 tick dataset from '{path}'...");

    // Configure NumCat L2 Source
    let mut src_params = Params::default();
    src_params.set("path", accelerando_core::ParamValue::Str(path.clone().into()));
    src_params.set("tick_size", accelerando_core::ParamValue::Float(0.01));
    src_params.set("multiplier", accelerando_core::ParamValue::Float(100.0));
    src_params.set("infer_aggressor", accelerando_core::ParamValue::Int(1));

    let source = Box::new(NumCatL2Source::from_params(&src_params));



    // Configure 30-minute Time Aggregator (1800s)
    let mut agg_params = Params::default();
    agg_params.set("bar_secs", accelerando_core::ParamValue::Int(1800));
    let aggregator = Box::new(TimeAggregator::from_params(&agg_params));



    // Configure BigTrades indicator
    let mut ind_params = Params::default();
    ind_params.set("min_trade_size", accelerando_core::ParamValue::Float(50.0));
    let indicator = Box::new(BigTrades::from_params(&ind_params));

    // Configure IndicatorCross strategy
    let strategy = Box::new(IndicatorCross::from_params(&Params::default()));


    let pipeline = Pipeline {
        source,
        aggregator,
        indicators: vec![indicator],
        strategy,
        broker_cfg: BrokerConfig::default(),
        keep_footprints: true,
        keep_liquidity_heatmap: false,
    };

    println!("Running footprint engine over NumCat L2 events...");
    let result = run_backtest(pipeline);

    println!(
        "Backtest complete! Produced {} footprint bars, {} trade points.",
        result.footprints.len(),
        result.trades.len()
    );

    println!("Loading event-time depth for the native viewer...");
    let (trades, depth) = accelerando_core::prepare_market_history(
        Box::new(NumCatL2Source::from_params(&src_params)),
    );
    accelerando_gui::view_with_market_history(
        result,
        accelerando_gui::ViewerOptions {
            title: format!("Accelerando — {path}"),
            utc_offset_hours: Some(8.0),
            red_up: true,
            initial_span_secs: span,
            ..Default::default()
        },
        trades.ok().map(std::sync::Arc::new),
        depth,
    )?;

    Ok(())
}
