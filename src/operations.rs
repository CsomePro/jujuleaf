use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use crate::latex_blocks::{LatexBlock, scan_latex_blocks};

pub const LEGACY_OT: &str = "sharejs-text-ot";
pub const HISTORY_OT: &str = "history-ot";
pub const LEGACY_REVIEW_DIFF_ALGORITHM: &str = "semantic-v1";
pub const REVIEW_DIFF_ALGORITHM: &str = "semantic-v2";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ReviewGranularity {
    #[default]
    Exact,
    Adaptive,
    Word,
    Sentence,
}

impl std::fmt::Display for ReviewGranularity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Exact => "exact",
            Self::Adaptive => "adaptive",
            Self::Word => "word",
            Self::Sentence => "sentence",
        };
        formatter.write_str(value)
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTextChange {
    pub change: InputChange,
    pub atomic_change_count: usize,
    pub removed: usize,
    pub inserted: usize,
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

/// Build ordered, non-overlapping CodeMirror changes from two complete texts.
///
/// The diff runs on Unicode scalar values, then converts every position to the
/// UTF-16 coordinate system used by CodeMirror and Overleaf. Unchanged spans
/// between independent edits remain untouched, which is important for remote
/// comment and tracked-change anchors.
pub fn minimal_text_changes(before: &str, after: &str) -> Vec<InputChange> {
    if before == after {
        return Vec::new();
    }

    let diff = TextDiff::from_chars(before, after);
    let mut changes = Vec::new();
    let mut old_position = 0;
    let mut pending_from = None;
    let mut removed = String::new();
    let mut inserted = String::new();

    let flush = |changes: &mut Vec<InputChange>,
                 pending_from: &mut Option<usize>,
                 old_position: usize,
                 removed: &mut String,
                 inserted: &mut String| {
        let Some(from) = pending_from.take() else {
            return;
        };
        changes.push(InputChange {
            from,
            to: Some(old_position),
            insert: Some(std::mem::take(inserted)),
            expect: Some(std::mem::take(removed)),
        });
    };

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                flush(
                    &mut changes,
                    &mut pending_from,
                    old_position,
                    &mut removed,
                    &mut inserted,
                );
                old_position += utf16_len(change.value());
            }
            ChangeTag::Delete => {
                pending_from.get_or_insert(old_position);
                removed.push_str(change.value());
                old_position += utf16_len(change.value());
            }
            ChangeTag::Insert => {
                pending_from.get_or_insert(old_position);
                inserted.push_str(change.value());
            }
        }
    }
    flush(
        &mut changes,
        &mut pending_from,
        old_position,
        &mut removed,
        &mut inserted,
    );
    changes
}

fn change_end(change: &InputChange) -> usize {
    change.to.unwrap_or(change.from)
}

fn contains_review_barrier(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character,
            '\\' | '{'
                | '}'
                | '$'
                | '%'
                | '&'
                | '#'
                | '^'
                | '~'
                | '\n'
                | '\r'
                | '.'
                | '?'
                | '!'
                | ';'
                | '。'
                | '？'
                | '！'
                | '；'
        )
    })
}

fn position_is_in_tex_command(before: &str, position: usize) -> Result<bool> {
    let prefix = slice_utf16(before, 0, position)?;
    let mut characters = prefix.chars().rev();
    for character in characters.by_ref() {
        if !(character.is_alphabetic() || character == '@') {
            return Ok(character == '\\');
        }
    }
    Ok(false)
}

fn protected_review_atom(before: &str, change: &InputChange) -> Result<bool> {
    let removed = change.expect.as_deref().unwrap_or_default();
    let inserted = change.insert.as_deref().unwrap_or_default();
    Ok(contains_review_barrier(removed)
        || contains_review_barrier(inserted)
        || position_is_in_tex_command(before, change.from)?)
}

fn word_internal_gap(gap: &str) -> bool {
    !gap.is_empty()
        && gap.chars().all(|character| {
            (character.is_alphanumeric() && !uses_dictionary_word_boundaries(character))
                || matches!(character, '_' | '-' | '\'' | '’')
        })
}

