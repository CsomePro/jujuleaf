use std::cmp::Ordering;
use std::ops::Range;

use anyhow::{Result, ensure};
use similar::{Algorithm, DiffTag};
use unicode_segmentation::UnicodeSegmentation;

use crate::latex_blocks::{LatexBlockKind, scan_latex_blocks};
use crate::operations::{
    InputChange, PlannedTextChange, ReviewAnchor, ReviewGranularity,
    capture_well_formed_diff_slices, minimal_text_changes, slice_utf16, utf16_len,
};

const HUNK_OVERHEAD: usize = 32;
const TINY_FRAGMENT_PENALTY: usize = 12;
const MAX_MERGED_ATOMS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReviewLevel {
    Word,
    Sentence,
    Line,
}

#[derive(Debug)]
struct TextHierarchy {
    words: Vec<Range<usize>>,
    sentences: Vec<Range<usize>>,
    lines: Vec<Range<usize>>,
    latex_barriers: Vec<usize>,
}

impl TextHierarchy {
    fn new(text: &str) -> Self {
        let lines = line_ranges(text);
        let mut words = Vec::new();
        let mut sentences = Vec::new();
        let mut line_base = 0;

        for line in text.split_inclusive('\n') {
            let without_newline = line.strip_suffix('\n').unwrap_or(line);
            let content = without_newline
                .strip_suffix('\r')
                .unwrap_or(without_newline);
            let mut previous_byte = 0;
            let mut position = 0;
            for (byte_from, word) in content.unicode_word_indices() {
                position += utf16_len(&content[previous_byte..byte_from]);
                let from = line_base + position;
                words.push(from..from + utf16_len(word));
                previous_byte = byte_from;
            }

            let mut previous_byte = 0;
            let mut position = 0;
            for (byte_from, sentence) in content.split_sentence_bound_indices() {
                position += utf16_len(&content[previous_byte..byte_from]);
                let from = line_base + position;
                let to = from + utf16_len(sentence);
                if from < to {
                    sentences.push(from..to);
                }
                previous_byte = byte_from;
            }
            line_base += utf16_len(line);
        }

        if !text.is_empty() && !text.ends_with('\n') && sentences.is_empty() {
            sentences.push(0..utf16_len(text));
        }

        let mut latex_byte_barriers = Vec::new();
        let scan = scan_latex_blocks(text);
        if scan.valid {
            for block in scan.blocks {
                if matches!(
                    block.kind,
                    LatexBlockKind::Command
                        | LatexBlockKind::FigurePart
                        | LatexBlockKind::Math
                        | LatexBlockKind::Code
                        | LatexBlockKind::Drawing
                        | LatexBlockKind::Opaque
                        | LatexBlockKind::Unknown
                ) {
                    latex_byte_barriers.push(block.byte_from);
                    latex_byte_barriers.push(block.byte_to);
                }
            }
        }
        latex_byte_barriers.sort_unstable();
        latex_byte_barriers.dedup();
        let mut previous_byte = 0;
        let mut position = 0;
        let latex_barriers = latex_byte_barriers
            .into_iter()
            .map(|byte| {
                position += utf16_len(&text[previous_byte..byte]);
                previous_byte = byte;
                position
            })
            .collect();

        Self {
            words,
            sentences,
            lines,
            latex_barriers,
        }
    }

    fn contains(&self, level: ReviewLevel, from: usize, to: usize) -> bool {
        let ranges = match level {
            ReviewLevel::Word => &self.words,
            ReviewLevel::Sentence => &self.sentences,
            ReviewLevel::Line => &self.lines,
        };
        contained_by(ranges, from, to)
    }

    fn crosses_latex_boundary(&self, from: usize, to: usize) -> bool {
        let next = self
            .latex_barriers
            .partition_point(|boundary| *boundary <= from);
        self.latex_barriers
            .get(next)
            .is_some_and(|boundary| *boundary < to)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlanScore {
    cost: usize,
    hunks: usize,
    swallowed: usize,
}

impl PlanScore {
    const ZERO: Self = Self {
        cost: 0,
        hunks: 0,
        swallowed: 0,
    };

    fn plus(self, other: Self) -> Self {
        Self {
            cost: self.cost.saturating_add(other.cost),
            hunks: self.hunks.saturating_add(other.hunks),
            swallowed: self.swallowed.saturating_add(other.swallowed),
        }
    }
}

impl Ord for PlanScore {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.cost, self.hunks, self.swallowed).cmp(&(other.cost, other.hunks, other.swallowed))
    }
}

