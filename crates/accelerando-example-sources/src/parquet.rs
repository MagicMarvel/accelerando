//! ATAS Replay Parquet v1, including files with ZSTD-compressed column pages.
//!
//! Large depth files are read selectively:
//!
//! * `start` / `end` keep only `[start, end)`. Row groups whose footer timestamp statistics fall
//!   outside the range are never decompressed. When depth is wanted, the rows before `start` are
//!   still read to rebuild the book (the file has one snapshot, at its beginning); the book as of
//!   `start` is then emitted as `SetLevel` events stamped `start`.
//!
//! A parquet row filter that decoded only trade rows was measured slower than decoding every
//! row (it decompresses the filter columns twice and skips in tiny runs), so rows outside the
//! interest are dropped while decoding instead.

use std::collections::BTreeMap;
use std::fs::File;

use accelerando_core::{
    Configurable, DataSource, EventInterest, OrderFlowEvent, ProgressHandle, Side,
};
use arrow_array::{Array, Decimal128Array, Int32Array, Int64Array, RecordBatch};
use arrow_schema::DataType;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::file::statistics::Statistics;

const TRADE: i32 = 2;
const SCALE: f64 = 1e18;

/// Streams the trades and absolute depth levels from an ATAS Replay Parquet export.
#[derive(Configurable)]
#[configurable(init = Self::init)]
pub struct ParquetSource {
    #[param(fixed, default = "")]
    path: String,
    /// First timestamp to emit (inclusive), UTC: `2026-08-20`, `2026-08-20 13:30[:05]` or
    /// nanoseconds since the epoch. Empty = from the beginning.
    #[param(fixed, default = "")]
    start: String,
    /// End of the range (exclusive), same formats. Empty = to the end of the file.
    #[param(fixed, default = "")]
    end: String,
    progress: Option<ProgressHandle>,
    event_interest: EventInterest,
}

impl ParquetSource {
    fn init(&mut self) {
        self.event_interest = EventInterest::ALL;
    }
}

impl DataSource for ParquetSource {
    fn events(self: Box<Self>) -> Box<dyn Iterator<Item = OrderFlowEvent>> {
        Box::new(
            self.try_events()
                .map(|event| event.unwrap_or_else(|e| panic!("{e}"))),
        )
    }

    fn try_events(self: Box<Self>) -> Box<dyn Iterator<Item = Result<OrderFlowEvent, String>>> {
        let path = &self.path;
        let range = parse_time(&self.start)
            .and_then(|start| Ok((start, parse_time(&self.end)?)))
            .and_then(|(start, end)| match (start, end) {
                (Some(s), Some(e)) if s >= e => Err(format!(
                    "start '{}' is not before end '{}'",
                    self.start, self.end
                )),
                range => Ok(range),
            });
        let (interest, progress) = (self.event_interest, self.progress.clone());
        let reader = range.and_then(|range| ReplayReader::open(path, interest, range, progress));
        match reader {
            Ok(reader) => Box::new(reader),
            Err(error) => Box::new(std::iter::once(Err(format!(
                "read Parquet '{path}': {error}"
            )))),
        }
    }

    fn set_event_interest(&mut self, interest: EventInterest) {
        self.event_interest = interest;
    }

    fn set_progress(&mut self, progress: ProgressHandle) {
        self.progress = Some(progress);
    }
}