fn uses_dictionary_word_boundaries(character: char) -> bool {
    matches!(
        character as u32,
        0x1100..=0x11ff
            | 0x3040..=0x30ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xac00..=0xd7af
            | 0xf900..=0xfaff
    )
}

fn planned_atom(change: InputChange) -> PlannedTextChange {
    PlannedTextChange {
        removed: utf16_len(change.expect.as_deref().unwrap_or_default()),
        inserted: utf16_len(change.insert.as_deref().unwrap_or_default()),
        change,
        atomic_change_count: 1,
    }
}

fn merge_review_cluster(before: &str, atoms: &[PlannedTextChange]) -> Result<PlannedTextChange> {
    ensure!(!atoms.is_empty(), "cannot merge an empty review cluster");
    if atoms.len() == 1 {
        return Ok(atoms[0].clone());
    }

    let from = atoms[0].change.from;
    let to = change_end(&atoms[atoms.len() - 1].change);
    let mut insert = String::new();
    let mut previous_end = from;
    for atom in atoms {
        ensure!(
            atom.change.from >= previous_end,
            "review changes overlap while grouping"
        );
        insert.push_str(slice_utf16(before, previous_end, atom.change.from)?);
        insert.push_str(atom.change.insert.as_deref().unwrap_or_default());
        previous_end = change_end(&atom.change);
    }

    Ok(PlannedTextChange {
        change: InputChange {
            from,
            to: Some(to),
            insert: Some(insert),
            expect: Some(slice_utf16(before, from, to)?.to_owned()),
        },
        atomic_change_count: atoms.iter().map(|atom| atom.atomic_change_count).sum(),
        removed: atoms.iter().map(|atom| atom.removed).sum(),
        inserted: atoms.iter().map(|atom| atom.inserted).sum(),
    })
}

fn safe_review_segments<'a>(
    before: &str,
    atoms: &'a [PlannedTextChange],
) -> Result<Vec<&'a [PlannedTextChange]>> {
    if atoms.is_empty() {
        return Ok(Vec::new());
    }

    let mut segments = Vec::new();
    let mut start = 0;
    for index in 0..atoms.len() {
        let atom_is_protected = protected_review_atom(before, &atoms[index].change)?;
        if atom_is_protected {
            if start < index {
                segments.push(&atoms[start..index]);
            }
            segments.push(&atoms[index..=index]);
            start = index + 1;
            continue;
        }
        if index + 1 == atoms.len() {
            continue;
        }
        let next_is_protected = protected_review_atom(before, &atoms[index + 1].change)?;
        let gap = slice_utf16(
            before,
            change_end(&atoms[index].change),
            atoms[index + 1].change.from,
        )?;
        if next_is_protected || contains_review_barrier(gap) {
            segments.push(&atoms[start..=index]);
            start = index + 1;
        }
    }
    if start < atoms.len() {
        segments.push(&atoms[start..]);
    }
    Ok(segments)
}

fn group_words(before: &str, atoms: &[PlannedTextChange]) -> Result<Vec<PlannedTextChange>> {
    if atoms.is_empty() {
        return Ok(Vec::new());
    }
    let mut grouped = Vec::new();
    let mut start = 0;
    for index in 0..atoms.len().saturating_sub(1) {
        let gap = slice_utf16(
            before,
            change_end(&atoms[index].change),
            atoms[index + 1].change.from,
        )?;
        if !word_internal_gap(gap) {
            grouped.push(merge_review_cluster(before, &atoms[start..=index])?);
            start = index + 1;
        }
    }
    grouped.push(merge_review_cluster(before, &atoms[start..])?);
    Ok(grouped)
}

fn dense_review_segment(before: &str, atoms: &[PlannedTextChange]) -> Result<bool> {
    if atoms.len() < 3 {
        return Ok(false);
    }
    let mut total_gap = 0;
    let mut max_gap = 0;
    for pair in atoms.windows(2) {
        let gap = utf16_len(slice_utf16(
            before,
            change_end(&pair[0].change),
            pair[1].change.from,
        )?);
        total_gap += gap;
        max_gap = max_gap.max(gap);
    }
    let changed: usize = atoms.iter().map(|atom| atom.removed + atom.inserted).sum();
    Ok(max_gap <= 12 && total_gap <= changed)
}

