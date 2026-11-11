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

#[derive(Configurable)]
struct Clock {
    #[param(time, default = "18:00", step = 5)]
    entry_start: u32,
    #[param(time, default = "16:55", min = "09:00", max = "17:00", step = 5)]
    entry_end: u32,
    #[param(time, fixed, default = "06:00")]
    session_start: i64,
}

#[test]
fn time_params_are_minutes_written_as_hhmm() {
    let c = Clock::build(&Params::default());
    assert_eq!((c.entry_start, c.entry_end, c.session_start), (1080, 1015, 360));
    let spec = Clock::param_spec();
    let end = spec.entries.iter().find(|e| e.name == "entry_end").unwrap();
    assert!(matches!(end.range, ParamRange::Int { min: 540, max: 1020, step: 5 }));
    let p = spec
        .parse_assignments(&["entry_start=09:35".to_string(), "entry_end=1000".to_string()])
        .unwrap();
    let c = Clock::build(&p);
    assert_eq!((c.entry_start, c.entry_end), (575, 1000));
    assert!(spec.parse_assignments(&["entry_end=08:00".to_string()]).is_err());
}

#[derive(Configurable)]
struct Numbers {
    /// A choice on a numeric field: the options are parsed with `FromStr`.
    #[param(default = "0.5", choice = ["0.5", "0.618"])]
    ratio: f64,
    /// `int` keeps an integer parameter in a float field.
    #[param(int, default = 8, min = 2, max = 40, step = 2)]
    stop_ticks: f64,
}

#[test]
fn numeric_choices_and_int_floats() {
    let n = Numbers::build(&Params::default());
    assert_eq!((n.ratio, n.stop_ticks), (0.5, 8.0));
    let spec = Numbers::param_spec();
    assert!(matches!(&spec.entries[0].range, ParamRange::Choice(o) if o == &["0.5", "0.618"]));
    assert!(matches!(spec.entries[1].range, ParamRange::Int { min: 2, max: 40, step: 2 }));
    let n = Numbers::build(&spec.parse_assignments(&["ratio=0.618".to_string()]).unwrap());
    assert_eq!(n.ratio, 0.618);
}