/// `""` = unbounded; an integer = nanoseconds; else `YYYY-MM-DD[( |T)HH:MM[:SS]][Z]` in UTC.
fn parse_time(text: &str) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Ok(ns) = text.parse::<i64>() {
        return Ok(Some(ns));
    }
    let bad = || {
        format!("invalid time '{text}': use 2026-08-20, \"2026-08-20 13:30[:05]\" (UTC) or ns")
    };
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let number = |s: &str, digits: usize| {
        (s.len() == digits && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<i64>().unwrap())
            .ok_or_else(bad)
    };
    let mut ymd = date.split('-');
    let (Some(y), Some(m), Some(d), None) = (ymd.next(), ymd.next(), ymd.next(), ymd.next()) else {
        return Err(bad());
    };
    let (y, m, d) = (number(y, 4)?, number(m, 2)?, number(d, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    let mut seconds = 0;
    if let Some(time) = time {
        let parts: Vec<&str> = time.split(':').collect();
        if !(2..=3).contains(&parts.len()) {
            return Err(bad());
        }
        let (h, min) = (number(parts[0], 2)?, number(parts[1], 2)?);
        let s = parts.get(2).map_or(Ok(0), |s| number(s, 2))?;
        if h > 23 || min > 59 || s > 59 {
            return Err(bad());
        }
        seconds = h * 3600 + min * 60 + s;
    }
    // Days from civil (Howard Hinnant), proleptic Gregorian.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(Some((days * 86_400 + seconds) * 1_000_000_000))
}

/// Which rows a read needs.
#[derive(Clone, Copy)]
struct Want {
    start: Option<i64>,
    end: Option<i64>,
    trades: bool,
    depth: bool,
    /// Depth is wanted and `start` is set: depth rows before `start` rebuild the book.
    warm_up: bool,
}

impl Want {
    fn row(self, ts: i64, kind: i32) -> bool {
        if self.end.is_some_and(|end| ts >= end) {
            return false;
        }
        let trade = kind == TRADE;
        if self.start.is_some_and(|start| ts < start) {
            return self.warm_up && !trade;
        }
        if trade {
            self.trades
        } else {
            self.depth
        }
    }
}

struct ReplayReader {
    path: String,
    reader: ParquetRecordBatchReader,
    columns: [usize; 6],
    contract: Option<OrderFlowEvent>,
    want: Want,
    /// Resting size by (side, raw price) before `start`; emitted once `start` is reached.
    book: Option<BTreeMap<(i32, i128), i128>>,
    progress: Option<ProgressHandle>,
    /// Compressed bytes of the selected row groups and their timestamp span, for progress.
    total_bytes: u64,
    span: Option<(i64, i64)>,
    /// Rows in the selected row groups (footer consistency check).
    expected_rows: u64,
    rows_read: u64,
    reported_bytes: u64,
    last_key: Option<(i64, i64)>,
    pending: std::vec::IntoIter<OrderFlowEvent>,
    done: bool,
}

impl ReplayReader {
    fn open(
        path: &str,
        interest: EventInterest,
        (start, end): (Option<i64>, Option<i64>),
        progress: Option<ProgressHandle>,
    ) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| e.to_string())?;
        let metadata = builder.metadata().file_metadata();
        let value = |key: &str| {
            metadata
                .key_value_metadata()?
                .iter()
                .find(|entry| entry.key == key)?
                .value
                .as_deref()
        };
        if value("schema_version") != Some("1") {
            return Err("expected ATAS Replay schema_version=1".into());
        }
        let positive = |key: &str| {
            value(key)
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| format!("missing or invalid '{key}' metadata"))
        };
        let contract = OrderFlowEvent::Contract {
            tick_size: positive("tick_size")?,
            multiplier: positive("multiplier")?,
        };
        let schema = builder.schema();
        let column = |name: &str, expected: DataType| {
            let index = schema.index_of(name).map_err(|e| e.to_string())?;
            let actual = schema.field(index).data_type();
            if *actual != expected {
                return Err(format!(
                    "column '{name}' has type {actual}, expected {expected}"
                ));
            }
            Ok(index)
        };
        let columns = [
            column("timestamp_ns", DataType::Int64)?,
            column("sequence", DataType::Int64)?,
            column("event_type", DataType::Int32)?,
            column("side", DataType::Int32)?,
            column("price", DataType::Decimal128(38, 18))?,
            column("size", DataType::Decimal128(38, 18))?,
        ];
        let ts_col = columns[0];

        let depth = interest.contains(EventInterest::L2);
        let want = Want {
            start,
            end,
            trades: interest.contains(EventInterest::TRADE),
            depth,
            warm_up: depth && start.is_some(),
        };

        // Row groups are in time order; skip those entirely outside the range. Groups without
        // statistics are always read.
        let mut row_groups = Vec::new();
        let (mut total_bytes, mut rows) = (0u64, 0u64);
        let mut span: Option<(i64, i64)> = None;
        let mut span_known = true;
        for (index, group) in builder.metadata().row_groups().iter().enumerate() {
            let (lo, hi) = match group.column(ts_col).statistics() {
                Some(Statistics::Int64(s)) => (s.min_opt().copied(), s.max_opt().copied()),
                _ => (None, None),
            };
            let after_end = matches!((lo, end), (Some(lo), Some(end)) if lo >= end);
            let before_start =
                !want.warm_up && matches!((hi, start), (Some(hi), Some(start)) if hi < start);
            if after_end || before_start {
                continue;
            }
            row_groups.push(index);
            total_bytes += group.compressed_size().max(0) as u64;
            rows += group.num_rows().max(0) as u64;
            match (lo, hi) {
                (Some(lo), Some(hi)) => {
                    span = Some(span.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))))
                }
                _ => span_known = false,
            }
        }
        let span = span.filter(|_| span_known).map(|(lo, hi)| {
            let lo = if want.warm_up { lo } else { start.map_or(lo, |s| lo.max(s)) };
            (lo, end.map_or(hi, |e| hi.min(e)))
        });

        let builder = builder.with_row_groups(row_groups).with_batch_size(8192);
        let reader = builder.build().map_err(|e| e.to_string())?;
        if let Some(progress) = &progress {
            progress.set_total_bytes(total_bytes);
        }
        Ok(Self {
            path: path.into(),
            reader,
            columns,
            contract: interest
                .contains(EventInterest::CONTRACT)
                .then_some(contract),
            want,
            book: want.warm_up.then(BTreeMap::new),
            progress,
            total_bytes,
            span,
            expected_rows: rows,
            rows_read: 0,
            reported_bytes: 0,
            last_key: None,
            pending: Vec::new().into_iter(),
            done: false,
        })
    }

    fn decode_batch(&mut self, batch: RecordBatch) -> Result<(), String> {
        let [ts, seq, kind, side, price, size] = self.columns.map(|i| batch.column(i));
        for column in [ts, seq, kind, side, price, size] {
            if column.null_count() != 0 {
                return Err("null value in a required replay column".into());
            }
        }
        // Column types were checked when opening the file.
        let ts = ts.as_any().downcast_ref::<Int64Array>().unwrap().values();
        let seq = seq.as_any().downcast_ref::<Int64Array>().unwrap().values();
        let kind = kind.as_any().downcast_ref::<Int32Array>().unwrap().values();
        let side = side.as_any().downcast_ref::<Int32Array>().unwrap().values();
        let price = price
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap()
            .values();
        let size = size
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap()
            .values();
        let mut events = Vec::with_capacity(batch.num_rows());
        for i in 0..batch.num_rows() {
            let ts_ns = ts[i];
            if self
                .last_key
                .is_some_and(|(prev_ts, prev_seq)| ts_ns < prev_ts || seq[i] <= prev_seq)
            {
                return Err(format!("out-of-order replay row at sequence {}", seq[i]));
            }
            self.last_key = Some((ts_ns, seq[i]));
            let side = match side[i] {
                1 => Side::Buy,
                -1 => Side::Sell,
                0 if kind[i] == TRADE => Side::Unknown,
                code => return Err(format!("invalid side {code} at sequence {}", seq[i])),
            };
            let interest = match kind[i] {
                TRADE => EventInterest::TRADE,
                0 | 1 | 3 => EventInterest::L2,
                code => return Err(format!("unknown event type {code} at sequence {}", seq[i])),
            };
            if size[i] < 0 || (kind[i] == 3 && size[i] != 0) {
                return Err(format!("invalid size at sequence {}", seq[i]));
            }
            if !self.want.row(ts_ns, kind[i]) {
                continue;
            }
            if let (Some(book), Some(start)) = (self.book.as_mut(), self.want.start) {
                if ts_ns < start {
                    // Warm-up: `want.row` only lets depth rows through here.
                    let key = (side_code(side), price[i]);
                    if size[i] == 0 {
                        book.remove(&key);
                    } else {
                        book.insert(key, size[i]);
                    }
                    continue;
                }
                for ((code, price), size) in self.book.take().unwrap() {
                    events.push(OrderFlowEvent::SetLevel {
                        ts_ns: start,
                        price: price as f64 / SCALE,
                        size: size as f64 / SCALE,
                        side: if code > 0 { Side::Buy } else { Side::Sell },
                    });
                }
            }
            if !self.want_interest(interest) {
                continue;
            }
            let price = price[i] as f64 / SCALE;
            let size = size[i] as f64 / SCALE;
            events.push(if kind[i] == TRADE {
                OrderFlowEvent::Trade {
                    ts_ns,
                    price,
                    size,
                    aggressor: side,
                }
            } else {
                OrderFlowEvent::SetLevel {
                    ts_ns,
                    price,
                    size,
                    side,
                }
            });
        }
        self.pending = events.into_iter();
        self.rows_read += batch.num_rows() as u64;
        if let (Some((lo, hi)), Some((ts, _))) = (self.span, self.last_key) {
            // Estimate input consumed from the timestamp reached within the selected groups.
            let done = if hi > lo {
                (ts.clamp(lo, hi) - lo) as f64 / (hi - lo) as f64
            } else {
                0.0
            };
            self.report_progress((self.total_bytes as f64 * done) as u64);
        }
        Ok(())
    }

    fn want_interest(&self, interest: EventInterest) -> bool {
        (interest == EventInterest::TRADE && self.want.trades)
            || (interest == EventInterest::L2 && self.want.depth)
    }

    fn report_progress(&mut self, bytes: u64) {
        if let Some(progress) = &self.progress {
            let bytes = bytes.max(self.reported_bytes);
            progress.add_bytes(bytes - self.reported_bytes);
            self.reported_bytes = bytes;
        }
    }
}