impl PartialOrd for PlanScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy)]
struct Choice {
    next: usize,
    merged: bool,
}

#[derive(Debug, Clone, Copy)]
struct CandidateMetrics {
    level: ReviewLevel,
    changed: usize,
    swallowed: usize,
    max_gap: usize,
}

const REVIEW_LEVELS: [ReviewLevel; 3] =
    [ReviewLevel::Word, ReviewLevel::Sentence, ReviewLevel::Line];

#[derive(Debug)]
struct PlannerFacts {
    changed_prefix: Vec<usize>,
    gap_prefix: Vec<usize>,
    gap_lengths: Vec<usize>,
    atom_hard_prefix: [Vec<usize>; 3],
    gap_hard_prefix: [Vec<usize>; 3],
}

impl PlannerFacts {
    fn new(before: &str, atoms: &[PlannedTextChange]) -> Result<Self> {
        let mut changed_prefix = Vec::with_capacity(atoms.len() + 1);
        changed_prefix.push(0usize);
        for atom in atoms {
            changed_prefix.push(
                changed_prefix
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(atom.removed + atom.inserted),
            );
        }

        let mut atom_hard_prefix = std::array::from_fn(|_| Vec::with_capacity(atoms.len() + 1));
        for prefix in &mut atom_hard_prefix {
            prefix.push(0usize);
        }
        for atom in atoms {
            for (level_index, level) in REVIEW_LEVELS.into_iter().enumerate() {
                let hard = contains_hard_latex_boundary(
                    atom.change.expect.as_deref().unwrap_or_default(),
                    level,
                ) || contains_inserted_block_boundary(
                    atom.change.insert.as_deref().unwrap_or_default(),
                    level,
                );
                let previous = atom_hard_prefix[level_index]
                    .last()
                    .copied()
                    .unwrap_or_default();
                atom_hard_prefix[level_index].push(previous + usize::from(hard));
            }
        }

        let mut gap_prefix = Vec::with_capacity(atoms.len());
        gap_prefix.push(0usize);
        let mut gap_lengths = Vec::with_capacity(atoms.len().saturating_sub(1));
        let mut gap_hard_prefix = std::array::from_fn(|_| Vec::with_capacity(atoms.len()));
        for prefix in &mut gap_hard_prefix {
            prefix.push(0usize);
        }
        for pair in atoms.windows(2) {
            let gap_from = change_end(&pair[0].change);
            let gap_to = pair[1].change.from;
            let gap = slice_utf16(before, gap_from, gap_to)?;
            let length = utf16_len(gap);
            gap_lengths.push(length);
            gap_prefix.push(
                gap_prefix
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(length),
            );
            for (level_index, level) in REVIEW_LEVELS.into_iter().enumerate() {
                let previous = gap_hard_prefix[level_index]
                    .last()
                    .copied()
                    .unwrap_or_default();
                gap_hard_prefix[level_index]
                    .push(previous + usize::from(contains_hard_latex_boundary(gap, level)));
            }
        }

        Ok(Self {
            changed_prefix,
            gap_prefix,
            gap_lengths,
            atom_hard_prefix,
            gap_hard_prefix,
        })
    }

    fn changed(&self, start: usize, end: usize) -> usize {
        self.changed_prefix[end] - self.changed_prefix[start]
    }

    fn swallowed(&self, start: usize, end: usize) -> usize {
        self.gap_prefix[end - 1] - self.gap_prefix[start]
    }

    fn max_gap(&self, start: usize, end: usize) -> usize {
        self.gap_lengths[start..end - 1]
            .iter()
            .copied()
            .max()
            .unwrap_or_default()
    }

    fn has_hard_boundary(&self, start: usize, end: usize, level_index: usize) -> bool {
        self.atom_hard_prefix[level_index][end] > self.atom_hard_prefix[level_index][start]
            || self.gap_hard_prefix[level_index][end - 1] > self.gap_hard_prefix[level_index][start]
    }
}