fn review_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            while characters
                .peek()
                .is_some_and(|next| next.is_alphabetic() || *next == '@')
            {
                characters.next();
            }
            continue;
        }
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            if !matches!(token.as_str(), "itemize" | "enumerate" | "description") {
                tokens.push(std::mem::take(&mut token));
            } else {
                token.clear();
            }
        }
    }
    if !token.is_empty() && !matches!(token.as_str(), "itemize" | "enumerate" | "description") {
        tokens.push(token);
    }
    tokens
}

fn lexical_partition_score(source: &str, target: &str) -> i64 {
    if source == target {
        return 1_000_000;
    }
    let source_tokens = review_tokens(source);
    let target_tokens = review_tokens(target);
    if source_tokens.is_empty() || target_tokens.is_empty() {
        return -500;
    }
    let mut counts = BTreeMap::new();
    for token in &source_tokens {
        *counts.entry(token).or_insert(0usize) += 1;
    }
    let mut shared = 0usize;
    for token in &target_tokens {
        if let Some(count) = counts.get_mut(token)
            && *count > 0
        {
            *count -= 1;
            shared += 1;
        }
    }
    let total = source_tokens.len() + target_tokens.len();
    let dice = (2_000 * shared / total) as i64;
    let length_delta = source_tokens.len().abs_diff(target_tokens.len()) as i64;
    dice + shared as i64 * 25 - length_delta * 2
}

fn block_partition_score(source: &[LatexBlock], target: &LatexBlock) -> i64 {
    if target.kind.is_structured() {
        if source.iter().any(|block| block.kind == target.kind) {
            750
        } else {
            -750
        }
    } else if source.iter().any(|block| block.kind.is_structured()) {
        -150
    } else {
        0
    }
}

fn chunk_group_text<'a>(text: &'a str, chunks: &[LatexBlock]) -> &'a str {
    let first = chunks.first().expect("review chunk group is not empty");
    let last = chunks.last().expect("review chunk group is not empty");
    &text[first.byte_from..last.byte_to]
}

/// Partition a longer sequence into one non-empty contiguous group per item in
/// a shorter sequence. Lexical overlap chooses human-meaningful boundaries;
/// every byte remains assigned exactly once.
fn best_review_partition(
    source_text: &str,
    source: &[LatexBlock],
    targets_text: &str,
    targets: &[LatexBlock],
) -> Vec<usize> {
    debug_assert!(source.len() >= targets.len());
    debug_assert!(!targets.is_empty());
    let source_len = source.len();
    let target_len = targets.len();
    let mut scores = vec![vec![i64::MIN; source_len + 1]; target_len + 1];
    let mut previous = vec![vec![0usize; source_len + 1]; target_len + 1];
    scores[0][0] = 0;

    for target_index in 1..=target_len {
        let min_end = target_index;
        let max_end = source_len - (target_len - target_index);
        for end in min_end..=max_end {
            for start in target_index - 1..end {
                let prior = scores[target_index - 1][start];
                if prior == i64::MIN {
                    continue;
                }
                let source_group = chunk_group_text(source_text, &source[start..end]);
                let target =
                    chunk_group_text(targets_text, &targets[target_index - 1..target_index]);
                let role_score =
                    block_partition_score(&source[start..end], &targets[target_index - 1]);
                let candidate = prior + lexical_partition_score(source_group, target) + role_score;
                if candidate > scores[target_index][end] {
                    scores[target_index][end] = candidate;
                    previous[target_index][end] = start;
                }
            }
        }
    }

    let mut boundaries = vec![0usize; target_len + 1];
    boundaries[target_len] = source_len;
    for target_index in (1..=target_len).rev() {
        boundaries[target_index - 1] = previous[target_index][boundaries[target_index]];
    }
    boundaries
}

