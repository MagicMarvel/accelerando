//! Trading-day bookkeeping shared by most intraday strategies.
//!
//! * [`SessionClock`] — turns a footprint timestamp into the US-Eastern day/minute and flags the
//!   first bar of a new day, so strategies stop hand-rolling `if day != self.cur_day { ... }`.
//! * [`SessionFilter`] — allowed ET windows plus open/close exclusion bands (parameter group).
//! * [`TradeDiscipline`] — per-day trade cap, cooldown, single-position rule (parameter group).
//!
//! Both groups derive `Configurable`, so embed them with `#[param(flatten)]`.

use crate::market_time::{eastern_day_minute, in_session, parse_minute, parse_sessions};
use crate::params::{ParamGroup, ParamSpec, Params};
use crate::Configurable;

/// One bar's position on the Eastern-time calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTick {
    /// Days since the Unix epoch, in US-Eastern civil time (DST aware).
    pub day: i64,
    /// Minute of the Eastern day, `0..1440`.
    pub minute: u32,
    /// `true` on the first bar seen for `day`.
    pub new_day: bool,
}

/// Tracks the Eastern trading day across bars.
#[derive(Clone, Debug)]
pub struct SessionClock {
    day: i64,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self { day: i64::MIN }
    }
}

impl SessionClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance to the bar ending at `ts_ns` (use `fp.ts_last_ns`).
    pub fn update(&mut self, ts_ns: i64) -> SessionTick {
        let (day, minute) = eastern_day_minute(ts_ns);
        let new_day = day != self.day;
        self.day = day;
        SessionTick {
            day,
            minute,
            new_day,
        }
    }

    pub fn day(&self) -> Option<i64> {
        (self.day != i64::MIN).then_some(self.day)
    }
}

/// Allowed Eastern-time windows, plus exclusion bands right after the RTH open and right before
/// the RTH close. All string times are `HH:MM` ET.
#[derive(Clone, Debug, Configurable)]
#[configurable(init = Self::init)]
pub struct SessionFilter {
    /// Comma-separated `HH:MM-HH:MM` windows in which entries are allowed.
    #[param(fixed, default = "09:35-11:30,13:30-15:45")]
    pub sessions: String,
    #[param(fixed, default = "09:30")]
    pub rth_open: String,
    #[param(fixed, default = "16:00")]
    pub rth_close: String,
    /// Minutes after the RTH open in which entries are blocked.
    #[param(default = 5, min = 0, max = 120)]
    pub exclude_first_minutes: u32,
    /// Minutes before the RTH close in which entries are blocked.
    #[param(default = 15, min = 0, max = 120)]
    pub exclude_last_minutes: u32,
    windows: Vec<(u32, u32)>,
    open_minute: u32,
    close_minute: u32,
}

impl SessionFilter {
    fn init(&mut self) {
        self.windows = parse_sessions(&self.sessions);
        self.open_minute = parse_minute(&self.rth_open).unwrap_or(9 * 60 + 30);
        self.close_minute = parse_minute(&self.rth_close).unwrap_or(16 * 60);
    }

    /// Append this group's parameters to a spec (same as `ParamGroup::declare_params`).
    pub fn params(spec: ParamSpec) -> ParamSpec {
        <Self as ParamGroup>::declare_params(spec)
    }

    /// Is an entry allowed at this Eastern minute? The open/close bands win over `sessions`.
    pub fn allows(&self, minute: u32) -> bool {
        if minute >= self.open_minute
            && minute < self.open_minute.saturating_add(self.exclude_first_minutes)
        {
            return false;
        }
        if minute >= self.close_minute.saturating_sub(self.exclude_last_minutes)
            && minute < self.close_minute
        {
            return false;
        }
        in_session(&self.windows, minute)
    }
}

impl Default for SessionFilter {
    fn default() -> Self {
        <Self as ParamGroup>::read_params(&Self::param_spec().defaults())
    }
}

/// Per-day trade cap + cooldown between entries + single-position rule.
///
/// Call [`TradeDiscipline::on_bar`] once per footprint before deciding, check
/// [`TradeDiscipline::can_enter`], and call [`TradeDiscipline::record_entry`] when you submit an
/// entry.
#[derive(Clone, Debug, Configurable)]
#[configurable(init = Self::init)]
pub struct TradeDiscipline {
    #[param(default = 3, min = 1, max = 100)]
    pub max_trades_per_day: usize,
    /// Completed bars required between two entries.
    #[param(default = 3, min = 0, max = 100)]
    pub cooldown_bars: usize,
    /// Block new entries while a position or a pending entry exists.
    #[param(default = true)]
    pub one_position_at_a_time: bool,
    cur_day: i64,
    trades_today: usize,
    bars_since_entry: usize,
}

