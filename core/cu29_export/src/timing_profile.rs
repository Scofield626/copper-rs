//! Per-task per-cycle `process()` timing extractor.
//!
//! Walks a recorded `.copper` log and emits, for every `CuMsg`, the
//! `process_time` window that the runtime stamped via the
//! `#[copper_runtime]` macro. Three output formats are supported:
//!
//! - [`TimingFormat::Csv`] for grep/pandas/gnuplot pipelines.
//! - [`TimingFormat::Json`] for programmatic consumers.
//! - [`TimingFormat::ChromeTrace`] for visualization in
//!   [Perfetto](https://ui.perfetto.dev) and
//!   [Speedscope](https://www.speedscope.app/) (which also exposes a
//!   flamegraph view over the same data).
//!
//! Known limitations of the underlying instrumentation:
//!
//! - Sinks have no output message and are therefore not captured here.
//! - Bridge-fed copperlist slots (origin id `bridge::<spec>::<dir>::<chan>`)
//!   only appear if the bridge stamps `process_time` on its outgoing
//!   message; otherwise the row is silently dropped.
//! - Output is strictly one row per stamped output message. Aggregations
//!   like whole-cycle wall-clock time are left to the consumer (e.g. group
//!   by `culist_id` and take `max(end_ns) - min(start_ns)`).

use crate::copperlists_reader;
use clap::ValueEnum;
use cu29::clock::OptionCuTime;
use cu29::prelude::{CopperListTuple, CuMsgMetadataTrait};
use cu29::{CuError, CuResult};
use serde::Serialize;
use std::fmt::{Display, Formatter};
use std::io::{Read, Write};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum TimingFormat {
    Csv,
    Json,
    ChromeTrace,
}

impl Display for TimingFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TimingFormat::Csv => write!(f, "csv"),
            TimingFormat::Json => write!(f, "json"),
            TimingFormat::ChromeTrace => write!(f, "chrome-trace"),
        }
    }
}

/// One row of timing data: a single message stamped by one task.
#[derive(Debug, Clone, Serialize)]
pub struct TaskTiming {
    pub culist_id: u64,
    pub task_index: usize,
    pub task_id: Option<String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ns: u64,
}

/// Reads copperlists from `reader` and writes per-task timing rows to
/// `writer` in the requested `format`.
///
/// Messages whose `process_time.start` or `process_time.end` is undefined
/// are skipped silently (they are emitted as `none()` by the runtime when
/// the message was not produced by a stamped task).
pub fn export_timing_profile<P>(
    reader: impl Read,
    mut writer: impl Write,
    format: TimingFormat,
) -> CuResult<()>
where
    P: CopperListTuple,
{
    let task_ids = P::get_all_task_ids();

    match format {
        TimingFormat::Csv => write_csv_header(&mut writer)?,
        TimingFormat::Json => write_all_bytes(&mut writer, b"[\n")?,
        TimingFormat::ChromeTrace => write_all_bytes(&mut writer, b"[\n")?,
    }

    let mut first_record = true;
    for culist in copperlists_reader::<P>(reader) {
        let culist_id = culist.id;
        let msgs = culist.msgs.cumsgs();
        for (task_index, msg) in msgs.iter().enumerate() {
            let Some(row) = extract_row(culist_id, task_index, task_ids, msg.metadata()) else {
                continue;
            };
            match format {
                TimingFormat::Csv => write_csv_row(&mut writer, &row)?,
                TimingFormat::Json => {
                    write_json_row(&mut writer, &row, first_record)?;
                    first_record = false;
                }
                TimingFormat::ChromeTrace => {
                    write_chrome_event(&mut writer, &row, first_record)?;
                    first_record = false;
                }
            }
        }
    }

    match format {
        TimingFormat::Csv => {}
        TimingFormat::Json | TimingFormat::ChromeTrace => write_all_bytes(&mut writer, b"\n]\n")?,
    }
    Ok(())
}

fn extract_row(
    culist_id: u64,
    task_index: usize,
    task_ids: &'static [&'static str],
    meta: &dyn CuMsgMetadataTrait,
) -> Option<TaskTiming> {
    let process_time = meta.process_time();
    let start_ns = option_time_ns(process_time.start)?;
    let end_ns = option_time_ns(process_time.end)?;
    let duration_ns = end_ns.saturating_sub(start_ns);
    let task_id = task_ids.get(task_index).map(|s| (*s).to_string());
    Some(TaskTiming {
        culist_id,
        task_index,
        task_id,
        start_ns,
        end_ns,
        duration_ns,
    })
}

fn option_time_ns(value: OptionCuTime) -> Option<u64> {
    Option::<cu29::clock::CuTime>::from(value).map(|t| t.as_nanos())
}

