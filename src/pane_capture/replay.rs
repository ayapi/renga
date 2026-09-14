//! Offline diagnostics: file/channel order is authoritative; event times may
//! decrease when a tick sampled its clock before waiting for a stream lock.
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

const DEFAULT_BURST_GAP_US: u64 = 40_000;

#[derive(Clone, Debug, PartialEq)]
struct Screen(Vec<String>);

impl Screen {
    fn of(parser: &vt100::Parser) -> Self {
        let mut rows: Vec<_> = parser
            .screen()
            .contents()
            .split('\n')
            .map(|row| row.trim_end().to_owned())
            .collect();
        rows.resize(parser.screen().size().0 as usize, String::new());
        Self(rows)
    }
}

/// swk/1fz text-only classes. Footer rows do not disqualify a shifted transcript.
fn class(before: &Screen, after: &Screen) -> String {
    if before == after {
        return "identical (none)".into();
    }
    if after.0.iter().all(String::is_empty) {
        return "cleared (full clear)".into();
    }
    let same = after
        .0
        .iter()
        .enumerate()
        .filter(|(i, row)| !row.is_empty() && before.0.get(*i) == Some(row))
        .count();
    let mut best = (0, 0);
    for k in 1..before.0.len() {
        let matches = after
            .0
            .iter()
            .enumerate()
            .filter(|(i, row)| !row.is_empty() && before.0.get(i + k) == Some(row))
            .count();
        if matches > best.1 {
            best = (k, matches);
        }
    }
    if best.1 >= 3 && best.1 > same {
        return format!("shifted({}) matches={} (scrolled)", best.0, best.1);
    }
    let changed = (0..before.0.len().max(after.0.len()))
        .filter(|&i| before.0.get(i) != after.0.get(i))
        .count();
    format!("rows_changed({changed})")
}

fn composition(pre: &Screen, post: &Screen, current: &Screen) -> String {
    let mut counts = [0; 4]; // post only, pre only, both, neither
    let mut saw_pre = false;
    let mut top_prefix = true;
    for (i, row) in current.0.iter().enumerate() {
        match (post.0.get(i) == Some(row), pre.0.get(i) == Some(row)) {
            (true, false) => {
                counts[0] += 1;
                if saw_pre {
                    top_prefix = false;
                }
            }
            (false, true) => {
                counts[1] += 1;
                saw_pre = true;
            }
            (true, true) => counts[2] += 1,
            (false, false) => {
                counts[3] += 1;
                top_prefix = false;
            }
        }
    }
    top_prefix &= counts[0] > 0 && counts[1] > 0;
    format!(
        "post_only={} pre_only={} both={} neither={} top_prefix={top_prefix}",
        counts[0], counts[1], counts[2], counts[3]
    )
}

struct Read {
    record: usize,
    offset: usize,
    len: usize,
    time: u64,
    pre: Screen,
    post: Screen,
}

struct Snapshot {
    record: usize,
    offset: usize,
    screen: Screen,
}

fn number(value: &Value, key: &str) -> Result<u64> {
    value[key]
        .as_u64()
        .with_context(|| format!("missing unsigned {key}"))
}

fn dimensions(record: &Value) -> Result<(u16, u16)> {
    let rows = u16::try_from(number(record, "rows")?)?;
    let cols = u16::try_from(number(record, "cols")?)?;
    ensure!(rows > 0 && cols > 0, "zero capture dimensions");
    Ok((rows, cols))
}

fn contains(data: &[u8], marker: &[u8]) -> bool {
    data.windows(marker.len()).any(|window| window == marker)
}

fn scrolling_counts(bytes: &[u8]) -> String {
    let mut counts = [0; 8];
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if bytes[i + 1] == b'M' {
            counts[5] += 1;
            i += 2;
            continue;
        }
        if bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let mut end = i + 2;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
            end += 1;
        }
        if end >= bytes.len() {
            break;
        }
        let index = match bytes[end] {
            b'r' => Some(0),
            b'S' => Some(1),
            b'T' => Some(2),
            b'L' => Some(3),
            b'M' => Some(4),
            b'K' => Some(6),
            b'J' => Some(7),
            _ => None,
        };
        if let Some(index) = index {
            counts[index] += 1;
        }
        i = end + 1;
    }
    format!(
        "DECSTBM={} SU={} SD={} IL={} DL={} RI={} EL={} ED={}",
        counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6], counts[7]
    )
}