fn side_code(side: Side) -> i32 {
    match side {
        Side::Buy => 1,
        _ => -1,
    }
}

impl Iterator for ReplayReader {
    type Item = Result<OrderFlowEvent, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(contract) = self.contract.take() {
            return Some(Ok(contract));
        }
        while !self.done {
            if let Some(event) = self.pending.next() {
                return Some(Ok(event));
            }
            let result = match self.reader.next() {
                Some(Ok(batch)) => self.decode_batch(batch),
                Some(Err(error)) => Err(error.to_string()),
                None if self.expected_rows != self.rows_read => {
                    Err("row count differs from footer".into())
                }
                None => {
                    self.done = true;
                    self.report_progress(self.total_bytes);
                    return None;
                }
            };
            if let Err(error) = result {
                self.done = true;
                return Some(Err(format!("read Parquet '{}': {error}", self.path)));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::ArrayRef;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::{metadata::KeyValue, properties::WriterProperties};
    use std::path::PathBuf;
    use std::sync::Arc;

    const TS: i64 = 1_793_888_385_000_000_101;
    const RAW: i128 = 10i128.pow(18);

    /// Rows: (ts offset, event_type, side, price, size). Written with ZSTD, `group` rows per group.
    fn write_replay(name: &str, rows: &[(i64, i32, i32, f64, f64)], group: usize) -> PathBuf {
        let decimal = |values: Vec<i128>| -> ArrayRef {
            Arc::new(
                Decimal128Array::from(values)
                    .with_precision_and_scale(38, 18)
                    .unwrap(),
            )
        };
        let raw = |v: f64| (v * 4.0) as i128 * RAW / 4;
        let batch = RecordBatch::try_from_iter(vec![
            (
                "timestamp_ns",
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| TS + r.0))) as ArrayRef,
            ),
            (
                "sequence",
                Arc::new(Int64Array::from_iter_values(0..rows.len() as i64)),
            ),
            (
                "event_type",
                Arc::new(Int32Array::from_iter_values(rows.iter().map(|r| r.1))),
            ),
            (
                "side",
                Arc::new(Int32Array::from_iter_values(rows.iter().map(|r| r.2))),
            ),
            ("price", decimal(rows.iter().map(|r| raw(r.3)).collect())),
            ("size", decimal(rows.iter().map(|r| raw(r.4)).collect())),
        ])
        .unwrap();
        let metadata = [
            ("schema_version", "1"),
            ("tick_size", "0.25"),
            ("multiplier", "50"),
        ]
        .map(|(key, value)| KeyValue::new(key.into(), Some(value.into())));
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(Default::default()))
            .set_max_row_group_size(group)
            .set_key_value_metadata(Some(metadata.into()))
            .build();
        let path = std::env::temp_dir().join(format!(
            "accelerando-replay-{name}-{}.parquet",
            std::process::id()
        ));
        let mut writer = ArrowWriter::try_new(
            File::create(&path).unwrap(),
            batch.schema(),
            Some(properties),
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    fn read(
        path: &std::path::Path,
        interest: EventInterest,
        range: (&str, &str),
        progress: Option<ProgressHandle>,
    ) -> Result<Vec<OrderFlowEvent>, String> {
        let mut registry = accelerando_core::Registry::default();
        crate::register_all(&mut registry);
        let params = accelerando_core::Params::default()
            .with("path", path.to_str().unwrap())
            .with("start", range.0)
            .with("end", range.1);
        let mut source = registry.build_source("parquet", &params).unwrap();
        source.set_event_interest(interest);
        if let Some(progress) = progress {
            source.set_progress(progress);
        }
        source.try_events().collect()
    }

    const CONTRACT: OrderFlowEvent = OrderFlowEvent::Contract {
        tick_size: 0.25,
        multiplier: 50.0,
    };
    const TRADES: fn() -> EventInterest = || EventInterest::CONTRACT.union(EventInterest::TRADE);

    fn level(ts: i64, price: f64, size: f64, side: Side) -> OrderFlowEvent {
        OrderFlowEvent::SetLevel {
            ts_ns: TS + ts,
            price,
            size,
            side,
        }
    }

    fn trade(ts: i64, price: f64, size: f64, aggressor: Side) -> OrderFlowEvent {
        OrderFlowEvent::Trade {
            ts_ns: TS + ts,
            price,
            size,
            aggressor,
        }
    }

    #[test]
    fn reads_zstd_replay_across_row_groups_in_export_order() {
        let path = write_replay(
            "all",
            &[
                (0, 0, 1, 400.25, 3.0),
                (0, 1, -1, 400.25, 1.0),
                (0, 2, -1, 400.25, 2.0),
                (1, 3, 1, 400.25, 0.0),
                (1, 2, 0, 400.25, 1.0),
            ],
            2,
        );
        let progress = ProgressHandle::new();
        let events = read(&path, EventInterest::ALL, ("", ""), Some(progress.clone()));
        let trades = read(&path, TRADES(), ("", ""), None);
        std::fs::remove_file(&path).unwrap();

        let events = events.unwrap();
        assert_eq!(
            events,
            [
                CONTRACT,
                level(0, 400.25, 3.0, Side::Buy),
                level(0, 400.25, 1.0, Side::Sell),
                trade(0, 400.25, 2.0, Side::Sell),
                level(1, 400.25, 0.0, Side::Buy),
                trade(1, 400.25, 1.0, Side::Unknown),
            ]
        );
        assert_eq!(trades.unwrap(), [events[0], events[3], events[5]]);
        let snapshot = progress.snapshot();
        assert!(snapshot.total_bytes > 0);
        assert_eq!(snapshot.bytes, snapshot.total_bytes);
    }

    #[test]
    fn time_range_skips_row_groups_and_seeds_the_book_at_start() {
        let path = write_replay(
            "range",
            &[
                (0, 0, 1, 400.25, 3.0),   // bid snapshot
                (0, 0, -1, 400.5, 1.0),   // ask snapshot
                (0, 2, -1, 400.25, 2.0),  // trade before start
                (10, 1, 1, 400.25, 5.0),  // bid update before start
                (20, 2, 1, 400.5, 1.0),   // trade in range
                (30, 1, -1, 400.5, 0.0),  // ask cleared in range
                (40, 2, 0, 400.25, 1.0),  // trade at/after end
            ],
            2,
        );
        let start = (TS + 15).to_string();
        let end = (TS + 40).to_string();
        let progress = ProgressHandle::new();
        let all = read(&path, EventInterest::ALL, (&start, &end), None);
        let trades = read(&path, TRADES(), (&start, &end), Some(progress.clone()));
        let until = read(&path, TRADES(), ("", &(TS + 10).to_string()), None);
        let file_bytes = std::fs::metadata(&path).unwrap().len();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(
            all.unwrap(),
            [
                CONTRACT,
                level(15, 400.5, 1.0, Side::Sell),
                level(15, 400.25, 5.0, Side::Buy),
                trade(20, 400.5, 1.0, Side::Buy),
                level(30, 400.5, 0.0, Side::Sell),
            ]
        );
        assert_eq!(trades.unwrap(), [CONTRACT, trade(20, 400.5, 1.0, Side::Buy)]);
        // Only the [20, 30] group is read for trades.
        let snapshot = progress.snapshot();
        assert!(snapshot.total_bytes > 0 && snapshot.total_bytes < file_bytes / 2);
        assert_eq!(snapshot.bytes, snapshot.total_bytes);
        assert_eq!(until.unwrap(), [CONTRACT, trade(0, 400.25, 2.0, Side::Sell)]);
    }

    #[test]
    fn parses_range_bounds() {
        assert_eq!(parse_time(""), Ok(None));
        assert_eq!(parse_time("123"), Ok(Some(123)));
        assert_eq!(parse_time("2026-08-20"), Ok(Some(1_787_184_000_000_000_000)));
        assert_eq!(
            parse_time("2026-08-20 13:30"),
            Ok(Some(1_787_232_600_000_000_000))
        );
        assert_eq!(
            parse_time("2026-08-20T13:30:05Z"),
            Ok(Some(1_787_232_605_000_000_000))
        );
        for bad in ["2026-8-20", "2026-08-20 25:00", "yesterday", "2026-13-01"] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
    }
}