impl TradeDiscipline {
    fn init(&mut self) {
        self.max_trades_per_day = self.max_trades_per_day.max(1);
        self.cur_day = i64::MIN;
        self.trades_today = 0;
        self.bars_since_entry = usize::MAX / 2;
    }

    /// Append this group's parameters to a spec (same as `ParamGroup::declare_params`).
    pub fn params(spec: ParamSpec) -> ParamSpec {
        <Self as ParamGroup>::declare_params(spec)
    }

    /// Call once per completed footprint, before any entry decision.
    pub fn on_bar(&mut self, day: i64) {
        if day != self.cur_day {
            self.cur_day = day;
            self.trades_today = 0;
        }
        self.bars_since_entry = self.bars_since_entry.saturating_add(1);
    }

    /// `position_dir` is the signed net position (0 = flat).
    pub fn can_enter(&self, position_dir: i32, pending_count: usize) -> bool {
        self.trades_today < self.max_trades_per_day
            && self.bars_since_entry >= self.cooldown_bars
            && !(self.one_position_at_a_time && (position_dir != 0 || pending_count > 0))
    }

    pub fn record_entry(&mut self) {
        self.trades_today += 1;
        self.bars_since_entry = 0;
    }

    pub fn trades_today(&self) -> usize {
        self.trades_today
    }
}

impl Default for TradeDiscipline {
    fn default() -> Self {
        <Self as ParamGroup>::read_params(&Self::param_spec().defaults())
    }
}

/// Convenience used by tests and docs: build a group from `name=value` pairs.
pub fn group_from_pairs<T: ParamGroup + Configurable>(pairs: &[(&str, crate::ParamValue)]) -> T {
    let mut p = Params::default();
    for (k, v) in pairs {
        p.set(k, v.clone());
    }
    T::build(&p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_time::{days_from_civil, NS_PER_SEC};
    use crate::ParamValue;

    #[test]
    fn session_filter_applies_exclusions_and_windows() {
        let f: SessionFilter = group_from_pairs(&[(
            "sessions",
            ParamValue::Str("09:30-11:30,13:30-16:00".into()),
        )]);
        assert!(!f.allows(9 * 60 + 32));
        assert!(f.allows(9 * 60 + 40));
        assert!(!f.allows(12 * 60));
        assert!(f.allows(13 * 60 + 40));
        assert!(!f.allows(15 * 60 + 50));
    }

    #[test]
    fn discipline_caps_cooldown_and_single_position() {
        let mut d: TradeDiscipline = group_from_pairs(&[
            ("max_trades_per_day", ParamValue::Int(2)),
            ("cooldown_bars", ParamValue::Int(3)),
        ]);
        d.on_bar(1);
        assert!(d.can_enter(0, 0));
        assert!(!d.can_enter(1, 0));
        assert!(!d.can_enter(0, 1));
        d.record_entry();
        d.on_bar(1);
        assert!(!d.can_enter(0, 0));
        d.on_bar(1);
        d.on_bar(1);
        assert!(d.can_enter(0, 0));
        d.record_entry();
        for _ in 0..3 {
            d.on_bar(1);
        }
        assert!(!d.can_enter(0, 0));
        d.on_bar(2);
        assert!(d.can_enter(0, 0));
    }

    #[test]
    fn clock_flags_first_bar_of_each_eastern_day() {
        // 2026-01-05 is EST (UTC-5); 20:00 ET Monday is still Monday, 00:30 ET is Tuesday.
        let base = days_from_civil(2026, 1, 5) * 86_400;
        let ts = |h: i64, m: i64| (base + (h + 5) * 3600 + m * 60) * NS_PER_SEC;
        let mut c = SessionClock::new();
        let a = c.update(ts(9, 31));
        assert!(a.new_day);
        assert_eq!(a.minute, 9 * 60 + 31);
        assert!(!c.update(ts(20, 0)).new_day);
        let b = c.update(ts(24, 30));
        assert!(b.new_day);
        assert_eq!(b.minute, 30);
    }

    #[test]
    fn bool_params_accept_words_on_the_cli() {
        let spec = TradeDiscipline::param_spec();
        let p = spec
            .parse_assignments(&["one_position_at_a_time=false".to_string()])
            .unwrap();
        assert!(!TradeDiscipline::build(&p).one_position_at_a_time);
    }
}