fn structured_replacement(
    before: &str,
    after: &str,
    old_chunks: &[LatexBlock],
    new_chunks: &[LatexBlock],
) -> PlannedTextChange {
    let old = chunk_group_text(before, old_chunks);
    let new = chunk_group_text(after, new_chunks);
    let from = utf16_len(&before[..old_chunks[0].byte_from]);
    let to = from + utf16_len(old);
    PlannedTextChange {
        change: InputChange {
            from,
            to: Some(to),
            insert: Some(new.to_owned()),
            expect: Some(old.to_owned()),
        },
        atomic_change_count: minimal_text_changes(old, new).len().max(1),
        removed: utf16_len(old),
        inserted: utf16_len(new),
    }
}

fn structural_review_changes(
    before: &str,
    after: &str,
    atomic_change_count: usize,
) -> Option<Vec<PlannedTextChange>> {
    const MIN_STRUCTURAL_ATOMS: usize = 8;
    const MAX_STRUCTURAL_BLOCKS: usize = 64;

    if atomic_change_count < MIN_STRUCTURAL_ATOMS {
        return None;
    }

    let old_scan = scan_latex_blocks(before);
    let new_scan = scan_latex_blocks(after);
    if !old_scan.valid || !new_scan.valid {
        return None;
    }

    let mut prefix = 0;
    while prefix < old_scan.blocks.len()
        && prefix < new_scan.blocks.len()
        && chunk_group_text(before, &old_scan.blocks[prefix..=prefix])
            == chunk_group_text(after, &new_scan.blocks[prefix..=prefix])
    {
        prefix += 1;
    }
    let mut old_end = old_scan.blocks.len();
    let mut new_end = new_scan.blocks.len();
    while old_end > prefix
        && new_end > prefix
        && chunk_group_text(before, &old_scan.blocks[old_end - 1..old_end])
            == chunk_group_text(after, &new_scan.blocks[new_end - 1..new_end])
    {
        old_end -= 1;
        new_end -= 1;
    }

    let old_blocks = &old_scan.blocks[prefix..old_end];
    let new_blocks = &new_scan.blocks[prefix..new_end];
    if old_blocks.is_empty()
        || new_blocks.is_empty()
        || old_blocks.len() > MAX_STRUCTURAL_BLOCKS
        || new_blocks.len() > MAX_STRUCTURAL_BLOCKS
        || old_blocks.iter().any(|block| block.kind.is_unknown())
        || new_blocks.iter().any(|block| block.kind.is_unknown())
    {
        return None;
    }

    let structure_changed = old_scan.signature != new_scan.signature;
    let changed_structured_block = old_blocks
        .iter()
        .chain(new_blocks)
        .any(|block| block.kind.is_structured());
    if !structure_changed && !changed_structured_block {
        return None;
    }

    let mut planned = Vec::new();
    if old_blocks.len() >= new_blocks.len() {
        let boundaries = best_review_partition(before, old_blocks, after, new_blocks);
        for (index, new_block) in new_blocks.iter().enumerate() {
            let old_group = &old_blocks[boundaries[index]..boundaries[index + 1]];
            let old = chunk_group_text(before, old_group);
            let new = chunk_group_text(after, std::slice::from_ref(new_block));
            if old != new {
                planned.push(structured_replacement(
                    before,
                    after,
                    old_group,
                    std::slice::from_ref(new_block),
                ));
            }
        }
    } else {
        let boundaries = best_review_partition(after, new_blocks, before, old_blocks);
        for (index, old_block) in old_blocks.iter().enumerate() {
            let new_group = &new_blocks[boundaries[index]..boundaries[index + 1]];
            let old = chunk_group_text(before, std::slice::from_ref(old_block));
            let new = chunk_group_text(after, new_group);
            if old != new {
                planned.push(structured_replacement(
                    before,
                    after,
                    std::slice::from_ref(old_block),
                    new_group,
                ));
            }
        }
    }
    Some(planned)
}