fn table(
    out: &mut String,
    name: &str,
    burst: usize,
    pre: &Screen,
    post: &Screen,
    snapshots: &[&Screen],
) {
    let thirds = snapshots
        .iter()
        .filter(|screen| ***screen != *pre && ***screen != *post)
        .count();
    writeln!(
        out,
        "{name} burst={burst} before_equals_after={} third_states={thirds}/{} change={}",
        pre == post,
        snapshots.len(),
        class(pre, post)
    )
    .unwrap();
    let mut previous = pre;
    for (index, screen) in snapshots.iter().enumerate() {
        writeln!(
            out,
            "  snapshot={} change={}",
            index + 1,
            class(previous, screen)
        )
        .unwrap();
        if **screen != *pre && **screen != *post {
            writeln!(out, "    third {}", composition(pre, post, screen)).unwrap();
        }
        previous = screen;
    }
}

fn replay(binary: &[u8], records: &[Value], gap_us: u64) -> Result<String> {
    ensure!(gap_us > 0, "burst gap must be positive");
    let first = records.first().context("empty capture")?;
    ensure!(
        first["event"] == "metadata" && first["version"] == 1,
        "expected version 1 metadata"
    );
    let (rows, cols) = dimensions(first)?;
    let mut raw = vt100::Parser::new(rows, cols, 10000);
    let mut applied = vt100::Parser::new(rows, cols, 10000);
    let mut reads = Vec::<Read>::new();
    let mut applies = Vec::<Snapshot>::new();
    let mut draws = Vec::<Snapshot>::new();
    let mut raw_offset = 0;
    let mut applied_offset = 0;
    let mut out = String::new();
    let mut last_draw = Screen::of(&applied);
    for (index, record) in records.iter().enumerate() {
        ensure!(
            number(record, "sequence")? == index as u64,
            "noncontiguous sequence at record {index}"
        );
        let time = number(record, "elapsed_us")?;
        match record["event"].as_str().context("missing event")? {
            "metadata" if index == 0 => {}
            "read" => {
                let offset = usize::try_from(number(record, "bin_offset")?)?;
                let len = usize::try_from(number(record, "read_len")?)?;
                let end = offset.checked_add(len).context("read offset overflow")?;
                ensure!(
                    offset == raw_offset && len > 0 && end <= binary.len(),
                    "invalid read range at record {index}"
                );
                let pre = Screen::of(&raw);
                raw.process(&binary[offset..end]);
                reads.push(Read {
                    record: index,
                    offset,
                    len,
                    time,
                    pre,
                    post: Screen::of(&raw),
                });
                raw_offset = end;
            }
            "parser_apply" => {
                let offset = usize::try_from(number(record, "bin_offset")?)?;
                let end = usize::try_from(number(record, "applied_offset")?)?;
                ensure!(
                    offset == applied_offset
                        && end >= offset
                        && end <= raw_offset
                        && end - offset == number(record, "byte_len")? as usize,
                    "invalid apply range at record {index}"
                );
                applied.process(&binary[offset..end]);
                applied_offset = end;
                applies.push(Snapshot {
                    record: index,
                    offset: end,
                    screen: Screen::of(&applied),
                });
            }
            "resize" => {
                ensure!(
                    number(record, "applied_offset")? as usize == applied_offset,
                    "resize offset mismatch"
                );
                let (rows, cols) = dimensions(record)?;
                for parser in [&mut raw, &mut applied] {
                    parser.screen_mut().set_size(rows, cols);
                    if record["clear"] == true {
                        parser.process(b"\x1b[2J\x1b[H");
                    }
                }
            }
            "app_draw" => {
                ensure!(
                    number(record, "applied_offset")? as usize == applied_offset,
                    "draw offset mismatch"
                );
                if record["drawn"] != true {
                    writeln!(
                        out,
                        "draw seq={index} elapsed_us={time} drawn=false class=none hold={}",
                        record["deferred"]
                    )
                    .unwrap();
                    continue;
                }
                applied
                    .screen_mut()
                    .set_scrollback(number(record, "scrollback")? as usize);
                let screen = Screen::of(&applied);
                applied.screen_mut().set_scrollback(0);
                writeln!(out, "draw seq={index} elapsed_us={time} drawn=true applied_offset={applied_offset} class={} hold={}", class(&last_draw, &screen), record["deferred"]).unwrap();
                last_draw = screen.clone();
                draws.push(Snapshot {
                    record: index,
                    offset: applied_offset,
                    screen,
                });
            }
            "app_tick_release" | "reader_exit" => {}
            "transition" => {}
            event => bail!("unknown event {event} at record {index}"),
        }
    }
    if raw_offset < binary.len() {
        writeln!(out, "unindexed_binary_tail={}", binary.len() - raw_offset).unwrap();
    }
    let mut start = 0;
    let mut burst = 0;
    let mut non_other = 0;
    let has_draw_records = records.iter().any(|record| record["event"] == "app_draw");
    while start < reads.len() {
        let mut end = start + 1;
        while end < reads.len() && reads[end].time.saturating_sub(reads[end - 1].time) < gap_us {
            end += 1;
        }
        burst += 1;
        let first = &reads[start];
        let last = &reads[end - 1];
        let byte_end = last.offset + last.len;
        let bytes = &binary[first.offset..byte_end];
        let kind = if contains(bytes, b"\x1b[2J") || contains(bytes, b"\x1b[3J") {
            "ERASE"
        } else if contains(bytes, b"\x1b[2m\x1b[H") {
            "H-OPEN"
        } else {
            "other"
        };
        if kind != "other" {
            non_other += 1;
        }
        let largest_gap = reads[start..end]
            .windows(2)
            .map(|pair| pair[1].time.saturating_sub(pair[0].time))
            .max()
            .unwrap_or(0);
        writeln!(out, "burst={burst} kind={kind} reads={} bytes={} duration_us={} largest_gap_us={largest_gap} {}", end - start, bytes.len(), last.time.saturating_sub(first.time), scrolling_counts(bytes)).unwrap();
        let record_end = reads.get(end).map_or(records.len(), |read| read.record);
        let in_window =
            |snapshot: &&Snapshot| snapshot.record >= first.record && snapshot.record < record_end;
        table(
            &mut out,
            "raw",
            burst,
            &first.pre,
            &last.post,
            &reads[start..end - 1]
                .iter()
                .map(|read| &read.post)
                .collect::<Vec<_>>(),
        );
        table(
            &mut out,
            "applied",
            burst,
            &first.pre,
            &last.post,
            &applies
                .iter()
                .filter(|snapshot| snapshot.offset > first.offset && snapshot.offset <= byte_end)
                .map(|snapshot| &snapshot.screen)
                .collect::<Vec<_>>(),
        );
        table(
            &mut out,
            "drawn",
            burst,
            &first.pre,
            &last.post,
            &draws
                .iter()
                .filter(in_window)
                .map(|snapshot| &snapshot.screen)
                .collect::<Vec<_>>(),
        );
        for hide in first.offset..byte_end {
            if !binary[hide..raw_offset].starts_with(b"\x1b[?25l") {
                continue;
            }
            let show = binary[hide + 6..raw_offset]
                .windows(6)
                .position(|window| window == b"\x1b[?25h")
                .map(|index| hide + 6 + index + 6);
            let envelope_end = show.unwrap_or(raw_offset);
            let first_read = reads
                .iter()
                .position(|read| hide < read.offset + read.len)
                .unwrap();
            let last_read = reads
                .iter()
                .position(|read| envelope_end <= read.offset + read.len)
                .unwrap();
            let draw_count = if has_draw_records {
                draws
                    .iter()
                    .filter(|draw| draw.offset > hide && draw.offset < envelope_end)
                    .count()
                    .to_string()
            } else {
                "n/a".into()
            };
            writeln!(out, "  envelope=[{hide},{envelope_end}) complete={} first_read={} last_read={} reads={} spans_multiple_reads={} duration_us={} draws={draw_count}", show.is_some(), first_read + 1, last_read + 1, last_read - first_read + 1, first_read != last_read, reads[last_read].time.saturating_sub(reads[first_read].time)).unwrap();
        }
        start = end;
    }
    writeln!(
        out,
        "totals reads={} bytes={raw_offset} bursts={burst} non_other_bursts={non_other} draws={}",
        reads.len(),
        draws.len()
    )
    .unwrap();
    Ok(out)
}

