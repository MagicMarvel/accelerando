//! US-Eastern market-time helpers shared by intraday strategies and indicators:
//! ns-timestamp to ET day/minute conversion (post-2007 DST rule), session-window
//! parsing, and Howard Hinnant civil-date arithmetic.

pub const NS_PER_SEC: i64 = 1_000_000_000;
/// Parse "09:35-11:30,13:30-15:45" into minute-of-day (ET) windows.
pub fn parse_sessions(spec: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let Some((a, b)) = part.split_once('-') else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_minute(a), parse_minute(b)) else {
            continue;
        };
        if start < end {
            out.push((start, end));
        }
    }
    out
}

pub fn parse_minute(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn in_session(sessions: &[(u32, u32)], minute: u32) -> bool {
    if sessions.is_empty() {
        return true;
    }
    sessions
        .iter()
        .any(|&(start, end)| minute >= start && minute < end)
}

/// US-Eastern civil day (days since epoch) and minute-of-day for a UTC timestamp, using the
/// post-2007 DST rule (second Sunday of March to first Sunday of November). Transition-day
/// precision is date-level, which is fine for session filtering.
pub fn eastern_day_minute(ts_ns: i64) -> (i64, u32) {
    let utc_s = ts_ns.div_euclid(NS_PER_SEC);
    let est_days = (utc_s - 5 * 3600).div_euclid(86_400);
    let (year, month, day) = civil_from_days(est_days);
    let offset_s = if in_us_dst(year, month, day) {
        4 * 3600
    } else {
        5 * 3600
    };
    let local_s = utc_s - offset_s;
    let days = local_s.div_euclid(86_400);
    let minute = (local_s.rem_euclid(86_400) / 60) as u32;
    (days, minute)
}

pub fn in_us_dst(year: i32, month: i32, day: i32) -> bool {
    if !(3..=11).contains(&month) {
        return false;
    }
    if month > 3 && month < 11 {
        return true;
    }
    let dow_first = |m: i32| ((days_from_civil(year, m, 1) + 4).rem_euclid(7)) as i32; // 0 = Sunday
    if month == 3 {
        let second_sunday = 1 + (7 - dow_first(3)) % 7 + 7;
        day >= second_sunday
    } else {
        let first_sunday = 1 + (7 - dow_first(11)) % 7;
        day < first_sunday
    }
}

pub fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146_097 + doe - 719_468)
}

pub fn civil_from_days(z: i64) -> (i32, i32, i32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + i64::from(m <= 2)) as i32, m as i32, d as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_parsing() {
        let s = parse_sessions("09:35-11:30,13:30-15:45");
        assert_eq!(s, vec![(575, 690), (810, 945)]);
        assert!(parse_sessions("junk").is_empty());
        assert!(in_session(&s, 600));
        assert!(!in_session(&s, 700));
        assert!(in_session(&[], 0));
    }

    #[test]
    fn us_dst_rule() {
        // 2026: DST starts Sunday 2026-03-08, ends Sunday 2026-11-01.
        assert!(!in_us_dst(2026, 3, 7));
        assert!(in_us_dst(2026, 3, 8));
        assert!(in_us_dst(2026, 7, 9));
        assert!(in_us_dst(2026, 10, 31));
        assert!(!in_us_dst(2026, 11, 1));
        assert!(!in_us_dst(2026, 1, 15));
    }

    #[test]
    fn eastern_minute_matches_known_timestamps() {
        // 2026-02-04 15:00:00 UTC = 10:00 EST (winter).
        let ts = (days_from_civil(2026, 2, 4) * 86_400 + 15 * 3600) * NS_PER_SEC;
        let (_, minute) = eastern_day_minute(ts);
        assert_eq!(minute, 10 * 60);
        // 2026-06-04 14:00:00 UTC = 10:00 EDT (summer).
        let ts = (days_from_civil(2026, 6, 4) * 86_400 + 14 * 3600) * NS_PER_SEC;
        let (_, minute) = eastern_day_minute(ts);
        assert_eq!(minute, 10 * 60);
    }
}