pub(crate) fn hierarchical_review_changes(
    before: &str,
    after: &str,
    granularity: ReviewGranularity,
    _anchors: &[ReviewAnchor],
) -> Result<Vec<PlannedTextChange>> {
    // Overleaf rebases comments to a replacement boundary. Sentence, line,
    // and existing LaTeX boundaries keep that movement local without turning
    // every edit inside a comment into a separate tracked change.
    let atoms = grapheme_review_atoms(before, after);
    if granularity == ReviewGranularity::Exact || atoms.len() < 2 {
        return Ok(atoms);
    }

    let hierarchy = TextHierarchy::new(before);
    let facts = PlannerFacts::new(before, &atoms)?;
    let mut best = vec![PlanScore::ZERO; atoms.len() + 1];
    let mut choices = vec![
        Choice {
            next: 0,
            merged: false,
        };
        atoms.len()
    ];

    for start in (0..atoms.len()).rev() {
        let atom_score = atomic_score(&atoms[start]).plus(best[start + 1]);
        best[start] = atom_score;
        choices[start] = Choice {
            next: start + 1,
            merged: false,
        };

        let limit = atoms.len().min(start + MAX_MERGED_ATOMS);
        for end in start + 2..=limit {
            let Some(metrics) =
                candidate_metrics(&atoms, start, end, &facts, &hierarchy, granularity)
            else {
                continue;
            };
            let candidate_score = merged_score(metrics).plus(best[end]);
            if candidate_score < best[start] {
                best[start] = candidate_score;
                choices[start] = Choice {
                    next: end,
                    merged: true,
                };
            }
        }
    }

    let mut planned = Vec::new();
    let mut index = 0;
    while index < atoms.len() {
        let choice = choices[index];
        if choice.merged {
            planned.push(merge_atoms(before, &atoms[index..choice.next])?);
        } else {
            planned.push(atoms[index].clone());
        }
        index = choice.next;
    }
    Ok(planned)
}

fn grapheme_review_atoms(before: &str, after: &str) -> Vec<PlannedTextChange> {
    if before == after {
        return Vec::new();
    }
    let old = before.graphemes(true).collect::<Vec<_>>();
    let new = after.graphemes(true).collect::<Vec<_>>();
    let old_offsets = utf16_offsets(&old);
    let new_offsets = utf16_offsets(&new);
    let operations = capture_well_formed_diff_slices(Algorithm::Myers, &old, &new);

    let mut atoms = Vec::new();

    for operation in operations {
        if operation.tag() == DiffTag::Equal {
            continue;
        }
        let old_range = operation.old_range();
        let new_range = operation.new_range();
        let removed = old[old_range.clone()].concat();
        let inserted = new[new_range.clone()].concat();
        atoms.push(PlannedTextChange {
            change: InputChange {
                from: old_offsets[old_range.start],
                to: Some(old_offsets[old_range.end]),
                insert: Some(inserted.clone()),
                expect: Some(removed.clone()),
            },
            atomic_change_count: minimal_text_changes(&removed, &inserted).len().max(1),
            removed: old_offsets[old_range.end] - old_offsets[old_range.start],
            inserted: new_offsets[new_range.end] - new_offsets[new_range.start],
        });
    }
    atoms
}

fn utf16_offsets(parts: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(parts.len() + 1);
    let mut position = 0;
    offsets.push(position);
    for part in parts {
        position += utf16_len(part);
        offsets.push(position);
    }
    offsets
}

fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut position = 0;
    for line in text.split_inclusive('\n') {
        let line_length = utf16_len(line);
        let newline_length = if line.ends_with("\r\n") {
            2
        } else {
            usize::from(line.ends_with('\n'))
        };
        ranges.push(position..position + line_length.saturating_sub(newline_length));
        position += line_length;
    }
    if text.is_empty() {
        ranges.push(0..0);
    }
    ranges
}

fn contained_by(ranges: &[Range<usize>], from: usize, to: usize) -> bool {
    let index = ranges.partition_point(|range| range.start <= from);
    ranges.get(index.saturating_sub(1)).is_some_and(|range| {
        if from == to {
            range.start <= from && from <= range.end
        } else {
            range.start <= from && to <= range.end
        }
    })
}

fn allowed_level(granularity: ReviewGranularity, level: ReviewLevel) -> bool {
    match granularity {
        ReviewGranularity::Exact => false,
        ReviewGranularity::Word => level <= ReviewLevel::Word,
        ReviewGranularity::Sentence => level <= ReviewLevel::Sentence,
        ReviewGranularity::Adaptive => true,
    }
}