fn replay_file(path: &Path, gap_us: u64) -> Result<String> {
    let binary = std::fs::read(path.with_extension("bin"))?;
    let jsonl = std::fs::read_to_string(path.with_extension("jsonl"))?;
    let records = jsonl
        .lines()
        .enumerate()
        .map(|(line, text)| {
            serde_json::from_str(text).with_context(|| format!("invalid JSONL line {}", line + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    replay(&binary, &records, gap_us)
}

#[test]
#[ignore = "offline helper: set RENGA_PANE_REPLAY to pane-ID.jsonl"]
fn replay_capture() {
    let path =
        std::env::var_os("RENGA_PANE_REPLAY").expect("set RENGA_PANE_REPLAY to pane-ID.jsonl");
    let gap = std::env::var("RENGA_PANE_REPLAY_GAP_US")
        .ok()
        .map(|value| value.parse().expect("integer gap us"))
        .unwrap_or(DEFAULT_BURST_GAP_US);
    print!(
        "{}",
        replay_file(Path::new(&path), gap).expect("valid capture")
    );
}

#[test]
fn shifted_footer_and_partial_top_prefix_are_measured() {
    let screen = |rows: &[&str]| Screen(rows.iter().map(|row| (*row).into()).collect());
    let pre = screen(&["a", "b", "c", "d", "e", "footer"]);
    let post = screen(&["b", "c", "d", "e", "f", "footer"]);
    assert_eq!(class(&pre, &post), "shifted(1) matches=4 (scrolled)");
    let partial = screen(&["b", "c", "c", "d", "e", "footer"]);
    assert_ne!(partial, pre);
    assert_ne!(partial, post);
    assert_eq!(
        composition(&pre, &post, &partial),
        "post_only=2 pre_only=3 both=1 neither=0 top_prefix=true"
    );
}

#[test]
fn synthetic_replay_tracks_resize_split_envelope_and_read_only_input() {
    let mut records = Vec::new();
    let mut add = |mut value: Value| {
        value["sequence"] = records.len().into();
        value["elapsed_us"] = (records.len() * 1000).into();
        records.push(value);
    };
    add(serde_json::json!({"event":"metadata","version":1,"rows":6,"cols":20}));
    let a = b"\x1b[?25la\r\nb\r\nc\r\nd\r\ne\r\nfooter";
    let b = b"\x1b[?25h";
    add(
        serde_json::json!({"event":"read","bin_offset":0,"read_len":a.len(),"deferred":{"kind":"none"}}),
    );
    add(
        serde_json::json!({"event":"parser_apply","bin_offset":0,"byte_len":a.len(),"applied_offset":a.len()}),
    );
    add(
        serde_json::json!({"event":"app_draw","drawn":true,"applied_offset":a.len(),"scrollback":0,"deferred":{"kind":"none"}}),
    );
    add(
        serde_json::json!({"event":"resize","rows":6,"cols":21,"clear":true,"applied_offset":a.len()}),
    );
    add(
        serde_json::json!({"event":"app_draw","drawn":true,"applied_offset":a.len(),"scrollback":0,"deferred":{"kind":"none"}}),
    );
    add(
        serde_json::json!({"event":"read","bin_offset":a.len(),"read_len":b.len(),"deferred":{"kind":"none"}}),
    );
    add(
        serde_json::json!({"event":"parser_apply","bin_offset":a.len(),"byte_len":b.len(),"applied_offset":a.len()+b.len()}),
    );
    let binary = [a.as_slice(), b].concat();
    let report = replay(&binary, &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(report.contains("class=cleared (full clear)"));
    assert!(
        report.contains("spans_multiple_reads=true duration_us=5000 draws=2"),
        "{report}"
    );
    records.retain(|record| record["event"] != "app_draw");
    for (index, record) in records.iter_mut().enumerate() {
        record["sequence"] = index.into();
    }
    let report = replay(&binary, &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(report.contains("draws=n/a"));
    assert!(report.contains("raw burst=1"));
}

#[test]
fn burst_tables_keep_raw_thirds_hidden_by_applied_hold_and_ignore_bare_home() {
    let chunks: &[&[u8]] = &[
        b"old",
        b"\x1b[2J\x1b[H",
        b"new",
        b"\x1b[2m\x1b[Hnew",
        b"\x1b[H\x1b[?25h",
    ];
    let times = [0, 50_000, 60_000, 100_000, 140_000];
    let mut records = vec![
        serde_json::json!({"event":"metadata","version":1,"rows":3,"cols":20,"sequence":0,"elapsed_us":0}),
    ];
    let mut offset = 0;
    let mut applied_offset = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        records.push(serde_json::json!({"event":"read","sequence":records.len(),"elapsed_us":times[index],"bin_offset":offset,"read_len":chunk.len()}));
        offset += chunk.len();
        if index == 1 {
            continue;
        }
        records.push(serde_json::json!({"event":"parser_apply","sequence":records.len(),"elapsed_us":times[index],"bin_offset":applied_offset,"byte_len":offset-applied_offset,"applied_offset":offset}));
        applied_offset = offset;
        records.push(serde_json::json!({"event":"app_draw","sequence":records.len(),"elapsed_us":times[index],"drawn":true,"applied_offset":offset,"scrollback":0,"deferred":{"kind":"none"}}));
    }
    let report = replay(&chunks.concat(), &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(report.contains("burst=2 kind=ERASE reads=2"), "{report}");
    assert!(report.contains("raw burst=2 before_equals_after=false third_states=1/1"));
    assert!(report.contains("applied burst=2 before_equals_after=false third_states=0/1"));
    assert!(report.contains("drawn burst=2 before_equals_after=false third_states=0/1"));
    assert!(report.contains("burst=3 kind=H-OPEN"));
    assert!(report.contains("burst=4 kind=other"));
    assert!(report.contains("non_other_bursts=2"));
    // An earlier sampled draw timestamp does not reorder parser activity.
    records[3]["elapsed_us"] = 0.into();
    assert!(replay(&chunks.concat(), &records, DEFAULT_BURST_GAP_US).is_ok());
}
