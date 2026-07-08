use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use accelerando_core::{Configurable, Footprint, Indicator, ParamSpec, Params, Plot};
use serde_json::Value;

pub const EVENT_COUNT: &str = "economic_calendar_event_count";
pub const EVENT_TITLE: &str = "economic_calendar_event";

const DEFAULT_SOURCE: &str = "http://nfs.faireconomy.media/ff_calendar_thisweek.json";
const DEFAULT_GROUP: &str = "ff_high_events";
const DEFAULT_COLOR: &str = "#a855f7";

#[derive(Clone, Debug)]
struct CalendarEvent {
    ts_ns: i64,
    impact: String,
    country: String,
    title: String,
    forecast: String,
    previous: String,
    actual: String,
}

impl CalendarEvent {
    fn line(&self) -> String {
        let mut parts = vec![self.country.clone(), self.title.clone()];
        if !self.forecast.is_empty() {
            parts.push(format!("forecast {}", self.forecast));
        }
        if !self.previous.is_empty() {
            parts.push(format!("previous {}", self.previous));
        }
        if !self.actual.is_empty() {
            parts.push(format!("actual {}", self.actual));
        }
        parts.join(" | ")
    }
}

/// ForexFactory-style economic calendar marker indicator.
///
/// By default this loads `ff_calendar_thisweek.json` from the public ForexFactory static feed,
/// filters `High` impact events, and emits one chart marker on the footprint whose time span
/// contains each event. For deterministic historical backtests, set `source` to a local JSON file
/// with the same schema instead of using the live weekly feed.
pub struct EconomicCalendar {
    events: Vec<CalendarEvent>,
    next_idx: usize,
    impact: String,
    countries: HashSet<String>,
    plot_markers: bool,
    max_events_per_bar: usize,
    group: String,
    color: String,
}

impl Configurable for EconomicCalendar {
    fn param_spec() -> ParamSpec {
        ParamSpec::new()
            .fixed_str("source", DEFAULT_SOURCE)
            .choice(
                "impact",
                "High",
                &["High", "Medium", "Low", "Holiday", "All"],
            )
            .fixed_str("countries", "")
            .int("plot_markers", 1, 0, 1, 1)
            .int("max_events_per_bar", 8, 1, 50, 1)
            .fixed_str("group", DEFAULT_GROUP)
            .fixed_str("color", DEFAULT_COLOR)
    }

    fn from_params(p: &Params) -> Self {
        let impact = p.str("impact", "High");
        let countries = p
            .str("countries", "")
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect::<HashSet<_>>();
        let mut events = load_events(&p.str("source", DEFAULT_SOURCE), &impact).unwrap_or_default();
        events.sort_by_key(|event| event.ts_ns);
        Self {
            events,
            next_idx: 0,
            impact,
            countries,
            plot_markers: p.int("plot_markers", 1) != 0,
            max_events_per_bar: p.usize("max_events_per_bar", 8).max(1),
            group: p.str("group", DEFAULT_GROUP),
            color: p.str("color", DEFAULT_COLOR),
        }
    }
}

impl Indicator for EconomicCalendar {
    fn name(&self) -> &str {
        "economic_calendar"
    }

    fn on_footprint(&mut self, fp: &mut Footprint, _history: &[Footprint]) {
        let mut hits = Vec::new();
        while self.next_idx < self.events.len() && self.events[self.next_idx].ts_ns <= fp.ts_last_ns
        {
            let event = &self.events[self.next_idx];
            if event.ts_ns >= fp.ts_first_ns && self.matches_filters(event) {
                hits.push(event.clone());
            }
            self.next_idx += 1;
        }

        fp.values.insert(EVENT_COUNT.to_string(), hits.len() as f64);
        if hits.is_empty() {
            return;
        }

        fp.tags
            .insert(EVENT_TITLE.to_string(), hits[0].title.clone());
        if self.plot_markers {
            let text = hits
                .iter()
                .take(self.max_events_per_bar)
                .map(CalendarEvent::line)
                .collect::<Vec<_>>()
                .join("\n");
            fp.plots.push(Plot::Marker {
                price: fp.low,
                shape: "event".to_string(),
                color: self.color.clone(),
                text,
                text_dx: None,
                text_dy: None,
                group: Some(self.group.clone()),
            });
        }
    }
}

impl EconomicCalendar {
    fn matches_filters(&self, event: &CalendarEvent) -> bool {
        (self.impact.eq_ignore_ascii_case("All") || event.impact.eq_ignore_ascii_case(&self.impact))
            && (self.countries.is_empty() || self.countries.contains(&event.country))
    }
}