fn candidate_metrics(
    atoms: &[PlannedTextChange],
    start: usize,
    end: usize,
    facts: &PlannerFacts,
    hierarchy: &TextHierarchy,
    granularity: ReviewGranularity,
) -> Option<CandidateMetrics> {
    let from = atoms[start].change.from;
    let to = change_end(&atoms[end - 1].change);
    if hierarchy.crosses_latex_boundary(from, to) {
        return None;
    }

    let swallowed = facts.swallowed(start, end);
    let max_gap = facts.max_gap(start, end);
    let changed = facts.changed(start, end);

    for (level_index, level) in REVIEW_LEVELS.into_iter().enumerate() {
        if !allowed_level(granularity, level) || !hierarchy.contains(level, from, to) {
            continue;
        }
        if facts.has_hard_boundary(start, end, level_index) {
            continue;
        }

        let dense = match level {
            ReviewLevel::Word => max_gap <= 12 && swallowed <= changed.saturating_mul(2).max(4),
            ReviewLevel::Sentence => max_gap <= 48 && swallowed.saturating_mul(2) <= changed * 3,
            ReviewLevel::Line => end - start >= 3 && max_gap <= 64 && swallowed <= changed,
        };
        if dense {
            return Some(CandidateMetrics {
                level,
                changed,
                swallowed,
                max_gap,
            });
        }
    }
    None
}

fn contains_hard_latex_boundary(text: &str, level: ReviewLevel) -> bool {
    text.chars().any(|character| {
        matches!(
            character,
            '\\' | '{' | '}' | '$' | '%' | '#' | '^' | '~' | '\n' | '\r'
        ) || (level != ReviewLevel::Line && character == '&')
    })
}

fn contains_inserted_block_boundary(text: &str, level: ReviewLevel) -> bool {
    text.chars().any(|character| {
        matches!(character, '$' | '%' | '#' | '^' | '\n' | '\r')
            || (level != ReviewLevel::Line && character == '&')
    })
}

fn atomic_score(atom: &PlannedTextChange) -> PlanScore {
    let changed = atom.removed + atom.inserted;
    PlanScore {
        cost: HUNK_OVERHEAD
            + if changed <= 2 {
                TINY_FRAGMENT_PENALTY
            } else {
                0
            },
        hunks: 1,
        swallowed: 0,
    }
}

fn merged_score(metrics: CandidateMetrics) -> PlanScore {
    let context_weight = match metrics.level {
        ReviewLevel::Word | ReviewLevel::Sentence => 1,
        ReviewLevel::Line => 2,
    };
    let density_bonus = metrics.changed.saturating_sub(metrics.swallowed).min(16);
    PlanScore {
        cost: HUNK_OVERHEAD
            .saturating_add(metrics.swallowed * context_weight)
            .saturating_add(metrics.max_gap / 8)
            .saturating_sub(density_bonus),
        hunks: 1,
        swallowed: metrics.swallowed,
    }
}

