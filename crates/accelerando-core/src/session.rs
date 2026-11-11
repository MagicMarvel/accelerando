//! Trading-day bookkeeping shared by most intraday strategies.
//!
//! * [`SessionClock`] — turns a footprint timestamp into a trading day + minute and flags the
//!   first bar of a new day, so strategies stop hand-rolling `if day != self.cur_day { ... }`.
//!   US-Eastern midnight by default; any [`TimeZone`] and any day-start time (e.g. Beijing 06:00,
//!   or CME's 18:00 ET Globex open) via [`SessionClock::in_zone`] / [`SessionClock::day_starts_at`].
//! * [`SessionFilter`] — allowed ET windows plus open/close exclusion bands (parameter group).
//! * [`TradeDiscipline`] — per-day trade cap, cooldown, single-position rule (parameter group).
//!
//! Both groups derive `Configurable`, so embed them with `#[param(flatten)]`.

use crate::market_time::{in_session, parse_minute, parse_sessions, TimeZone};
use crate::params::{ParamGroup, ParamSpec, Params};
use crate::Configurable;

/// One bar's position on a [`SessionClock`]'s calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTick {
    /// Trading day: days since the Unix epoch of the local date on which the trading day
    /// *started* (with an 18:00 start, Monday 20:00 and Tuesday 10:00 share Monday's number).
    pub day: i64,
    /// Local minute-of-day in the clock's time zone, `0..1440`.
    pub minute: u32,
    /// Minutes since the trading day started, `0..1440`.
    pub elapsed: u32,
    /// `true` on the first bar seen for `day`.
    pub new_day: bool,
}

/// Tracks the trading day across bars: US-Eastern midnight-to-midnight by default.
#[derive(Clone, Debug)]
pub struct SessionClock {
    zone: TimeZone,
    day_start: u32,
    day: i64,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self {
            zone: TimeZone::Eastern,
            day_start: 0,
            day: i64::MIN,
        }
    }
}

impl SessionClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep the calendar in `zone` (`TimeZone::BEIJING`, `"utc+8".parse()?`, ...).
    pub fn in_zone(mut self, zone: TimeZone) -> Self {
        self.zone = zone;
        self
    }

    /// Start each trading day at this local minute instead of midnight (`18 * 60` for the CME
    /// Globex day in Eastern time; `6 * 60` for a Beijing 06:00 day). Taken modulo 1440.
    pub fn day_starts_at(mut self, minute: u32) -> Self {
        self.day_start = minute % 1440;
        self
    }

    pub fn zone(&self) -> TimeZone {
        self.zone
    }

    /// `(trading day, local minute, minutes since day start)` of `ts_ns`, without advancing.
    pub fn locate(&self, ts_ns: i64) -> (i64, u32, u32) {
        let (date, minute) = self.zone.day_minute(ts_ns);
        let since = date * 1440 + minute as i64 - self.day_start as i64;
        (since.div_euclid(1440), minute, since.rem_euclid(1440) as u32)
    }

    /// Advance to the bar ending at `ts_ns` (use `fp.ts_last_ns`).
    pub fn update(&mut self, ts_ns: i64) -> SessionTick {
        let (day, minute, elapsed) = self.locate(ts_ns);
        let new_day = day != self.day;
        self.day = day;
        SessionTick {
            day,
            minute,
            elapsed,
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
    fn clock_supports_zones_and_day_starts() {
        // 2026-01-05 is EST (UTC-5); Beijing = UTC+8.
        let base = days_from_civil(2026, 1, 5) * 86_400;
        let utc = |h: i64, m: i64| (base + h * 3600 + m * 60) * NS_PER_SEC;
        // Beijing day starting 06:00: 21:59 UTC Jan 5 = 05:59 BJ Jan 6 -> still Jan 5's day.
        let mut bj = SessionClock::new().in_zone(TimeZone::BEIJING).day_starts_at(6 * 60);
        let a = bj.update(utc(21, 59));
        assert_eq!((a.day, a.minute, a.elapsed), (days_from_civil(2026, 1, 5), 5 * 60 + 59, 1439));
        let b = bj.update(utc(22, 0));
        assert!(b.new_day);
        assert_eq!((b.day, b.elapsed), (days_from_civil(2026, 1, 6), 0));
        // CME Globex day in ET starting 18:00: Mon 18:00 ET (23:00 UTC) and Tue 10:00 ET share it.
        let mut cme = SessionClock::new().day_starts_at(18 * 60);
        let mon = cme.update(utc(23, 0));
        let tue = cme.update(utc(24 + 15, 0));
        assert_eq!(mon.day, days_from_civil(2026, 1, 5));
        assert_eq!((tue.day, tue.minute, tue.elapsed, tue.new_day), (mon.day, 600, 960, false));
        assert_eq!("utc-5:30".parse::<TimeZone>().unwrap(), TimeZone::Fixed(-330));
        assert_eq!("Beijing".parse::<TimeZone>().unwrap(), TimeZone::BEIJING);
        assert!("mars".parse::<TimeZone>().is_err());
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
