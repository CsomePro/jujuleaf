use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const LEGACY_OT: &str = "sharejs-text-ot";
pub const HISTORY_OT: &str = "history-ot";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRange {
    pub pos: usize,
    pub length: usize,
}

#[derive(Debug, Clone)]
pub struct DocumentState {
    pub ot_type: String,
    pub content: String,
    pub source_content: String,
    pub tracked_delete_ranges: Vec<DeleteRange>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputChange {
    pub from: usize,
    #[serde(default)]
    pub to: Option<usize>,
    #[serde(default)]
    pub insert: Option<String>,
    #[serde(default)]
    pub expect: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub from: usize,
    pub to: usize,
    pub insert: String,
    pub removed: String,
}

#[derive(Debug, Clone, Default)]
pub struct TextSelector {
    pub position: Option<usize>,
    pub occurrence: Option<usize>,
    pub all: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    pub tracked: bool,
    pub user_id: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BuiltOperations {
    pub ops: Vec<Value>,
    pub expected_content: String,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMatch {
    pub occurrence: usize,
    pub position: usize,
    pub line: usize,
    pub column: usize,
}

pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn byte_index_at_utf16(text: &str, target: usize) -> Result<usize> {
    let mut units = 0;
    for (byte_index, ch) in text.char_indices() {
        if units == target {
            return Ok(byte_index);
        }
        ensure!(
            !(target == units + 1 && ch.len_utf16() == 2),
            "UTF-16 position {target} splits a surrogate pair"
        );
        units += ch.len_utf16();
    }
    if units == target {
        Ok(text.len())
    } else {
        bail!("UTF-16 position {target} is outside document range 0..{units}")
    }
}

pub fn slice_utf16(text: &str, from: usize, to: usize) -> Result<&str> {
    ensure!(to >= from, "invalid UTF-16 range {from}..{to}");
    let start = byte_index_at_utf16(text, from)?;
    let end = byte_index_at_utf16(text, to)?;
    Ok(&text[start..end])
}

fn replace_utf16(text: &str, from: usize, to: usize, insert: &str) -> Result<String> {
    let start = byte_index_at_utf16(text, from)?;
    let end = byte_index_at_utf16(text, to)?;
    let mut result = String::with_capacity(text.len() - (end - start) + insert.len());
    result.push_str(&text[..start]);
    result.push_str(insert);
    result.push_str(&text[end..]);
    Ok(result)
}

fn latin1_bytes(value: &str) -> Result<Vec<u8>> {
    value
        .chars()
        .map(|ch| {
            u8::try_from(ch as u32)
                .map_err(|_| anyhow!("invalid Latin-1 snapshot character U+{:04X}", ch as u32))
        })
        .collect()
}

fn tracked_delete_ranges(raw: &Value) -> Vec<DeleteRange> {
    let mut ranges: Vec<_> = raw
        .get("trackedChanges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|change| change.pointer("/tracking/type").and_then(Value::as_str) == Some("delete"))
        .filter_map(|change| {
            let pos = change.pointer("/range/pos")?.as_u64()? as usize;
            let length = change.pointer("/range/length")?.as_u64()? as usize;
            (length > 0).then_some(DeleteRange { pos, length })
        })
        .collect();
    ranges.sort_by_key(|range| range.pos);
    ranges
}

fn visible_history_content(source: &str, ranges: &[DeleteRange]) -> Result<String> {
    let mut content = String::new();
    let mut cursor = 0;
    let source_len = utf16_len(source);
    for range in ranges {
        if range.pos < cursor || range.pos > source_len {
            continue;
        }
        content.push_str(slice_utf16(source, cursor, range.pos)?);
        cursor = source_len.min(range.pos.saturating_add(range.length));
    }
    content.push_str(slice_utf16(source, cursor, source_len)?);
    Ok(content)
}

pub fn parse_document_snapshot(snapshot: Value, ot_type: &str) -> Result<DocumentState> {
    match ot_type {
        HISTORY_OT => {
            let source = snapshot
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("invalid history-ot document snapshot"))?
                .to_owned();
            let ranges = tracked_delete_ranges(&snapshot);
            let content = visible_history_content(&source, &ranges)?;
            Ok(DocumentState {
                ot_type: ot_type.to_owned(),
                content,
                source_content: source,
                tracked_delete_ranges: ranges,
                raw: snapshot,
            })
        }
        LEGACY_OT => {
            let lines = snapshot
                .as_array()
                .ok_or_else(|| anyhow!("invalid sharejs-text-ot document snapshot"))?;
            let content = lines
                .iter()
                .map(|line| {
                    let line = line
                        .as_str()
                        .ok_or_else(|| anyhow!("invalid legacy document line"))?;
                    String::from_utf8(latin1_bytes(line)?).map_err(Into::into)
                })
                .collect::<Result<Vec<_>>>()?
                .join("\n");
            Ok(DocumentState {
                ot_type: ot_type.to_owned(),
                source_content: content.clone(),
                content,
                tracked_delete_ranges: vec![],
                raw: snapshot,
            })
        }
        other => bail!("unsupported OT type: {other}"),
    }
}

pub fn visible_to_source_position(position: usize, state: &DocumentState) -> Result<usize> {
    let visible_len = utf16_len(&state.content);
    ensure!(
        position <= visible_len,
        "position {position} is outside document range 0..{visible_len}"
    );
    if state.ot_type != HISTORY_OT {
        return Ok(position);
    }
    let mut mapped = position;
    let mut hidden_before = 0;
    for range in &state.tracked_delete_ranges {
        let visible_boundary = range.pos.saturating_sub(hidden_before);
        if position <= visible_boundary {
            break;
        }
        mapped += range.length;
        hidden_before += range.length;
    }
    Ok(mapped)
}

pub fn normalize_changes(inputs: &[InputChange], content: &str) -> Result<Vec<Change>> {
    let content_len = utf16_len(content);
    let mut previous_to = 0;
    let mut changes = Vec::with_capacity(inputs.len());
    for (index, input) in inputs.iter().enumerate() {
        let to = input.to.unwrap_or(input.from);
        ensure!(
            input.from <= to && to <= content_len,
            "changes[{index}] range {}..{to} is outside document range 0..{content_len}",
            input.from
        );
        ensure!(
            index == 0 || input.from >= previous_to,
            "changes must be ordered by position and must not overlap"
        );
        let removed = slice_utf16(content, input.from, to)?.to_owned();
        if let Some(expect) = &input.expect {
            ensure!(
                expect == &removed,
                "changes[{index}].expect does not match text at {}..{to}",
                input.from
            );
        }
        changes.push(Change {
            from: input.from,
            to,
            insert: input.insert.clone().unwrap_or_default(),
            removed,
        });
        previous_to = to;
    }
    Ok(changes)
}

pub fn apply_changes(content: &str, changes: &[Change]) -> Result<String> {
    let mut result = content.to_owned();
    let mut shift: isize = 0;
    for change in changes {
        let position = change
            .from
            .checked_add_signed(shift)
            .ok_or_else(|| anyhow!("change position overflow"))?;
        let remove_length = change.to - change.from;
        result = replace_utf16(&result, position, position + remove_length, &change.insert)?;
        shift += utf16_len(&change.insert) as isize - remove_length as isize;
    }
    Ok(result)
}

fn find_occurrences(content: &str, needle: &str) -> Vec<usize> {
    let haystack: Vec<u16> = content.encode_utf16().collect();
    let needle: Vec<u16> = needle.encode_utf16().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return vec![];
    }
    (0..=haystack.len() - needle.len())
        .filter(|&at| haystack[at..at + needle.len()] == needle)
        .collect()
}

pub fn changes_for_text(
    content: &str,
    old: &str,
    new: &str,
    selector: &TextSelector,
) -> Result<Vec<Change>> {
    ensure!(
        !old.is_empty(),
        "old text cannot be empty; use insert with --position"
    );
    let selector_count = selector.position.is_some() as u8
        + selector.occurrence.is_some() as u8
        + selector.all as u8;
    ensure!(
        selector_count <= 1,
        "--position, --occurrence, and --all are mutually exclusive"
    );
    let old_len = utf16_len(old);

    let positions = if let Some(position) = selector.position {
        let content_len = utf16_len(content);
        ensure!(
            position + old_len <= content_len,
            "position {position} is outside document range 0..{content_len}"
        );
        ensure!(
            slice_utf16(content, position, position + old_len)? == old,
            "old text does not match at position {position}"
        );
        vec![position]
    } else {
        let positions = find_occurrences(content, old);
        if let Some(occurrence) = selector.occurrence {
            ensure!(occurrence >= 1, "occurrence must be at least 1");
            vec![
                *positions
                    .get(occurrence - 1)
                    .ok_or_else(|| anyhow!("old text occurrence {occurrence} was not found"))?,
            ]
        } else if selector.all {
            let mut non_overlapping = Vec::new();
            let mut next = 0;
            for position in positions {
                if position >= next {
                    non_overlapping.push(position);
                    next = position + old_len;
                }
            }
            ensure!(!non_overlapping.is_empty(), "old text was not found");
            non_overlapping
        } else {
            ensure!(!positions.is_empty(), "old text was not found");
            ensure!(
                positions.len() == 1,
                "old text is not unique; use more context, --position, --occurrence, or --all"
            );
            positions
        }
    };

    Ok(positions
        .into_iter()
        .map(|from| Change {
            from,
            to: from + old_len,
            insert: new.to_owned(),
            removed: old.to_owned(),
        })
        .collect())
}

pub fn locate_text(content: &str, text: &str) -> Result<Vec<TextMatch>> {
    ensure!(!text.is_empty(), "text must be a non-empty string");
    find_occurrences(content, text)
        .into_iter()
        .enumerate()
        .map(|(index, position)| {
            let before = slice_utf16(content, 0, position)?;
            let line_start = before
                .encode_utf16()
                .enumerate()
                .filter_map(|(at, unit)| (unit == b'\n' as u16).then_some(at))
                .last()
                .map_or(0, |at| at + 1);
            Ok(TextMatch {
                occurrence: index + 1,
                position,
                line: before.chars().filter(|&ch| ch == '\n').count() + 1,
                column: position - line_start + 1,
            })
        })
        .collect()
}

fn effective_changes(changes: &[Change]) -> impl Iterator<Item = &Change> {
    changes
        .iter()
        .filter(|change| change.removed != change.insert)
}

fn build_legacy_operations(content: &str, changes: &[Change]) -> Result<(Vec<Value>, String)> {
    let mut ops = Vec::new();
    let mut working = content.to_owned();
    let mut shift: isize = 0;
    for change in effective_changes(changes) {
        let position = change
            .from
            .checked_add_signed(shift)
            .ok_or_else(|| anyhow!("change position overflow"))?;
        let remove_length = change.to - change.from;
        let deleted = slice_utf16(&working, position, position + remove_length)?;
        ensure!(
            deleted == change.removed,
            "change no longer matches at position {position}"
        );
        if !deleted.is_empty() {
            ops.push(json!({"d": deleted, "p": position}));
        }
        if !change.insert.is_empty() {
            ops.push(json!({"i": change.insert, "p": position}));
        }
        working = replace_utf16(&working, position, position + remove_length, &change.insert)?;
        shift += utf16_len(&change.insert) as isize - remove_length as isize;
    }
    Ok((ops, working))
}

fn append_scan(scan: &mut Vec<Value>, value: Value) {
    if value.as_i64() == Some(0) || value.as_str() == Some("") {
        return;
    }
    if let (Some(last), Some(current)) = (scan.last().and_then(Value::as_i64), value.as_i64())
        && last.signum() == current.signum()
    {
        *scan.last_mut().expect("last exists") = json!(last + current);
        return;
    }
    if let (Some(last), Some(current)) = (scan.last().and_then(Value::as_str), value.as_str()) {
        let mut joined = last.to_owned();
        joined.push_str(current);
        *scan.last_mut().expect("last exists") = Value::String(joined);
        return;
    }
    scan.push(value);
}

fn build_history_operations(
    state: &DocumentState,
    changes: &[Change],
    options: &BuildOptions,
) -> Result<(Vec<Value>, String)> {
    let active: Vec<_> = effective_changes(changes).collect();
    if active.is_empty() {
        return Ok((vec![], state.content.clone()));
    }
    ensure!(
        active
            .iter()
            .all(|change| change.insert.chars().all(|ch| ch.len_utf16() == 1)),
        "history-ot does not support inserting non-BMP characters"
    );
    if options.tracked {
        ensure!(
            options.user_id.as_ref().is_some_and(|id| !id.is_empty()),
            "tracked history-ot edits require a user ID"
        );
    }
    let timestamp = options
        .timestamp
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let mut scan = Vec::new();
    let mut source_cursor = 0;
    for change in active {
        let from = visible_to_source_position(change.from, state)?;
        let to = visible_to_source_position(change.to, state)?;
        ensure!(
            from >= source_cursor,
            "changes overlap after mapping tracked deletions"
        );
        append_scan(&mut scan, json!((from - source_cursor) as i64));
        if !change.insert.is_empty() {
            if options.tracked {
                scan.push(json!({
                    "i": change.insert,
                    "tracking": {
                        "type": "insert",
                        "userId": options.user_id,
                        "ts": timestamp
                    }
                }));
            } else {
                append_scan(&mut scan, json!(change.insert));
            }
        }
        if to > from {
            if options.tracked {
                scan.push(json!({
                    "r": to - from,
                    "tracking": {
                        "type": "delete",
                        "userId": options.user_id,
                        "ts": timestamp
                    }
                }));
            } else {
                append_scan(&mut scan, json!(-((to - from) as i64)));
            }
        }
        source_cursor = to;
    }
    append_scan(
        &mut scan,
        json!((utf16_len(&state.source_content) - source_cursor) as i64),
    );
    Ok((
        vec![json!({"textOperation": scan})],
        apply_changes(&state.content, changes)?,
    ))
}

pub fn build_document_operations(
    state: &DocumentState,
    input: &[InputChange],
    options: &BuildOptions,
) -> Result<BuiltOperations> {
    let changes = normalize_changes(input, &state.content)?;
    let (ops, expected_content) = if state.ot_type == HISTORY_OT {
        build_history_operations(state, &changes, options)?
    } else if state.ot_type == LEGACY_OT {
        build_legacy_operations(&state.content, &changes)?
    } else {
        bail!("unsupported OT type: {}", state.ot_type)
    };
    Ok(BuiltOperations {
        ops,
        expected_content,
        changes,
    })
}

pub fn build_document_operations_from_changes(
    state: &DocumentState,
    changes: Vec<Change>,
    options: &BuildOptions,
) -> Result<BuiltOperations> {
    let inputs = changes
        .iter()
        .map(|change| InputChange {
            from: change.from,
            to: Some(change.to),
            insert: Some(change.insert.clone()),
            expect: Some(change.removed.clone()),
        })
        .collect::<Vec<_>>();
    build_document_operations(state, &inputs, options)
}

pub fn apply_legacy_operations(content: &str, ops: &[Value]) -> Result<String> {
    let mut result = content.to_owned();
    for (index, op) in ops.iter().enumerate() {
        let object = op
            .as_object()
            .ok_or_else(|| anyhow!("ops[{index}] must be an object"))?;
        let position = object
            .get("p")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("ops[{index}].p must be a non-negative integer"))?
            as usize;
        let kinds: Vec<_> = ["i", "d", "c"]
            .into_iter()
            .filter(|key| object.contains_key(*key))
            .collect();
        ensure!(
            kinds.len() == 1,
            "ops[{index}] must contain exactly one of i, d, or c"
        );
        let kind = kinds[0];
        let value = object[kind]
            .as_str()
            .ok_or_else(|| anyhow!("ops[{index}].{kind} must be a string"))?;
        match kind {
            "i" => result = replace_utf16(&result, position, position, value)?,
            "d" => {
                let end = position + utf16_len(value);
                ensure!(
                    slice_utf16(&result, position, end)? == value,
                    "ops[{index}] delete text does not match at position {position}"
                );
                result = replace_utf16(&result, position, end, "")?;
            }
            "c" => {
                let end = position + utf16_len(value);
                ensure!(
                    slice_utf16(&result, position, end)? == value,
                    "ops[{index}] comment text does not match at position {position}"
                );
            }
            _ => unreachable!(),
        }
    }
    Ok(result)
}

fn validate_tracking(value: &Value, name: &str) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{name}.tracking must be an object"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{name}.tracking.type is invalid"))?;
    ensure!(
        ["insert", "delete", "none"].contains(&kind),
        "{name}.tracking.type is invalid"
    );
    if kind != "none" {
        ensure!(
            object
                .get("userId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty()),
            "{name}.tracking.userId must be a non-empty string"
        );
        ensure!(
            object
                .get("ts")
                .and_then(Value::as_str)
                .is_some_and(|ts| !ts.is_empty()),
            "{name}.tracking.ts must be an ISO date string"
        );
    }
    Ok(())
}