fn merge_atoms(before: &str, atoms: &[PlannedTextChange]) -> Result<PlannedTextChange> {
    ensure!(!atoms.is_empty(), "cannot merge an empty review cluster");
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

fn change_end(change: &InputChange) -> usize {
    change.to.unwrap_or(change.from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::{apply_changes, normalize_changes};

    fn plan(
        before: &str,
        after: &str,
        granularity: ReviewGranularity,
        anchors: &[ReviewAnchor],
    ) -> Vec<PlannedTextChange> {
        let planned = hierarchical_review_changes(before, after, granularity, anchors).unwrap();
        let inputs = planned
            .iter()
            .map(|item| item.change.clone())
            .collect::<Vec<_>>();
        let normalized = normalize_changes(&inputs, before).unwrap();
        assert_eq!(apply_changes(before, &normalized).unwrap(), after);
        planned
    }

    #[test]
    fn isolated_letters_stay_exact() {
        let planned = plan(
            "The model achieve.",
            "The models achieves.",
            ReviewGranularity::Adaptive,
            &[],
        );
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].change.expect.as_deref(), Some(""));
        assert_eq!(planned[0].change.insert.as_deref(), Some("s"));
        assert_eq!(planned[1].change.insert.as_deref(), Some("s"));
    }

    #[test]
    fn dense_sentence_rewrite_becomes_one_hunk() {
        let before = "We use a fixed threshold for every sample. Results remain stable.";
        let after =
            "We estimate an adaptive threshold from validation data. Results remain stable.";
        let planned = plan(before, after, ReviewGranularity::Adaptive, &[]);
        assert_eq!(planned.len(), 1);
        assert!(
            !planned[0]
                .change
                .expect
                .as_deref()
                .unwrap()
                .contains("Results remain stable")
        );
    }

    #[test]
    fn dense_table_row_keeps_stable_latex_edges() {
        let before = "Ours & 89.4 & 88.7 & 90.1 \\\\\n";
        let after = "Ours & 91.2 & 90.8 & 92.0 \\\\\n";
        let planned = plan(before, after, ReviewGranularity::Adaptive, &[]);
        assert_eq!(planned.len(), 1);
        let removed = planned[0].change.expect.as_deref().unwrap();
        assert_eq!(removed, "89.4 & 88.7 & 90.1");
    }

    #[test]
    fn line_is_a_hard_default_boundary() {
        let before = "alpha one two three\nbeta four five six\n";
        let after = "alpha red green blue\nbeta cyan black white\n";
        let planned = plan(before, after, ReviewGranularity::Adaptive, &[]);
        assert!(planned.len() >= 2);
        assert!(planned.iter().all(|item| {
            !item
                .change
                .expect
                .as_deref()
                .unwrap_or_default()
                .contains('\n')
        }));
    }

    #[test]
    fn unicode_graphemes_are_not_split() {
        let before = "Cafe\u{301} noir";
        let after = "Cafe\u{300} noir";
        let planned = plan(before, after, ReviewGranularity::Exact, &[]);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].change.expect.as_deref(), Some("e\u{301}"));
        assert_eq!(planned[0].change.insert.as_deref(), Some("e\u{300}"));
    }

    #[test]
    fn compacted_crossing_edits_fall_back_to_monotonic_operations() {
        for granularity in [
            ReviewGranularity::Exact,
            ReviewGranularity::Word,
            ReviewGranularity::Sentence,
            ReviewGranularity::Adaptive,
        ] {
            let planned = plan("aab", "bbb", granularity, &[]);
            assert!(
                planned
                    .windows(2)
                    .all(|pair| change_end(&pair[0].change) <= pair[1].change.from)
            );
        }
    }

    #[test]
    fn repeated_and_unicode_graphemes_always_produce_applicable_plans() {
        let samples = [
            "",
            "a",
            "aa",
            "aab",
            "bbb",
            "ababa",
            "🙂a🙂",
            "🙂🙂b",
            "e\u{301}e\u{301}",
            "e\u{300}e\u{301}",
        ];
        for before in samples {
            for after in samples {
                for granularity in [
                    ReviewGranularity::Exact,
                    ReviewGranularity::Word,
                    ReviewGranularity::Sentence,
                    ReviewGranularity::Adaptive,
                ] {
                    plan(before, after, granularity, &[]);
                }
            }
        }
    }

    #[test]
    fn comment_anchor_does_not_fragment_a_dense_sentence_rewrite() {
        let before = "Context. A fixed interval is selected before evaluation. Tail.";
        let after = "Context. When the interval is unknown, the controller searches several public candidates and retains the unique consistent candidate. Tail.";
        let anchor_from = utf16_len("Context. ");
        let anchor_to = anchor_from + utf16_len("A fixed interval is selected before evaluation.");
        let planned = plan(
            before,
            after,
            ReviewGranularity::Adaptive,
            &[ReviewAnchor {
                from: anchor_from,
                to: anchor_to,
            }],
        );
        assert_eq!(planned.len(), 1);
        let removed = planned[0].change.expect.as_deref().unwrap();
        assert!(!removed.contains("Context."));
        assert!(!removed.contains("Tail."));
    }

    #[test]
    fn comment_anchor_with_latex_reference_stays_readable() {
        let before = "Context. In a search over multiple public intervals, uniqueness is defined relative to the prespecified search domain. Tail.";
        let after = "Context. When the reachable interval is unknown, the controller searches a prespecified collection of candidate intervals and accepts a candidate only when it is unique across that searched collection (Section~\\ref{sec:consistency}). Tail.";
        let anchor_from = utf16_len("Context. ");
        let anchor_to = anchor_from
            + utf16_len(
                "In a search over multiple public intervals, uniqueness is defined relative to the prespecified search domain.",
            );
        let planned = plan(
            before,
            after,
            ReviewGranularity::Adaptive,
            &[ReviewAnchor {
                from: anchor_from,
                to: anchor_to,
            }],
        );
        assert_eq!(
            planned.len(),
            1,
            "unexpectedly fragmented plan: {planned:#?}"
        );
        assert!(planned.iter().all(|item| {
            let removed = item.change.expect.as_deref().unwrap_or_default();
            !removed.contains("Context.") && !removed.contains("Tail.")
        }));
    }

    #[test]
    fn existing_latex_command_remains_a_merge_boundary() {
        let before = "See Section~\\ref{sec:old} for the old result.";
        let after = "Consult Section~\\ref{sec:old} for the revised result.";
        let planned = plan(before, after, ReviewGranularity::Adaptive, &[]);
        assert_eq!(planned.len(), 2);
        assert!(planned.iter().all(|item| {
            let removed = item.change.expect.as_deref().unwrap_or_default();
            let inserted = item.change.insert.as_deref().unwrap_or_default();
            !removed.contains("\\ref") && !inserted.contains("\\ref")
        }));
    }
}