fn group_review_atoms(
    before: &str,
    atoms: &[PlannedTextChange],
    granularity: ReviewGranularity,
) -> Result<Vec<PlannedTextChange>> {
    if granularity == ReviewGranularity::Exact || atoms.len() < 2 {
        return Ok(atoms.to_vec());
    }

    let mut planned = Vec::new();
    for segment in safe_review_segments(before, atoms)? {
        let protected = segment.len() == 1 && protected_review_atom(before, &segment[0].change)?;
        match granularity {
            ReviewGranularity::Exact => planned.extend_from_slice(segment),
            ReviewGranularity::Word => {
                if protected {
                    planned.extend_from_slice(segment);
                } else {
                    planned.extend(group_words(before, segment)?);
                }
            }
            ReviewGranularity::Sentence => {
                planned.push(merge_review_cluster(before, segment)?);
            }
            ReviewGranularity::Adaptive => {
                if !protected && dense_review_segment(before, segment)? {
                    planned.push(merge_review_cluster(before, segment)?);
                } else if protected {
                    planned.extend_from_slice(segment);
                } else {
                    planned.extend(group_words(before, segment)?);
                }
            }
        }
    }
    Ok(planned)
}

/// Plan human-reviewable changes while preserving the exact transport diff for
/// ordinary synchronization. A single contiguous edit is never widened. Only
/// multiple nearby atoms may be combined. Large changes are scanned into
/// LaTeX-aware prose, list, figure, table, math, code, and drawing blocks before
/// alignment, while small edits retain exact character ranges.
pub fn review_text_changes(
    before: &str,
    after: &str,
    granularity: ReviewGranularity,
) -> Result<Vec<PlannedTextChange>> {
    let atoms: Vec<_> = minimal_text_changes(before, after)
        .into_iter()
        .map(planned_atom)
        .collect();
    if matches!(
        granularity,
        ReviewGranularity::Adaptive | ReviewGranularity::Sentence
    ) && let Some(planned) = structural_review_changes(before, after, atoms.len())
    {
        return Ok(planned);
    }
    group_review_atoms(before, &atoms, granularity)
}

pub fn review_text_changes_for_algorithm(
    before: &str,
    after: &str,
    granularity: ReviewGranularity,
    algorithm: &str,
) -> Result<Vec<PlannedTextChange>> {
    match algorithm {
        REVIEW_DIFF_ALGORITHM => review_text_changes(before, after, granularity),
        LEGACY_REVIEW_DIFF_ALGORITHM => {
            let atoms: Vec<_> = minimal_text_changes(before, after)
                .into_iter()
                .map(planned_atom)
                .collect();
            group_review_atoms(before, &atoms, granularity)
        }
        _ => bail!("unsupported review diff algorithm '{algorithm}'"),
    }
}