fn validate_history_text_operation(raw: &Value, source_length: usize, name: &str) -> Result<usize> {
    let scans = raw
        .get("textOperation")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("{name}.textOperation must be an array"))?;
    let mut consumed = 0;
    let mut target_length = 0;
    for (index, scan) in scans.iter().enumerate() {
        let scan_name = format!("{name}.textOperation[{index}]");
        if let Some(number) = scan.as_i64() {
            ensure!(number != 0, "{scan_name} must be a non-zero integer");
            if number > 0 {
                consumed += number as usize;
                target_length += number as usize;
            } else {
                consumed += (-number) as usize;
            }
        } else if let Some(insert) = scan.as_str() {
            ensure!(
                insert.chars().all(|ch| ch.len_utf16() == 1),
                "{scan_name} contains non-BMP characters"
            );
            target_length += utf16_len(insert);
        } else if let Some(object) = scan.as_object() {
            if let Some(insert) = object.get("i") {
                let insert = insert
                    .as_str()
                    .ok_or_else(|| anyhow!("{scan_name}.i must be a string"))?;
                ensure!(
                    insert.chars().all(|ch| ch.len_utf16() == 1),
                    "{scan_name}.i contains non-BMP characters"
                );
                if let Some(tracking) = object.get("tracking") {
                    validate_tracking(tracking, &scan_name)?;
                }
                if let Some(ids) = object.get("commentIds") {
                    ensure!(
                        ids.as_array()
                            .is_some_and(|ids| ids.iter().all(Value::is_string)),
                        "{scan_name}.commentIds must be a string array"
                    );
                }
                target_length += utf16_len(insert);
            } else if let Some(remove) = object.get("r") {
                let remove = remove
                    .as_u64()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| anyhow!("{scan_name}.r must be a positive integer"))?
                    as usize;
                if let Some(tracking) = object.get("tracking") {
                    validate_tracking(tracking, &scan_name)?;
                }
                consumed += remove;
                target_length += remove;
            } else {
                bail!("{scan_name} must contain i or r");
            }
        } else {
            bail!("{scan_name} is not a valid scan operation");
        }
        ensure!(
            consumed <= source_length,
            "{name} consumes more than {source_length} characters"
        );
    }
    ensure!(
        consumed == source_length,
        "{name} must consume the whole source ({consumed} != {source_length})"
    );
    Ok(target_length)
}

