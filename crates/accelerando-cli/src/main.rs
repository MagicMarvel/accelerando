//! Accelerando CLI — `run | hyperopt | serve`. The world has been accelerated.

mod config;
mod pipeline;
mod studio;

use accelerando_core::{run_backtest, BacktestResult, Metrics, Params};
use accelerando_hyperopt::{search, Algo, CpuEvaluator};

use config::RunConfig;
use studio::StudioConfig;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    let result = match cmd {
        "run" => cmd_run(rest),
        "hyperopt" => cmd_hyperopt(rest),
        "serve" => cmd_serve(rest),
        "studio" => cmd_studio(rest),
        "-h" | "--help" | "help" | "" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command: {other}\n\nTry `accelerando --help`.")),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "accelerando — high-speed footprint backtesting\n\
         \n\
         USAGE:\n\
           accelerando run      --config run.toml [--result result.json]\n\
           accelerando hyperopt --config run.toml [--algo random|grid] [--evals N] [--jobs M]\n\
                                [--seed S] [--objective sharpe|sortino|pnl|return|profit_factor]\n\
           accelerando serve    [--result result.json] [--port 8080]\n\
           accelerando studio   [--config run.toml] [--runs-dir runs] [--port 8080]\n\
                                interactive UI: edit params, run, watch progress, save/load runs\n"
    );
}

/// Minimal flag parser: returns the value following `--name`, if present.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    let cfg_path = flag(args, "--config").ok_or("run needs --config <path>")?;
    let cfg = RunConfig::load(cfg_path)?;
    let out = flag(args, "--result").map(str::to_string).unwrap_or(cfg.result.clone());

    eprintln!("running backtest from {cfg_path} ...");
    let t0 = std::time::Instant::now();
    let pipeline = pipeline::build_pipeline(&cfg, &Params::default(), cfg.keep_footprints)?;
    let result = run_backtest(pipeline);
    eprintln!(
        "done in {:.2}s — {} footprints, {} trades",
        t0.elapsed().as_secs_f64(),
        result.footprints.len(),
        result.trades.len()
    );

    print_metrics(&result.metrics);
    let json = serde_json::to_string(&result).map_err(|e| format!("serialize result: {e}"))?;
    std::fs::write(&out, json).map_err(|e| format!("write {out}: {e}"))?;
    eprintln!("wrote {out}  (view with: accelerando serve --result {out})");
    Ok(())
}

fn cmd_hyperopt(args: &[String]) -> Result<(), String> {
    let cfg_path = flag(args, "--config").ok_or("hyperopt needs --config <path>")?;
    let cfg = RunConfig::load(cfg_path)?;
    let algo = Algo::parse(flag(args, "--algo").unwrap_or("random"))
        .ok_or("--algo must be random or grid")?;
    let evals: usize = flag(args, "--evals").unwrap_or("64").parse().map_err(|_| "bad --evals")?;
    let jobs: usize = flag(args, "--jobs")
        .map(|s| s.parse().unwrap_or(0))
        .unwrap_or(0);
    let seed: u64 = flag(args, "--seed").unwrap_or("42").parse().map_err(|_| "bad --seed")?;
    let objective = flag(args, "--objective").unwrap_or("sharpe").to_string();

    let space = pipeline::build_search_space(&cfg)?;
    eprintln!(
        "hyperopt: {} tunable params, {} evals, algo {:?}, objective {}",
        space.dims.len(),
        evals,
        algo,
        objective
    );

    // Each candidate rebuilds and runs the pipeline (footprints dropped for speed).
    let cfg_ref = &cfg;
    let obj = objective.clone();
    let func = move |p: &Params| -> f64 {
        match pipeline::build_pipeline(cfg_ref, p, false) {
            Ok(pl) => objective_value(&run_backtest(pl).metrics, &obj),
            Err(_) => f64::NEG_INFINITY,
        }
    };
    let evaluator = CpuEvaluator { func };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs) // 0 => rayon default (all cores)
        .build()
        .map_err(|e| format!("thread pool: {e}"))?;

    let t0 = std::time::Instant::now();
    let report = pool.install(|| search(&space, algo, evals, seed, &evaluator));
    let secs = t0.elapsed().as_secs_f64();

    let finite = report.trials.iter().filter(|t| t.score.is_finite()).count();
    eprintln!(
        "evaluated {} candidates in {:.2}s ({:.1}/s, {} valid)",
        report.trials.len(),
        secs,
        report.trials.len() as f64 / secs.max(1e-9),
        finite
    );
    println!("\nbest {objective} = {:.4}", report.best.score);
    println!("best params:");
    let mut keys: Vec<_> = report.best.params.0.keys().collect();
    keys.sort();
    for k in keys {
        println!("  {k} = {:?}", report.best.params.0[k]);
    }
    Ok(())
}

fn cmd_serve(args: &[String]) -> Result<(), String> {
    let path = flag(args, "--result").unwrap_or("result.json");
    let port: u16 = flag(args, "--port").unwrap_or("8080").parse().map_err(|_| "bad --port")?;
    let json = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let result: BacktestResult =
        serde_json::from_str(&json).map_err(|e| format!("parse {path}: {e}"))?;
    accelerando_web::serve(&result, port).map_err(|e| format!("serve: {e}"))
}

fn cmd_studio(args: &[String]) -> Result<(), String> {
    let port: u16 = flag(args, "--port").unwrap_or("8080").parse().map_err(|_| "bad --port")?;
    let runs_dir = std::path::PathBuf::from(flag(args, "--runs-dir").unwrap_or("runs"));
    let seed = match flag(args, "--config") {
        Some(p) => StudioConfig::from_run_config(&RunConfig::load(p)?),
        None => studio::default_config(),
    };
    studio::serve(seed, runs_dir, port)
}

fn objective_value(m: &Metrics, objective: &str) -> f64 {
    let v = match objective {
        "sharpe" => m.sharpe,
        "sortino" => m.sortino,
        "pnl" => m.net_pnl,
        "return" => m.return_pct,
        "profit_factor" => m.profit_factor,
        _ => m.sharpe,
    };
    if v.is_finite() {
        v
    } else {
        f64::NEG_INFINITY
    }
}

fn print_metrics(m: &Metrics) {
    println!("──────────────── metrics ────────────────");
    println!("  net pnl        {:>14.2}", m.net_pnl);
    println!("  return         {:>13.2}%", m.return_pct);
    println!("  trades         {:>14}", m.trades);
    println!("  win rate       {:>13.2}%", m.win_rate * 100.0);
    println!("  profit factor  {:>14.2}", m.profit_factor);
    println!("  expectancy     {:>14.2}", m.expectancy);
    println!("  avg win/loss   {:>9.2} / {:.2}", m.avg_win, m.avg_loss);
    println!("  sharpe         {:>14.2}", m.sharpe);
    println!("  sortino        {:>14.2}", m.sortino);
    println!("  max drawdown   {:>14.2}  ({:.2}%)", m.max_drawdown, m.max_drawdown_pct);
    println!("─────────────────────────────────────────");
}