fn load_events(source: &str, impact: &str) -> Result<Vec<CalendarEvent>, String> {
    let body = if source.starts_with("http://") {
        fetch_http(source)?
    } else if source.starts_with("https://") {
        return Err("https calendar sources are not supported without a TLS client".to_string());
    } else {
        fs::read_to_string(source).map_err(|e| format!("read calendar file {source}: {e}"))?
    };
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("parse calendar json: {e}"))?;
    parse_forex_factory_events(&value, impact)
}

fn parse_forex_factory_events(value: &Value, impact: &str) -> Result<Vec<CalendarEvent>, String> {
    let events = value
        .as_array()
        .ok_or_else(|| "calendar json is not an array".to_string())?;
    let mut out = Vec::new();
    for item in events {
        let event_impact = str_field(item, "impact");
        if !impact.eq_ignore_ascii_case("All") && !event_impact.eq_ignore_ascii_case(impact) {
            continue;
        }
        let Some(ts_ns) = parse_iso_offset_ns(&str_field(item, "date")) else {
            continue;
        };
        out.push(CalendarEvent {
            ts_ns,
            impact: event_impact,
            country: str_field(item, "country").to_ascii_uppercase(),
            title: str_field(item, "title"),
            forecast: str_field(item, "forecast"),
            previous: str_field(item, "previous"),
            actual: str_field(item, "actual"),
        });
    }
    Ok(out)
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn fetch_http(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported calendar url {url}"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = format!("/{path}");
    let mut stream =
        TcpStream::connect((host, 80)).map_err(|e| format!("connect {host}:80: {e}"))?;
    let timeout = Some(Duration::from_secs(8));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| format!("set read timeout: {e}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| format!("set write timeout: {e}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: accelerando/0.1\r\nAccept: application/json\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read response: {e}"))?;
    let split = find_bytes(&response, b"\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response: missing headers".to_string())?;
    let headers = String::from_utf8_lossy(&response[..split]).into_owned();
    let status = headers
        .lines()
        .next()
        .ok_or_else(|| "malformed HTTP response: missing status".to_string())?;
    if !status.contains(" 200 ") {
        return Err(format!("calendar server returned {status}"));
    }
    let headers_lc = headers.to_ascii_lowercase();
    if headers_lc.contains("content-encoding:")
        && !headers_lc.contains("content-encoding: identity")
    {
        return Err("calendar response is compressed".to_string());
    }
    let mut body = response[split + 4..].to_vec();
    if headers_lc.contains("transfer-encoding: chunked") {
        body = decode_chunked(&body)?;
    }
    String::from_utf8(body).map_err(|e| format!("calendar body is not UTF-8: {e}"))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find_bytes(body, b"\r\n")
            .ok_or_else(|| "malformed chunked response: missing chunk size".to_string())?;
        let size_line = String::from_utf8_lossy(&body[..line_end]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|e| format!("malformed chunk size '{size_hex}': {e}"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            break;
        }
        if body.len() < size + 2 {
            return Err("malformed chunked response: truncated chunk".to_string());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size..];
        if !body.starts_with(b"\r\n") {
            return Err("malformed chunked response: missing chunk terminator".to_string());
        }
        body = &body[2..];
    }
    Ok(out)
}

fn parse_iso_offset_ns(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let year = parse_i32(&value[0..4])?;
    let month = parse_i32(&value[5..7])?;
    let day = parse_i32(&value[8..10])?;
    let hour = parse_i64(&value[11..13])?;
    let minute = parse_i64(&value[14..16])?;
    let second = parse_i64(&value[17..19])?;
    let (offset_sign, off_start) = match bytes.get(19).copied()? {
        b'Z' => (1, 0),
        b'+' if bytes.len() >= 25 => (1, 20),
        b'-' if bytes.len() >= 25 => (-1, 20),
        _ => return None,
    };
    let offset_seconds = if off_start == 0 {
        0
    } else {
        let off_hour = parse_i64(&value[off_start..off_start + 2])?;
        let off_minute = parse_i64(&value[off_start + 3..off_start + 5])?;
        offset_sign * (off_hour * 3600 + off_minute * 60)
    };
    let days = days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    local_seconds
        .checked_sub(offset_seconds)?
        .checked_mul(1_000_000_000)
}

fn parse_i32(s: &str) -> Option<i32> {
    s.parse().ok()
}

fn parse_i64(s: &str) -> Option<i64> {
    s.parse().ok()
}

fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146097 + doe - 719468)
}
