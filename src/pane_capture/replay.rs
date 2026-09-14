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

fn partial_overwrite_chars(pre: &str, post: &str, current: &str) -> Option<usize> {
    let pre: Vec<_> = pre.chars().collect();
    let post: Vec<_> = post.chars().collect();
    (1..post.len()).find(|&k| {
        post[..k]
            .iter()
            .chain(pre.iter().skip(k))
            .copied()
            .eq(current.chars())
    })
}

fn composition(pre: &Screen, post: &Screen, current: &Screen) -> String {
    let mut counts = [0; 4]; // post only, pre only, both, neither
    let mut saw_pre = false;
    let mut top_prefix = true;
    let mut partial = None;
    for (i, row) in current.0.iter().enumerate() {
        match (post.0.get(i) == Some(row), pre.0.get(i) == Some(row)) {
            (true, false) => {
                counts[0] += 1;
                if saw_pre || partial.is_some() {
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
                let k = pre
                    .0
                    .get(i)
                    .zip(post.0.get(i))
                    .and_then(|(pre, post)| partial_overwrite_chars(pre, post, row));
                if let Some(k) = k.filter(|_| !saw_pre && partial.is_none()) {
                    partial = Some((i, k));
                } else {
                    top_prefix = false;
                }
            }
        }
    }
    top_prefix &= counts[0] > 0 && counts[1] > 0;
    let (partial_row, partial_chars) = partial
        .map(|(row, chars)| (row.to_string(), chars.to_string()))
        .unwrap_or_else(|| ("none".into(), "none".into()));
    format!(
        "post_only={} pre_only={} both={} neither={} top_prefix={top_prefix} partial_row={partial_row} partial_chars={partial_chars}",
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
    scrollback: usize,
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

fn apply_resize(parser: &mut vt100::Parser, record: &Value) -> Result<()> {
    let (rows, cols) = dimensions(record)?;
    parser.screen_mut().set_size(rows, cols);
    if record["clear"] == true {
        parser.process(b"\x1b[2J\x1b[H");
    }
    Ok(())
}

/// Raw replay has already consumed held reads when their resize record arrives.
/// Pre-index resize injections and splice them into the raw byte order instead.
fn advance_raw(
    parser: &mut vt100::Parser,
    binary: &[u8],
    offset: &mut usize,
    resizes: &[&Value],
    next_resize: &mut usize,
    end: usize,
) -> Result<()> {
    while let Some(record) = resizes.get(*next_resize) {
        let position = usize::try_from(number(record, "applied_offset")?)?;
        if position > end {
            break;
        }
        ensure!(
            position >= *offset,
            "resize offsets must follow parser application order"
        );
        parser.process(&binary[*offset..position]);
        apply_resize(parser, record)?;
        *offset = position;
        *next_resize += 1;
    }
    parser.process(&binary[*offset..end]);
    *offset = end;
    Ok(())
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

/// Locate erase commands and printable payload runs, excluding CSI/OSC/DCS
/// housekeeping. A home or cursor-hide after an erase is not a rewrite byte.
fn repaint_tokens(bytes: &[u8]) -> (Vec<std::ops::Range<usize>>, Vec<usize>) {
    let mut erases = Vec::new();
    let mut payloads = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            let start = index;
            index += 1;
            let Some(&command) = bytes.get(index) else {
                break;
            };
            index += 1;
            match command {
                b'[' => {
                    while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                        index += 1;
                    }
                    if index < bytes.len() {
                        index += 1;
                    }
                    if matches!(&bytes[start..index], b"\x1b[2J" | b"\x1b[3J") {
                        erases.push(start..index);
                    }
                }
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    while index < bytes.len() {
                        if command == b']' && bytes[index] == 7 {
                            index += 1;
                            break;
                        }
                        if bytes[index..].starts_with(b"\x1b\\") {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                0x20..=0x2f => {
                    while index < bytes.len() && (0x20..=0x2f).contains(&bytes[index]) {
                        index += 1;
                    }
                    if index < bytes.len() {
                        index += 1;
                    }
                }
                _ => {}
            }
        } else if bytes[index] >= 0x20 && bytes[index] != 0x7f {
            payloads.push(index);
            while index < bytes.len() && bytes[index] >= 0x20 && bytes[index] != 0x7f {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    (erases, payloads)
}

fn erase_hold_close(records: &[Value], erase_offset: usize) -> Option<(usize, &Value)> {
    let is_transition = |record: &Value, action: &str| {
        record["event"] == "transition"
            && record["kind"] == "erase_hold"
            && record["action"] == action
    };
    let marker = records.iter().position(|record| {
        record["event"] == "transition"
            && record["action"] == "marker"
            && record["bin_offset"].as_u64() == Some(erase_offset as u64)
    });
    let direct_open = records.iter().position(|record| {
        is_transition(record, "open") && record["bin_offset"].as_u64() == Some(erase_offset as u64)
    });
    let open = direct_open.or_else(|| {
        let marker = marker?;
        records[..marker]
            .iter()
            .rposition(|record| is_transition(record, "open"))
            .filter(|&open| {
                !records[open + 1..marker]
                    .iter()
                    .any(|record| is_transition(record, "close"))
            })
    })?;
    records
        .iter()
        .enumerate()
        .skip(open + 1)
        .find(|(_, record)| is_transition(record, "close"))
}

/// Link erase-only bursts to the later payload burst. This is an association
/// table, not a change to the 40 ms burst definition used by the other tables.
fn erase_rewrite_table(
    out: &mut String,
    binary: &[u8],
    records: &[Value],
    reads: &[Read],
    draws: &[Snapshot],
    gap_us: u64,
) -> Result<()> {
    let (erases, payloads) = repaint_tokens(binary);
    let mut bursts = Vec::new();
    let mut start = 0;
    while start < reads.len() {
        let mut end = start + 1;
        while end < reads.len() && reads[end].time.saturating_sub(reads[end - 1].time) < gap_us {
            end += 1;
        }
        bursts.push(start..end);
        start = end;
    }
    let has_draws = records.iter().any(|record| record["event"] == "app_draw");
    for (erase_index, erase) in erases.iter().enumerate() {
        // A split erase is recognized when its final byte arrives.
        let erase_read = reads.partition_point(|read| read.offset + read.len < erase.end);
        let burst = bursts.partition_point(|range| range.end <= erase_read);
        let read = &reads[erase_read];
        let next_erase = erases
            .get(erase_index + 1)
            .map_or(binary.len(), |range| range.start);
        // A resize injects another clear and ends attribution to this raw erase.
        let next_resize = records
            .iter()
            .enumerate()
            .skip(read.record + 1)
            .find(|(_, record)| record["event"] == "resize" && record["clear"] == true)
            .map(|(sequence, _)| sequence);
        let resize_limit = next_resize
            .map(|sequence| {
                reads
                    .iter()
                    .find(|read| read.record > sequence)
                    .map_or(binary.len(), |read| read.offset)
            })
            .unwrap_or(binary.len());
        let limit = next_erase.min(resize_limit);
        let rewrite_start = payloads
            .get(payloads.partition_point(|&offset| offset < erase.end))
            .copied()
            .filter(|&offset| offset < limit);
        let close = erase_hold_close(records, erase.start);
        let reason = close
            .and_then(|(_, record)| record["reason"].as_str())
            .unwrap_or("unknown");
        let (delay, before, rewrite_bytes, rewrite_reads, inside_draws, span) =
            if let Some(offset) = rewrite_start {
                let first = reads.partition_point(|read| read.offset + read.len <= offset);
                let rewrite_burst = bursts.partition_point(|range| range.end <= first);
                let last = &reads[bursts[rewrite_burst].end - 1];
                let end = (last.offset + last.len).min(limit);
                let count = reads[first..]
                    .iter()
                    .take_while(|read| read.offset < end)
                    .count();
                let before = close
                    .map(|(sequence, record)| {
                        Ok::<_, anyhow::Error>(
                            sequence < reads[first].record
                                && number(record, "elapsed_us")? < reads[first].time,
                        )
                    })
                    .transpose()?
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let inside = if has_draws {
                    draws
                        .iter()
                        .filter(|draw| {
                            draw.scrollback == 0
                                && draw.offset > offset
                                && draw.offset < end
                                && next_resize.is_none_or(|sequence| draw.record < sequence)
                        })
                        .count()
                        .to_string()
                } else {
                    "n/a".into()
                };
                (
                    reads[first].time.saturating_sub(read.time).to_string(),
                    before,
                    end - offset,
                    count,
                    inside,
                    format!("[{offset},{end})"),
                )
            } else {
                (
                    "n/a".into(),
                    if close.is_some() { "true" } else { "unknown" }.into(),
                    0,
                    0,
                    if has_draws { "0" } else { "n/a" }.into(),
                    "none".into(),
                )
            };
        writeln!(out, "erase_rewrite kind=PTY_ERASE burst={} erase_offset={} erase_read={} erase_elapsed_us={} first_rewrite_delay_us={delay} hold_close_reason={reason} closed_before_rewrite={before} rewrite_span={span} rewrite_bytes={rewrite_bytes} rewrite_reads={rewrite_reads} draws_inside_rewrite={inside_draws}", burst + 1, erase.start, erase_read + 1, read.time).unwrap();
    }
    for (sequence, record) in records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["event"] == "resize" && record["clear"] == true)
    {
        writeln!(out, "erase_rewrite kind=RESIZE_CLEAR seq={sequence} elapsed_us={} applied_offset={} injected_bytes=7 hold_close_reason=not_applicable closed_before_rewrite=not_applicable", number(record, "elapsed_us")?, number(record, "applied_offset")?).unwrap();
    }
    Ok(())
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
    let resizes: Vec<_> = records
        .iter()
        .filter(|record| record["event"] == "resize")
        .collect();
    let mut next_resize = 0;
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
                advance_raw(
                    &mut raw,
                    binary,
                    &mut raw_offset,
                    &resizes,
                    &mut next_resize,
                    offset,
                )?;
                let pre = Screen::of(&raw);
                advance_raw(
                    &mut raw,
                    binary,
                    &mut raw_offset,
                    &resizes,
                    &mut next_resize,
                    end,
                )?;
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
                    scrollback: 0,
                });
            }
            "resize" => {
                ensure!(
                    number(record, "applied_offset")? as usize == applied_offset,
                    "resize offset mismatch"
                );
                apply_resize(&mut applied, record)?;
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
                let scrollback = number(record, "scrollback")? as usize;
                applied.screen_mut().set_scrollback(scrollback);
                let screen = Screen::of(&applied);
                applied.screen_mut().set_scrollback(0);
                let change = if scrollback > 0 {
                    format!("scrolled_view scrollback={scrollback}")
                } else {
                    class(&last_draw, &screen)
                };
                writeln!(out, "draw seq={index} elapsed_us={time} drawn=true applied_offset={applied_offset} class={change} hold={}", record["deferred"]).unwrap();
                if scrollback == 0 {
                    last_draw = screen.clone();
                }
                draws.push(Snapshot {
                    record: index,
                    offset: applied_offset,
                    screen,
                    scrollback,
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
        let in_window = |snapshot: &&Snapshot| {
            snapshot.scrollback == 0
                && snapshot.record >= first.record
                && snapshot.record < record_end
        };
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
                    .filter(|draw| {
                        draw.scrollback == 0 && draw.offset > hide && draw.offset < envelope_end
                    })
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
    erase_rewrite_table(
        &mut out,
        &binary[..raw_offset],
        records,
        &reads,
        &draws,
        gap_us,
    )?;
    Ok(out)
}

pub(crate) fn replay_file(path: &Path, gap_us: u64) -> Result<String> {
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
        "post_only=2 pre_only=3 both=1 neither=0 top_prefix=true partial_row=none partial_chars=none"
    );
}

#[test]
fn review_resize_while_bytes_held_uses_applied_position_for_raw_truth() {
    let first = b"\x1b[?2026hhello";
    let second = b"\x1b[?2026l";
    let binary = [first.as_slice(), second].concat();
    let records = vec![
        serde_json::json!({"sequence":0,"elapsed_us":0,"event":"metadata","version":1,"rows":3,"cols":20}),
        serde_json::json!({"sequence":1,"elapsed_us":1000,"event":"read","bin_offset":0,"read_len":first.len()}),
        serde_json::json!({"sequence":2,"elapsed_us":2000,"event":"resize","rows":4,"cols":21,"clear":true,"applied_offset":0}),
        serde_json::json!({"sequence":3,"elapsed_us":3000,"event":"read","bin_offset":first.len(),"read_len":second.len()}),
        serde_json::json!({"sequence":4,"elapsed_us":3000,"event":"parser_apply","bin_offset":0,"byte_len":binary.len(),"applied_offset":binary.len()}),
        serde_json::json!({"sequence":5,"elapsed_us":4000,"event":"app_draw","drawn":true,"applied_offset":binary.len(),"scrollback":0}),
    ];
    let mut truth = vt100::Parser::new(3, 20, 10000);
    truth.screen_mut().set_size(4, 21);
    truth.process(b"\x1b[2J\x1b[H");
    truth.process(&binary);
    assert_eq!(truth.screen().contents(), "hello");
    let report = replay(&binary, &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(
        report.contains("raw burst=1 before_equals_after=false third_states=0/1"),
        "{report}"
    );
    assert!(report.contains("applied burst=1 before_equals_after=false third_states=0/1"));
    assert!(report.contains("drawn burst=1 before_equals_after=false third_states=0/1"));
    assert!(report.contains("erase_rewrite kind=RESIZE_CLEAR"));
}

#[test]
fn review_scrollback_draw_is_scrolled_view_and_excluded_from_thirds() {
    let binary = b"one\r\ntwo\r\nthree\r\nfour";
    let records = vec![
        serde_json::json!({"sequence":0,"elapsed_us":0,"event":"metadata","version":1,"rows":2,"cols":20}),
        serde_json::json!({"sequence":1,"elapsed_us":1000,"event":"read","bin_offset":0,"read_len":binary.len()}),
        serde_json::json!({"sequence":2,"elapsed_us":1000,"event":"parser_apply","bin_offset":0,"byte_len":binary.len(),"applied_offset":binary.len()}),
        serde_json::json!({"sequence":3,"elapsed_us":2000,"event":"app_draw","drawn":true,"applied_offset":binary.len(),"scrollback":1}),
        serde_json::json!({"sequence":4,"elapsed_us":3000,"event":"app_draw","drawn":true,"applied_offset":binary.len(),"scrollback":0}),
    ];
    let report = replay(binary, &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(
        report.contains("class=scrolled_view scrollback=1"),
        "{report}"
    );
    assert!(report.contains("drawn burst=1 before_equals_after=false third_states=0/1"));
}

#[test]
fn review_mid_row_overwrite_preserves_top_prefix_signature() {
    let screen = |rows: &[&str]| Screen(rows.iter().map(|row| (*row).into()).collect());
    let pre = screen(&["alpha", "bravo", "charlie", "echo", "footer"]);
    let post = screen(&["bravo", "charlie", "delta", "foxtrot", "footer"]);
    let current = screen(&["bravo", "charlie", "delrlie", "echo", "footer"]);
    let result = composition(&pre, &post, &current);
    assert!(result.contains("top_prefix=true"), "{result}");
    assert!(result.contains("partial_row=2 partial_chars=3"), "{result}");
}

#[test]
fn shifted_class_requires_three_matches_and_more_than_unchanged_rows() {
    let screen = |rows: &[&str]| Screen(rows.iter().map(|row| (*row).into()).collect());
    let pre = screen(&["a", "b", "c", "d", "footer"]);
    assert!(class(&pre, &screen(&["b", "c", "x", "y", "footer"])).starts_with("rows_changed"));
    assert!(
        class(&pre, &screen(&["b", "c", "d", "y", "footer"])).starts_with("shifted(1) matches=3")
    );
    let pre = screen(&["a", "b", "c", "d", "fixed1", "fixed2", "fixed3"]);
    assert!(class(
        &pre,
        &screen(&["b", "c", "d", "x", "fixed1", "fixed2", "fixed3"])
    )
    .starts_with("rows_changed"));
}

#[test]
fn top_prefix_row_sets_and_character_split_reject_unrelated_rows() {
    let screen = |rows: &[&str]| Screen(rows.iter().map(|row| (*row).into()).collect());
    let pre = screen(&["alpha", "bravo", "charlie", "echo", "footer"]);
    let post = screen(&["bravo", "charlie", "delta", "foxtrot", "footer"]);
    let row_end = composition(
        &pre,
        &post,
        &screen(&["bravo", "charlie", "charlie", "echo", "footer"]),
    );
    assert_eq!(row_end, "post_only=2 pre_only=2 both=1 neither=0 top_prefix=true partial_row=none partial_chars=none");
    let invalid = composition(
        &pre,
        &post,
        &screen(&["bravo", "charlie", "zzzrlie", "echo", "footer"]),
    );
    assert!(invalid.contains("top_prefix=false partial_row=none"));
    let reversed = composition(
        &pre,
        &post,
        &screen(&["alpha", "charlie", "delrlie", "echo", "footer"]),
    );
    assert!(reversed.contains("top_prefix=false"));
    assert_eq!(
        partial_overwrite_chars("古い行末", "新しい行", "新し行末"),
        Some(2)
    );
    assert_eq!(partial_overwrite_chars("", "新しい行", "新し"), Some(2));
}

#[test]
fn envelope_start_is_exact_and_start_end_draws_are_excluded() {
    let chunks: &[&[u8]] = &[b"seed", b"\x1b[?25", b"lpaint", b"\x1b[?25h"];
    let mut records = vec![
        serde_json::json!({"sequence":0,"elapsed_us":0,"event":"metadata","version":1,"rows":3,"cols":20}),
    ];
    let mut offset = 0;
    for (index, bytes) in chunks.iter().enumerate() {
        let end = offset + bytes.len();
        records.push(serde_json::json!({"sequence":records.len(),"elapsed_us":index*1000,"event":"read","bin_offset":offset,"read_len":bytes.len()}));
        records.push(serde_json::json!({"sequence":records.len(),"elapsed_us":index*1000,"event":"parser_apply","bin_offset":offset,"byte_len":bytes.len(),"applied_offset":end}));
        records.push(serde_json::json!({"sequence":records.len(),"elapsed_us":index*1000,"event":"app_draw","drawn":true,"applied_offset":end,"scrollback":0}));
        offset = end;
    }
    let report = replay(&chunks.concat(), &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(report.contains("envelope=[4,21) complete=true first_read=2 last_read=4 reads=3 spans_multiple_reads=true duration_us=2000 draws=2"), "{report}");
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

#[test]
fn erase_table_links_sixty_ms_gap_to_multi_read_rewrite_and_flags_resize() {
    let erase = b"\x1b[2J\x1b[H";
    let first = b"\x1b[?25l\x1b[Hone\r\n";
    let second = b"two\x1b[?25h";
    let first_end = erase.len() + first.len();
    let end = first_end + second.len();
    let mut records = Vec::new();
    let mut add = |time: u64, mut record: Value| {
        record["sequence"] = records.len().into();
        record["elapsed_us"] = time.into();
        records.push(record);
    };
    add(
        0,
        serde_json::json!({"event":"metadata","version":1,"rows":4,"cols":20}),
    );
    add(
        0,
        serde_json::json!({"event":"transition","action":"open","kind":"erase_hold","marker":"erase_display","bin_offset":0}),
    );
    add(
        0,
        serde_json::json!({"event":"read","bin_offset":0,"read_len":erase.len()}),
    );
    add(
        40_000,
        serde_json::json!({"event":"transition","action":"close","kind":"erase_hold","bin_offset":erase.len(),"reason":"tick_release"}),
    );
    add(
        40_000,
        serde_json::json!({"event":"app_tick_release","released_len":erase.len()}),
    );
    add(
        40_000,
        serde_json::json!({"event":"parser_apply","bin_offset":0,"byte_len":erase.len(),"applied_offset":erase.len()}),
    );
    add(
        41_000,
        serde_json::json!({"event":"app_draw","drawn":true,"applied_offset":erase.len(),"scrollback":0}),
    );
    add(
        60_000,
        serde_json::json!({"event":"read","bin_offset":erase.len(),"read_len":first.len()}),
    );
    add(
        60_000,
        serde_json::json!({"event":"parser_apply","bin_offset":erase.len(),"byte_len":first.len(),"applied_offset":first_end}),
    );
    add(
        61_000,
        serde_json::json!({"event":"app_draw","drawn":true,"applied_offset":first_end,"scrollback":0}),
    );
    add(
        65_000,
        serde_json::json!({"event":"read","bin_offset":first_end,"read_len":second.len()}),
    );
    add(
        65_000,
        serde_json::json!({"event":"parser_apply","bin_offset":first_end,"byte_len":second.len(),"applied_offset":end}),
    );
    add(
        66_000,
        serde_json::json!({"event":"app_draw","drawn":true,"applied_offset":end,"scrollback":0}),
    );
    add(
        70_000,
        serde_json::json!({"event":"resize","rows":4,"cols":21,"clear":true,"applied_offset":end}),
    );
    let binary = [erase.as_slice(), first, second].concat();
    let report = replay(&binary, &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(report.contains("burst=1 kind=ERASE reads=1"), "{report}");
    assert!(report.contains("burst=2 kind=other reads=2"));
    let row = report
        .lines()
        .find(|line| line.starts_with("erase_rewrite kind=PTY_ERASE"))
        .unwrap();
    assert!(row.contains("first_rewrite_delay_us=60000 hold_close_reason=tick_release closed_before_rewrite=true"), "{row}");
    assert!(
        row.contains("rewrite_bytes=14 rewrite_reads=2 draws_inside_rewrite=1"),
        "{row}"
    );
    assert!(report.contains("erase_rewrite kind=RESIZE_CLEAR seq=13 elapsed_us=70000"));
    // A reader-side expiry on the arriving rewrite read is not an earlier
    // release, even though its transition is recorded before that read line.
    records[3]["elapsed_us"] = 60_000.into();
    let report = replay(&binary, &records, DEFAULT_BURST_GAP_US).unwrap();
    assert!(report.contains("hold_close_reason=tick_release closed_before_rewrite=false"));
}
