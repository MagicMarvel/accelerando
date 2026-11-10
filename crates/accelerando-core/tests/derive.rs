use accelerando_core::{
    Configurable, ParamRange, ParamValue, Params, SessionFilter, TradeDiscipline,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    Both,
    Long,
    Short,
}

impl std::str::FromStr for Side {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "both" => Ok(Self::Both),
            "long" => Ok(Self::Long),
            "short" => Ok(Self::Short),
            _ => Err(()),
        }
    }
}

#[derive(Configurable)]
#[configurable(init = Self::init)]
struct Demo {
    #[param(default = 8, min = 1, max = 100)]
    stop_ticks: u32,
    #[param(default = 1.5, min = 0.5, max = 4.0, step = 0.5)]
    target_r: f64,
    #[param(default = true)]
    exit_on_cross: bool,
    #[param(default = "both", choice = ["both", "long", "short"])]
    side: Side,
    #[param(choice = ["fast", "slow"])]
    mode: String,
    #[param(fixed, default = 0.25)]
    tick: f64,
    #[param(name = "lookback_bars", default = -3, min = -10, max = 10, step = 1)]
    lookback: i64,
    #[param(flatten)]
    session: SessionFilter,
    #[param(flatten)]
    discipline: TradeDiscipline,
    // state
    stop_distance: f64,
    counter: usize,
}

impl Demo {
    fn init(&mut self) {
        self.stop_distance = self.stop_ticks as f64 * self.tick;
    }
}

#[test]
fn spec_is_declared_once_from_the_struct() {
    let spec = Demo::param_spec();
    let names: Vec<&str> = spec.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        &names[..7],
        &["stop_ticks", "target_r", "exit_on_cross", "side", "mode", "tick", "lookback_bars"]
    );
    assert!(names.contains(&"exclude_first_minutes"));
    assert!(names.contains(&"max_trades_per_day"));
    let target = spec.entries.iter().find(|e| e.name == "target_r").unwrap();
    assert!(matches!(target.range, ParamRange::Float { step: Some(s), .. } if s == 0.5));
    let flag = spec.entries.iter().find(|e| e.name == "exit_on_cross").unwrap();
    assert!(matches!(flag.range, ParamRange::Int { min: 0, max: 1, step: 1 }));
    assert_eq!(flag.default, ParamValue::Int(1));
}

#[test]
fn defaults_overrides_state_and_init() {
    let d = Demo::build(&Params::default());
    assert_eq!((d.stop_ticks, d.target_r), (8, 1.5));
    assert!(d.exit_on_cross);
    assert_eq!(d.side, Side::Both);
    assert_eq!(d.mode, "fast");
    assert_eq!(d.lookback, -3);
    assert_eq!(d.stop_distance, 2.0);
    assert_eq!(d.counter, 0);
    assert!(d.discipline.one_position_at_a_time);

    let spec = Demo::param_spec();
    let p = spec
        .parse_assignments(&[
            "stop_ticks=12".into(),
            "exit_on_cross=off".into(),
            "side=short".into(),
            "max_trades_per_day=5".into(),
        ])
        .unwrap();
    let d = Demo::build(&p);
    assert_eq!((d.stop_ticks, d.exit_on_cross, d.side), (12, false, Side::Short));
    assert_eq!(d.stop_distance, 3.0);
    assert_eq!(d.discipline.max_trades_per_day, 5);
    assert!(d.session.allows(10 * 60));
}

#[test]
#[should_panic(expected = "valid options")]
fn bad_choice_still_fails_loudly() {
    Demo::build(&Params::default().with("side", "sideways"));
}