pub fn validate_history_operations(state: &DocumentState, ops: &[Value]) -> Result<usize> {
    ensure!(
        state.ot_type == HISTORY_OT,
        "state must be a history-ot snapshot"
    );
    let mut source_length = utf16_len(&state.source_content);
    for (index, op) in ops.iter().enumerate() {
        let name = format!("ops[{index}]");
        let object = op
            .as_object()
            .ok_or_else(|| anyhow!("{name} must be an object"))?;
        if object.contains_key("textOperation") {
            source_length = validate_history_text_operation(op, source_length, &name)?;
        } else if object.contains_key("commentId") && object.contains_key("ranges") {
            ensure!(
                object
                    .get("commentId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty()),
                "{name}.commentId must be a non-empty string"
            );
            let ranges = object
                .get("ranges")
                .and_then(Value::as_array)
                .filter(|ranges| !ranges.is_empty())
                .ok_or_else(|| anyhow!("{name}.ranges must be a non-empty array"))?;
            for (range_index, range) in ranges.iter().enumerate() {
                let pos = range.get("pos").and_then(Value::as_u64).map(|n| n as usize);
                let length = range
                    .get("length")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                ensure!(
                    matches!((pos, length), (Some(pos), Some(length)) if length > 0 && pos + length <= source_length),
                    "{name}.ranges[{range_index}] is outside source range 0..{source_length}"
                );
            }
        } else if object.contains_key("deleteComment") {
            ensure!(
                object
                    .get("deleteComment")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty()),
                "{name}.deleteComment must be a non-empty string"
            );
        } else if object.contains_key("commentId") && object.contains_key("resolved") {
            ensure!(
                object.get("commentId").and_then(Value::as_str).is_some()
                    && object.get("resolved").and_then(Value::as_bool).is_some(),
                "{name} has an invalid comment state operation"
            );
        } else {
            ensure!(
                object.contains_key("noOp"),
                "{name} is not a supported history-ot operation"
            );
        }
    }
    Ok(source_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn legacy(content: &str) -> DocumentState {
        DocumentState {
            ot_type: LEGACY_OT.to_owned(),
            content: content.to_owned(),
            source_content: content.to_owned(),
            tracked_delete_ranges: vec![],
            raw: json!([]),
        }
    }

    #[test]
    fn legacy_snapshot_decodes_utf8_hidden_in_latin1() {
        let encoded: String = "中文".as_bytes().iter().copied().map(char::from).collect();
        let state = parse_document_snapshot(json!(["hello", encoded]), LEGACY_OT).unwrap();
        assert_eq!(state.content, "hello\n中文");
    }

    #[test]
    fn exact_text_is_unique_by_default_and_uses_utf16() {
        assert!(
            changes_for_text("cat dog cat", "cat", "fox", &TextSelector::default())
                .unwrap_err()
                .to_string()
                .contains("not unique")
        );
        let changes = changes_for_text(
            "😀 cat cat",
            "cat",
            "fox",
            &TextSelector {
                occurrence: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(changes[0].from, 7);
        assert_eq!(changes[0].to, 10);
    }

    #[test]
    fn locate_overlaps_and_matches_codemirror_utf16_offsets() {
        assert_eq!(
            locate_text("😀\ncat cat", "cat").unwrap(),
            vec![
                TextMatch {
                    occurrence: 1,
                    position: 3,
                    line: 2,
                    column: 1
                },
                TextMatch {
                    occurrence: 2,
                    position: 7,
                    line: 2,
                    column: 5
                },
            ]
        );
        assert_eq!(locate_text("aaa", "aa").unwrap().len(), 2);
    }

    #[test]
    fn batch_legacy_operations_preserve_transaction_coordinates() {
        let state = legacy("one cat two cat");
        let changes = changes_for_text(
            &state.content,
            "cat",
            "kitten",
            &TextSelector {
                all: true,
                ..Default::default()
            },
        )
        .unwrap();
        let built =
            build_document_operations_from_changes(&state, changes, &BuildOptions::default())
                .unwrap();
        assert_eq!(
            built.ops,
            vec![
                json!({"d":"cat","p":4}),
                json!({"i":"kitten","p":4}),
                json!({"d":"cat","p":15}),
                json!({"i":"kitten","p":15}),
            ]
        );
        assert_eq!(
            apply_legacy_operations(&state.content, &built.ops).unwrap(),
            "one kitten two kitten"
        );
    }

    #[test]
    fn history_hides_tracked_deletes_and_maps_boundaries() {
        let state = parse_document_snapshot(
            json!({
                "content":"abcXXdef",
                "trackedChanges":[{"range":{"pos":3,"length":2},"tracking":{"type":"delete"}}]
            }),
            HISTORY_OT,
        )
        .unwrap();
        assert_eq!(state.content, "abcdef");
        assert_eq!(visible_to_source_position(3, &state).unwrap(), 3);
        assert_eq!(visible_to_source_position(4, &state).unwrap(), 6);
        let built = build_document_operations(
            &state,
            &[InputChange {
                from: 2,
                to: Some(4),
                insert: Some("Q".into()),
                expect: Some("cd".into()),
            }],
            &BuildOptions::default(),
        )
        .unwrap();
        assert_eq!(built.ops, vec![json!({"textOperation":[2,"Q",-4,2]})]);
        assert_eq!(built.expected_content, "abQef");
    }

    #[test]
    fn history_emits_tracking_and_rejects_non_bmp() {
        let state = parse_document_snapshot(json!({"content":"abcd"}), HISTORY_OT).unwrap();
        let built = build_document_operations(
            &state,
            &[InputChange {
                from: 1,
                to: Some(3),
                insert: Some("X".into()),
                expect: None,
            }],
            &BuildOptions {
                tracked: true,
                user_id: Some("user-1".into()),
                timestamp: Some("2026-01-02T03:04:05.000Z".into()),
            },
        )
        .unwrap();
        assert_eq!(
            built.ops,
            vec![json!({"textOperation":[
                1,
                {"i":"X","tracking":{"type":"insert","userId":"user-1","ts":"2026-01-02T03:04:05.000Z"}},
                {"r":2,"tracking":{"type":"delete","userId":"user-1","ts":"2026-01-02T03:04:05.000Z"}},
                1
            ]})]
        );
        let error = build_document_operations(
            &state,
            &[InputChange {
                from: 1,
                to: Some(1),
                insert: Some("😀".into()),
                expect: None,
            }],
            &BuildOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-BMP"));
    }

    #[test]
    fn low_level_validation_is_strict() {
        assert!(apply_legacy_operations("abc", &[json!({"p":0,"i":"x","d":"a"})]).is_err());
        let state = parse_document_snapshot(json!({"content":"abcd"}), HISTORY_OT).unwrap();
        assert_eq!(
            validate_history_operations(&state, &[json!({"textOperation":[1,"X",-2,1]})]).unwrap(),
            3
        );
        assert!(validate_history_operations(&state, &[json!({"textOperation":[1,"X"]})]).is_err());
        assert!(validate_history_operations(&state, &[json!({"textOperation":[4,"😀"]})]).is_err());
    }
}