fn write_all_bytes(writer: &mut impl Write, bytes: &[u8]) -> CuResult<()> {
    writer
        .write_all(bytes)
        .map_err(|e| CuError::new_with_cause("Failed to write timing profile output", e))
}

fn write_csv_header(writer: &mut impl Write) -> CuResult<()> {
    write_all_bytes(
        writer,
        b"culist_id,task_index,task_id,start_ns,end_ns,duration_ns\n",
    )
}

fn write_csv_row(writer: &mut impl Write, row: &TaskTiming) -> CuResult<()> {
    let task_id = row.task_id.as_deref().unwrap_or("");
    let line = format!(
        "{},{},{},{},{},{}\n",
        row.culist_id,
        row.task_index,
        csv_quote(task_id),
        row.start_ns,
        row.end_ns,
        row.duration_ns
    );
    write_all_bytes(writer, line.as_bytes())
}

/// RFC 4180 quoting: wrap in double quotes and double internal quotes if the
/// field contains a comma, quote, CR, or LF; otherwise pass through. Today's
/// task ids and bridge origin ids never trigger the slow path — this is a
/// defensive guard for future config changes.
fn csv_quote(field: &str) -> std::borrow::Cow<'_, str> {
    if field.contains([',', '"', '\n', '\r']) {
        std::borrow::Cow::Owned(format!("\"{}\"", field.replace('"', "\"\"")))
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

fn write_json_row(writer: &mut impl Write, row: &TaskTiming, first: bool) -> CuResult<()> {
    if !first {
        write_all_bytes(writer, b",\n")?;
    }
    serde_json::to_writer(&mut *writer, row)
        .map_err(|e| CuError::new_with_cause("Failed to serialize timing row to JSON", e))
}

fn write_chrome_event(writer: &mut impl Write, row: &TaskTiming, first: bool) -> CuResult<()> {
    if !first {
        write_all_bytes(writer, b",\n")?;
    }
    // Chrome Trace Event Format expects µs for `ts` and `dur` (floating-point ok).
    let ts_us = row.start_ns as f64 / 1_000.0;
    let dur_us = row.duration_ns as f64 / 1_000.0;
    let name = row
        .task_id
        .clone()
        .unwrap_or_else(|| format!("task_{}", row.task_index));
    let event = ChromeEvent {
        name,
        ph: "X",
        ts: ts_us,
        dur: dur_us,
        pid: 0,
        tid: row.task_index as u64,
        args: ChromeArgs {
            culist_id: row.culist_id,
        },
    };
    serde_json::to_writer(&mut *writer, &event)
        .map_err(|e| CuError::new_with_cause("Failed to serialize chrome trace event", e))
}

#[derive(Serialize)]
struct ChromeEvent {
    name: String,
    ph: &'static str,
    ts: f64,
    dur: f64,
    pid: u64,
    tid: u64,
    args: ChromeArgs,
}

#[derive(Serialize)]
struct ChromeArgs {
    culist_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::config::standard;
    use bincode::{Decode, Encode, encode_into_slice};
    use cu29::clock::{CuTime, PartialCuTimeRange};
    use cu29::prelude::*;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::io::Cursor;

    #[derive(
        Debug, Clone, Copy, Default, PartialEq, Encode, Decode, Serialize, Deserialize, Reflect,
    )]
    struct TestPayload(u32);

    /// Three messages so we can verify multi-task output.
    #[derive(Debug, Clone, Default, Encode, Decode, Serialize, Deserialize)]
    struct TestMsgs {
        a: CuMsg<TestPayload>,
        b: CuMsg<TestPayload>,
        c: CuMsg<TestPayload>,
    }

    impl ErasedCuStampedDataSet for TestMsgs {
        fn cumsgs(&self) -> Vec<&dyn ErasedCuStampedData> {
            vec![&self.a, &self.b, &self.c]
        }
    }

    impl MatchingTasks for TestMsgs {
        fn get_all_task_ids() -> &'static [&'static str] {
            &["src", "transform", "downstream"]
        }
    }

    impl CuPayloadRawBytes for TestMsgs {
        fn payload_raw_bytes(&self) -> Vec<Option<u64>> {
            vec![Some(4), Some(4), Some(4)]
        }
    }

    fn make_msg(start_ns: u64, end_ns: u64, payload: u32) -> CuMsg<TestPayload> {
        let mut msg = CuMsg::<TestPayload>::new(Some(TestPayload(payload)));
        msg.metadata.process_time = PartialCuTimeRange {
            start: OptionCuTime::from(CuTime::from(start_ns)),
            end: OptionCuTime::from(CuTime::from(end_ns)),
        };
        msg
    }

    fn make_msg_unstamped(payload: u32) -> CuMsg<TestPayload> {
        CuMsg::<TestPayload>::new(Some(TestPayload(payload)))
    }

    fn encode_log(culists: &[CopperList<TestMsgs>]) -> Vec<u8> {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut offset = 0;
        for cl in culists {
            offset += encode_into_slice(cl, &mut buffer[offset..], standard()).unwrap();
        }
        buffer.truncate(offset);
        buffer
    }

    fn synthesized_log() -> Vec<u8> {
        let mut cl0 = CopperList::<TestMsgs>::new(0, TestMsgs::default());
        cl0.msgs.a = make_msg(1_000, 1_500, 10);
        cl0.msgs.b = make_msg(2_000, 2_300, 20);
        cl0.msgs.c = make_msg(3_000, 4_100, 30);

        let mut cl1 = CopperList::<TestMsgs>::new(1, TestMsgs::default());
        cl1.msgs.a = make_msg(11_000, 11_400, 11);
        cl1.msgs.b = make_msg(12_000, 12_300, 21);
        cl1.msgs.c = make_msg(13_000, 14_500, 31);

        encode_log(&[cl0, cl1])
    }

    #[test]
    fn csv_emits_one_row_per_message() {
        let bytes = synthesized_log();
        let mut out = Vec::new();
        export_timing_profile::<TestMsgs>(Cursor::new(bytes), &mut out, TimingFormat::Csv).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 7); // header + 2 cycles * 3 messages
        assert_eq!(
            lines[0],
            "culist_id,task_index,task_id,start_ns,end_ns,duration_ns"
        );
        assert_eq!(lines[1], "0,0,src,1000,1500,500");
        assert_eq!(lines[3], "0,2,downstream,3000,4100,1100");
        assert_eq!(lines[4], "1,0,src,11000,11400,400");
    }

    #[test]
    fn json_emits_array_of_records() {
        let bytes = synthesized_log();
        let mut out = Vec::new();
        export_timing_profile::<TestMsgs>(Cursor::new(bytes), &mut out, TimingFormat::Json)
            .unwrap();
        let parsed: Vec<Value> = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[0]["culist_id"], 0);
        assert_eq!(parsed[0]["task_id"], "src");
        assert_eq!(parsed[0]["duration_ns"], 500);
        assert_eq!(parsed[5]["culist_id"], 1);
        assert_eq!(parsed[5]["task_index"], 2);
    }

    #[test]
    fn chrome_trace_emits_complete_events_in_microseconds() {
        let bytes = synthesized_log();
        let mut out = Vec::new();
        export_timing_profile::<TestMsgs>(Cursor::new(bytes), &mut out, TimingFormat::ChromeTrace)
            .unwrap();
        let parsed: Vec<Value> = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[0]["ph"], "X");
        assert_eq!(parsed[0]["name"], "src");
        assert_eq!(parsed[0]["tid"], 0);
        assert_eq!(parsed[0]["args"]["culist_id"], 0);
        assert!((parsed[0]["ts"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert!((parsed[0]["dur"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        // Second cycle, third message: start 13_000 ns = 13.0 µs, dur 1.5 µs.
        assert!((parsed[5]["ts"].as_f64().unwrap() - 13.0).abs() < 1e-9);
        assert!((parsed[5]["dur"].as_f64().unwrap() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn skips_messages_without_stamped_timings() {
        let mut cl = CopperList::<TestMsgs>::new(7, TestMsgs::default());
        cl.msgs.a = make_msg(100, 250, 1);
        cl.msgs.b = make_msg_unstamped(2);
        cl.msgs.c = make_msg(900, 1_400, 3);
        let bytes = encode_log(&[cl]);

        let mut out = Vec::new();
        export_timing_profile::<TestMsgs>(Cursor::new(bytes), &mut out, TimingFormat::Csv).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 stamped messages, unstamped skipped
        assert!(lines[1].starts_with("7,0,src,100,250,150"));
        assert!(lines[2].starts_with("7,2,downstream,900,1400,500"));
    }

    #[test]
    fn csv_quote_escapes_special_characters() {
        assert_eq!(csv_quote("simple"), "simple");
        assert_eq!(csv_quote("bridge::foo::rx::ch"), "bridge::foo::rx::ch");
        assert_eq!(csv_quote("with,comma"), "\"with,comma\"");
        assert_eq!(csv_quote("with\"quote"), "\"with\"\"quote\"");
        assert_eq!(csv_quote("with\nnewline"), "\"with\nnewline\"");
    }
}