pub fn supported_review_diff_algorithm(algorithm: &str) -> bool {
    matches!(
        algorithm,
        REVIEW_DIFF_ALGORITHM | LEGACY_REVIEW_DIFF_ALGORITHM
    )
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

/// Translate a history-OT source coordinate into the visible CodeMirror
/// coordinate space, collapsing tracked deletions to their visible boundary.
pub fn source_to_visible_position(position: usize, state: &DocumentState) -> Result<usize> {
    let source_len = utf16_len(&state.source_content);
    ensure!(
        position <= source_len,
        "source position {position} is outside document range 0..{source_len}"
    );
    if state.ot_type != HISTORY_OT {
        return Ok(position);
    }

    let mut hidden = 0;
    let mut source_cursor = 0;
    for range in &state.tracked_delete_ranges {
        if range.pos < source_cursor || range.pos > source_len {
            continue;
        }
        let end = source_len.min(range.pos.saturating_add(range.length));
        if position <= range.pos {
            break;
        }
        hidden += position.min(end) - range.pos;
        source_cursor = end;
    }
    Ok(position - hidden)
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
    fn complete_text_diff_keeps_independent_edits_separate() {
        let before = "alpha middle omega";
        let after = "ALPHA middle OMEGA";
        let inputs = minimal_text_changes(before, after);
        assert_eq!(inputs.len(), 2);
        let normalized = normalize_changes(&inputs, before).unwrap();
        assert_eq!(apply_changes(before, &normalized).unwrap(), after);
        assert_eq!(inputs[0].expect.as_deref(), Some("alpha"));
        assert_eq!(inputs[1].expect.as_deref(), Some("omega"));
    }

    #[test]
    fn complete_text_diff_uses_codemirror_utf16_positions() {
        let before = "😀 old and stay";
        let after = "😀 new and stay!";
        let inputs = minimal_text_changes(before, after);
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].from, 3);
        assert_eq!(inputs[0].to, Some(6));
        assert_eq!(inputs[1].from, utf16_len(before));
        let normalized = normalize_changes(&inputs, before).unwrap();
        assert_eq!(apply_changes(before, &normalized).unwrap(), after);
    }

    fn apply_review_plan(
        before: &str,
        after: &str,
        granularity: ReviewGranularity,
    ) -> Vec<PlannedTextChange> {
        let planned = review_text_changes(before, after, granularity).unwrap();
        let inputs: Vec<_> = planned.iter().map(|item| item.change.clone()).collect();
        let normalized = normalize_changes(&inputs, before).unwrap();
        assert_eq!(apply_changes(before, &normalized).unwrap(), after);
        planned
    }

    #[test]
    fn review_plan_never_widens_a_single_letter_edit() {
        for granularity in [
            ReviewGranularity::Exact,
            ReviewGranularity::Adaptive,
            ReviewGranularity::Word,
            ReviewGranularity::Sentence,
        ] {
            let planned = apply_review_plan("model", "models", granularity);
            assert_eq!(planned.len(), 1);
            assert_eq!(planned[0].change.expect.as_deref(), Some(""));
            assert_eq!(planned[0].change.insert.as_deref(), Some("s"));
            assert_eq!(planned[0].atomic_change_count, 1);
        }
    }

    #[test]
    fn review_plan_groups_fragmented_edits_inside_a_word() {
        let planned = apply_review_plan("abcdef", "abXdeY", ReviewGranularity::Adaptive);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].atomic_change_count, 2);
        assert_eq!(planned[0].change.expect.as_deref(), Some("cdef"));
        assert_eq!(planned[0].change.insert.as_deref(), Some("XdeY"));
    }

    #[test]
    fn adaptive_review_plan_groups_a_dense_phrase_rewrite() {
        let planned = apply_review_plan(
            "the quick brown fox",
            "a fast red fox",
            ReviewGranularity::Adaptive,
        );
        assert_eq!(planned.len(), 1);
        assert!(planned[0].atomic_change_count >= 3);
        assert_eq!(planned[0].change.expect.as_deref(), Some("the quick brown"));
        assert_eq!(planned[0].change.insert.as_deref(), Some("a fast red"));
    }

    #[test]
    fn adaptive_review_plan_aligns_paragraph_to_itemize_rewrites() {
        let before = "Three challenges determine whether this observation supports a recovery method. First, the information must be absent from the passive outputs against which the method is compared. We construct score vectors with identical complete finite-precision passive outputs and retain queries that distinguish them. Second, recovery must handle an unknown tie priority and bounded biases without receiving a numerical comparison result. We derive binary queries that identify the scores exactly and bound their cost. Third, removing the dependence must occur before the operation that introduces it. We compare preprocessing orders and measure their effects on both the distinguishing queries and independently generated random biases.";
        let after = r#"We address three questions about this numerical channel:
\begin{itemize}
\item \textbf{What additional information is disclosed?} We test whether chosen biases distinguish score vectors with identical complete passive outputs.
\item \textbf{How can absolute scores be recovered?} We seek exact recovery from token responses under bounded biases and unknown tie priority.
\item \textbf{How does processing order affect disclosure?} We compare subtraction of the maximum before and after bias addition, examining disclosure and preservation of token decisions under random biases.
\end{itemize}"#;

        let planned = apply_review_plan(before, after, ReviewGranularity::Adaptive);
        assert_eq!(planned.len(), 4);
        assert!(
            planned[0]
                .change
                .expect
                .as_deref()
                .is_some_and(|text| text.starts_with("Three challenges"))
        );
        assert!(
            planned[0]
                .change
                .insert
                .as_deref()
                .is_some_and(|text| text.starts_with("We address"))
        );
        assert!(
            planned[1]
                .change
                .expect
                .as_deref()
                .is_some_and(|text| text.contains("First,") && text.contains("We construct"))
        );
        assert!(
            planned[1]
                .change
                .insert
                .as_deref()
                .is_some_and(|text| text.contains(r"\begin{itemize}") && text.contains(r"\item"))
        );
        assert!(
            planned[2]
                .change
                .expect
                .as_deref()
                .is_some_and(|text| text.contains("Second,") && text.contains("We derive"))
        );
        assert!(
            planned[3]
                .change
                .expect
                .as_deref()
                .is_some_and(|text| text.contains("Third,") && text.contains("We compare"))
        );
        assert!(
            planned[3]
                .change
                .insert
                .as_deref()
                .is_some_and(|text| text.ends_with(r"\end{itemize}"))
        );

        let legacy = review_text_changes_for_algorithm(
            before,
            after,
            ReviewGranularity::Adaptive,
            LEGACY_REVIEW_DIFF_ALGORITHM,
        )
        .unwrap();
        assert!(legacy.len() > planned.len());
    }

    #[test]
    fn adaptive_review_plan_preserves_figure_components() {
        let before = r#"\begin{figure}
\centering
\includegraphics[width=.45\textwidth]{baseline-left.pdf}
\includegraphics[width=.45\textwidth]{baseline-right.pdf}
\caption{Baseline accuracy and latency under the original configuration.}
\label{fig:comparison}
\end{figure}
"#;
        let after = r#"\begin{figure}
\centering
\includegraphics[width=.48\textwidth]{revised-primary.pdf}
\includegraphics[width=.48\textwidth]{revised-secondary.pdf}
\caption{Revised precision and throughput under the hardened configuration.}
\label{fig:comparison}
\end{figure}
"#;
        assert!(minimal_text_changes(before, after).len() >= 8);
        let planned = apply_review_plan(before, after, ReviewGranularity::Adaptive);
        assert_eq!(planned.len(), 3);
        assert!(
            planned[0]
                .change
                .expect
                .as_deref()
                .is_some_and(|text| text.contains("baseline-left.pdf"))
        );
        assert!(planned[1].change.expect.as_deref().is_some_and(|text| {
            text.starts_with(r"\includegraphics") && text.contains("baseline-right.pdf")
        }));
        assert!(planned[2].change.expect.as_deref().is_some_and(
            |text| text.starts_with(r"\caption")
                && text.contains(r"\label{fig:comparison}")
                && text.ends_with("\\end{figure}\n")
        ));
    }

    #[test]
    fn adaptive_review_plan_treats_protected_environments_as_atomic() {
        let cases = [
            (
                r#"\begin{table}
\begin{tabular}{ll}
alpha & one \\
bravo & two \\
charlie & three \\
delta & four \\
echo & five \\
foxtrot & six \\
\end{tabular}
\end{table}
"#,
                r#"\begin{table}
\begin{tabular}{ll}
amber & first \\
blue & second \\
cyan & third \\
denim & fourth \\
emerald & fifth \\
fuchsia & sixth \\
\end{tabular}
\end{table}
"#,
            ),
            (
                r#"\begin{align}
a_1 &= b_1 + c_1 \\
a_2 &= b_2 + c_2 \\
a_3 &= b_3 + c_3 \\
a_4 &= b_4 + c_4
\end{align}
"#,
                r#"\begin{align}
x_1 &= y_1 - z_1 \\
x_2 &= y_2 - z_2 \\
x_3 &= y_3 - z_3 \\
x_4 &= y_4 - z_4
\end{align}
"#,
            ),
            (
                r#"\begin{lstlisting}
let alpha = first();
let bravo = second();
let charlie = third();
let delta = fourth();
\end{lstlisting}
"#,
                r#"\begin{lstlisting}
const amber = primary();
const blue = secondary();
const cyan = tertiary();
const denim = quaternary();
\end{lstlisting}
"#,
            ),
            (
                r#"\begin{tikzpicture}
\draw[red] (0,0) -- (1,1);
\draw[blue] (1,0) -- (2,1);
\draw[green] (2,0) -- (3,1);
\draw[black] (3,0) -- (4,1);
\end{tikzpicture}
"#,
                r#"\begin{tikzpicture}
\draw[cyan] (0,1) -- (1,2);
\draw[magenta] (1,1) -- (2,2);
\draw[yellow] (2,1) -- (3,2);
\draw[gray] (3,1) -- (4,2);
\end{tikzpicture}
"#,
            ),
        ];

        for (before, after) in cases {
            assert!(minimal_text_changes(before, after).len() >= 8);
            let planned = apply_review_plan(before, after, ReviewGranularity::Adaptive);
            assert_eq!(planned.len(), 1);
            assert_eq!(planned[0].change.expect.as_deref(), Some(before));
            assert_eq!(planned[0].change.insert.as_deref(), Some(after));
        }
    }

    #[test]
    fn adaptive_review_plan_falls_back_for_unknown_latex_environments() {
        let before = r#"\begin{customlayout}
alpha first beta second gamma third delta fourth.
epsilon fifth zeta sixth eta seventh theta eighth.
\end{customlayout}
"#;
        let after = r#"\begin{customlayout}
amber primary blue secondary cyan tertiary denim quaternary.
emerald fifth fuchsia sixth gold seventh hazel eighth.
\end{customlayout}
"#;
        assert!(minimal_text_changes(before, after).len() >= 8);
        let current = review_text_changes(before, after, ReviewGranularity::Adaptive).unwrap();
        let legacy = review_text_changes_for_algorithm(
            before,
            after,
            ReviewGranularity::Adaptive,
            LEGACY_REVIEW_DIFF_ALGORITHM,
        )
        .unwrap();
        assert_eq!(current, legacy);
    }

    #[test]
    fn review_plan_does_not_cross_latex_or_sentence_boundaries() {
        let before = "alpha \\textbf{beta}. gamma";
        let after = "ALPHA \\textit{BETA}. GAMMA";
        for granularity in [
            ReviewGranularity::Adaptive,
            ReviewGranularity::Word,
            ReviewGranularity::Sentence,
        ] {
            let planned = apply_review_plan(before, after, granularity);
            assert!(planned.len() >= 4);
            for item in planned {
                let removed = item.change.expect.as_deref().unwrap_or_default();
                assert!(!(removed.contains("alpha") && removed.contains("beta")));
                assert!(!(removed.contains("beta") && removed.contains("gamma")));
            }
        }
    }

    #[test]
    fn review_plan_does_not_cross_math_comments_or_lines() {
        for (before, after) in [
            ("alpha $x$ beta", "ALPHA $y$ BETA"),
            ("alpha % note\nbeta", "ALPHA % NOTE\nBETA"),
            ("alpha\nbeta", "ALPHA\nBETA"),
        ] {
            let planned = apply_review_plan(before, after, ReviewGranularity::Sentence);
            assert!(planned.len() >= 2);
            for item in planned {
                let removed = item.change.expect.as_deref().unwrap_or_default();
                assert!(!(removed.contains("alpha") && removed.contains("beta")));
            }
        }
    }

    #[test]
    fn review_plan_preserves_utf16_coordinates_when_grouped() {
        let before = "😀 abcdef tail";
        let after = "😀 abXdeY tail";
        let planned = apply_review_plan(before, after, ReviewGranularity::Word);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].change.from, 5);
        assert_eq!(planned[0].change.to, Some(9));
    }

    #[test]
    fn word_review_does_not_treat_a_cjk_sentence_as_one_word() {
        let planned = apply_review_plan("甲乙丙丁戊", "甲X丙Y戊", ReviewGranularity::Word);
        assert_eq!(planned.len(), 2);
        assert!(planned.iter().all(|item| item.atomic_change_count == 1));
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
