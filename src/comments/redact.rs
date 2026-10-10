//! The hiding transforms: the text the LLM sees in place of a file.
//!
//! **Redact mode** ([`redact`]) replaces every target comment with a
//! placeholder that keeps the comment's delimiter style and carries the
//! record's inventory id (`//⟦C12⟧`, `#⟦C12⟧`, `/*⟦C12⟧*/`). Everything
//! else — code, non-target comments, functional, protected and
//! `jsx-expression` comments — stays byte-for-byte verbatim.
//!
//! **Remove mode** ([`remove_comments`]) deletes every target comment with
//! the shared removal/line-hygiene engine
//! ([`compute_removal_edits`](super::hygiene::compute_removal_edits)), so
//! the clean text is byte-identical to what an apply removing exactly the
//! targets would write. A segment map ([`Segment`], [`map_clean_offset`])
//! translates clean-text offsets back to original offsets.
//!
//! The transforms never assign ids (the inventory does) and never parse:
//! they are pure byte surgery on original offsets. Their output is prompt
//! material only; every edit that reaches disk is computed against the
//! original text, so placeholder-induced shifts never matter.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::lang::adapter::LanguageAdapter;

use super::hygiene::{compute_removal_edits, is_line_terminator};
use super::inventory::{Inventory, InventoryComment};
use super::scan::line_start_of;
use super::types::{CommentKind, Edit};

/// Opening placeholder bracket, U+27E6 (3 UTF-8 bytes).
const OPEN: &str = "⟦";
/// Closing placeholder bracket, U+27E7 (3 UTF-8 bytes).
const CLOSE: &str = "⟧";

/// Options shared by both hiding transforms.
#[derive(Debug, Clone, Default)]
pub struct TransformOpts<'a> {
    /// The ids of the records to hide. `None` means every editable record.
    /// Ids naming non-editable records (functional, doc-protected,
    /// `jsx-expression`) or no record at all hide nothing: those comments
    /// always stay verbatim.
    pub targets: Option<&'a BTreeSet<String>>,
    /// Restricts the output to this byte range of the original text (one
    /// chunk). Only targets lying entirely inside the range are hidden;
    /// chunk boundaries never cut a comment, since comments travel with
    /// their anchored node. Offsets in the outputs stay original-text
    /// offsets. `None` means the whole file.
    pub range: Option<Range<usize>>,
}

/// The result of [`redact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactOutput {
    /// The redacted text of the requested range (the whole file without a
    /// range). Empty when `fallback` is set: nothing is redacted then, and
    /// the caller must process the file in remove mode instead, so no
    /// unredacted text is ever handed out as if it were blind.
    pub blind_text: String,
    /// Each placeholder's id → the record's original byte range
    /// (`start..end` of the whole record, a group's full span included).
    /// Empty when `fallback` is set.
    pub id_map: BTreeMap<String, Range<usize>>,
    /// The file already contains placeholder-shaped text (`⟦C<digits>⟧`),
    /// so placeholders would be ambiguous: redact mode is not possible for
    /// this file.
    pub fallback: bool,
}

/// Whether `text` contains text a placeholder could be confused with —
/// a match of `⟦C[0-9]+⟧` anywhere in it.
pub fn has_placeholder_collision(text: &str) -> bool {
    let mut rest = text;
    while let Some(found) = rest.find(OPEN) {
        rest = &rest[found + OPEN.len()..];
        if let Some(after_c) = rest.strip_prefix('C') {
            let digits = after_c.len()
                - after_c
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .len();
            if digits > 0 && after_c[digits..].starts_with(CLOSE) {
                return true;
            }
        }
    }
    false
}

/// Redacts `text` (inventoried as `inventory` under `adapter`'s language):
/// every target record within `opts.range` becomes one placeholder in its
/// delimiter style, and everything else is copied verbatim.
///
/// - A line record (a lone line comment or a whole logical group) becomes
///   `<line prefix>⟦id⟧` — `//⟦C3⟧`, `#⟦C3⟧`.
/// - A block or doc record becomes `<open>⟦id⟧<close>` — `/*⟦C4⟧*/` (doc
///   comments redact to plain block placeholders). A language without
///   block comments has no block or doc records; should one appear, it
///   takes the line style, the language's only comment style.
/// - A multi-line block placeholder keeps one line break before its closer
///   when collapsing it would join two lines of code: `return /*` + line
///   break + `*/ x` becomes `return /*⟦C2⟧` + line break + `*/ x`, since a
///   comment containing a line break ends the statement in JavaScript (ASI)
///   and Go (semicolon insertion).
///
/// The collision probe ([`has_placeholder_collision`]) runs first, over the
/// whole file whatever the range; on a collision nothing is redacted and
/// `fallback` is set.
///
/// # Panics
///
/// If `opts.range` is not a range of `text` on character boundaries (chunk
/// ranges always start and end at line boundaries of the original text).
pub fn redact(
    text: &str,
    inventory: &Inventory,
    adapter: &LanguageAdapter,
    opts: TransformOpts<'_>,
) -> RedactOutput {
    if has_placeholder_collision(text) {
        return RedactOutput {
            blind_text: String::new(),
            id_map: BTreeMap::new(),
            fallback: true,
        };
    }
    let range = opts.range.clone().unwrap_or(0..text.len());
    let targets: Vec<&InventoryComment> = inventory
        .comments
        .iter()
        .filter(|comment| is_target(comment, &opts, &range))
        .collect();
    // Back-to-front over ascending, non-overlapping records, so every
    // pending replacement's offsets are still valid when it is applied.
    let mut blind_text = text[range.clone()].to_string();
    let mut id_map = BTreeMap::new();
    for comment in targets.iter().rev() {
        let local = comment.start - range.start..comment.end - range.start;
        blind_text.replace_range(local, &placeholder(comment, adapter, text));
        id_map.insert(comment.id.clone(), comment.start..comment.end);
    }
    RedactOutput {
        blind_text,
        id_map,
        fallback: false,
    }
}

/// Whether `comment` is hidden under `opts`: editable, selected by the
/// targets (all editable records by default), and entirely inside `range`.
fn is_target(comment: &InventoryComment, opts: &TransformOpts<'_>, range: &Range<usize>) -> bool {
    is_selected(comment, opts) && range.start <= comment.start && comment.end <= range.end
}

/// Whether `comment` is editable and selected by the targets (every
/// editable record by default), wherever it lies.
fn is_selected(comment: &InventoryComment, opts: &TransformOpts<'_>) -> bool {
    comment.editable
        && opts
            .targets
            .is_none_or(|targets| targets.contains(&comment.id))
}

/// The placeholder for `comment` in its delimiter style: the line style
/// for a line record and the block style for a block or doc record, each
/// falling back to the language's other style when it lacks that one (a
/// language with neither has no comment records; a bare `⟦id⟧` keeps the
/// function total).
///
/// A block-style placeholder keeps one of the record's line breaks
/// (`/*⟦C2⟧` + line break + `*/`) when collapsing it would join two lines
/// of code: `return /*` + line break + `*/ x` must not read as
/// `return /*⟦C2⟧*/ x`, since a comment containing a line break ends the
/// statement in JavaScript (ASI) and Go (semicolon insertion). The blind
/// text must describe the same program as the original.
fn placeholder(comment: &InventoryComment, adapter: &LanguageAdapter, text: &str) -> String {
    let marker = format!("{OPEN}{}{CLOSE}", comment.id);
    let line = adapter
        .line_prefixes
        .first()
        .map(|prefix| format!("{prefix}{marker}"));
    let block = adapter.block_delimiters.map(|block| {
        let line_break = joined_line_break(comment, text).unwrap_or("");
        format!("{}{marker}{line_break}{}", block.open, block.close)
    });
    let styled = match comment.kind {
        CommentKind::Line => line.or(block),
        CommentKind::Block | CommentKind::Doc => block.or(line),
    };
    styled.unwrap_or(marker)
}

/// The record's first line break, as written (`\r\n`, or a single `\n`,
/// `\r`, U+2028 or U+2029 — the terminators the removal engine and the
/// semantic verifier count), when replacing the record with a one-line
/// placeholder would join two lines of code: the record spans lines, and
/// non-whitespace text precedes it on its first line and follows it on its
/// last. Otherwise the line breaks before or after the record keep the
/// surrounding code's lines apart, and `None`.
fn joined_line_break<'c>(comment: &'c InventoryComment, text: &str) -> Option<&'c str> {
    let at = comment.text.find(is_line_terminator)?;
    let line_start = &text[line_start_of(text, comment.start)..comment.start];
    let before = line_start.rsplit(is_line_terminator).next().unwrap_or("");
    let after = text[comment.end..]
        .split(is_line_terminator)
        .next()
        .unwrap_or("");
    let blank = |slice: &str| slice.chars().all(char::is_whitespace);
    if blank(before) || blank(after) {
        return None;
    }
    let rest = &comment.text[at..];
    let length = if rest.starts_with("\r\n") {
        2
    } else {
        rest.chars().next().map_or(0, char::len_utf8)
    };
    Some(&rest[..length])
}

/// One piece of the clean text ↔ original text correspondence. A
/// [`RemoveOutput`]'s segments partition both its clean text and the
/// original slice, in order: verbatim copies, replacements (the text a
/// removal edit put in place of a longer span) and deletions (an empty
/// clean side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Start in the clean text.
    pub clean_start: usize,
    /// End in the clean text (exclusive; equal to `clean_start` for a
    /// deletion).
    pub clean_end: usize,
    /// Start in the original text (an absolute offset, not relative to
    /// the slice).
    pub original_start: usize,
    /// End in the original text (exclusive).
    pub original_end: usize,
    /// Whether the clean side is a byte-for-byte copy of the original
    /// side (then both have the same length).
    pub verbatim: bool,
}

/// The result of [`remove_comments`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutput {
    /// The requested slice of the original (the whole file without a
    /// range) with the target comments removed.
    pub clean_text: String,
    /// The segments partitioning `clean_text` and the original slice;
    /// never empty (an empty slice has one empty verbatim segment).
    pub offset_map: Vec<Segment>,
    /// Where the slice starts in the original text (`range.start`, or 0).
    pub original_base: usize,
}

/// Removes the target comments of `text` (inventoried as `inventory`),
/// returning the clean text of `opts.range` and its offset map.
///
/// Targets are the editable records `opts.targets` selects (every
/// editable record by default). With a range, only targets entirely
/// inside it are removed from the output; a target straddling a range
/// boundary is left alone (chunk boundaries never cut comments).
/// Hygiene is computed on the whole file, then the edits are clipped to
/// the range, so the clean text of a range is exactly the corresponding
/// slice of the whole-file clean text: an edit belongs to the range its
/// start lies in, and only the part of its deleted span inside the range
/// is cut. In particular, a blank-line collapse that a removal in a
/// neighbouring range causes across the boundary shows here too, which is
/// why targets outside the range take part in the computation.
///
/// # Panics
///
/// If `opts.range` is not a range of `text` on character boundaries.
pub fn remove_comments(text: &str, inventory: &Inventory, opts: TransformOpts<'_>) -> RemoveOutput {
    let range = opts.range.clone().unwrap_or(0..text.len());
    assert!(
        range.start <= range.end
            && range.end <= text.len()
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end),
        "range {range:?} is not a char-boundary range of the text"
    );
    let to_remove: Vec<&InventoryComment> = inventory
        .comments
        .iter()
        .filter(|comment| {
            let inside = range.start <= comment.start && comment.end <= range.end;
            let outside = comment.end <= range.start || range.end <= comment.start;
            is_selected(comment, &opts) && (inside || outside)
        })
        .collect();
    let edits = compute_removal_edits(text, &to_remove);
    clip(text, &edits, range)
}

/// The clean text and segments of `range` under `edits` (ascending,
/// non-overlapping): an edit's replacement is emitted iff its start lies in
/// the range, and its deleted span is cut where it overlaps the range.
fn clip(text: &str, edits: &[Edit], range: Range<usize>) -> RemoveOutput {
    let mut output = RemoveOutput {
        clean_text: String::new(),
        offset_map: Vec::new(),
        original_base: range.start,
    };
    let mut at = range.start;
    for edit in edits {
        let end = edit.start + edit.length;
        let owned = range.contains(&edit.start);
        if !(owned || edit.start < range.start && range.start < end) {
            continue;
        }
        let cut = edit.start.max(range.start)..end.min(range.end);
        if at < cut.start {
            output.push(&text[at..cut.start], at..cut.start, true);
        }
        let replacement = if owned { edit.replacement.as_str() } else { "" };
        output.push(replacement, cut.clone(), false);
        at = cut.end;
    }
    if at < range.end || output.offset_map.is_empty() {
        output.push(&text[at..range.end], at..range.end, true);
    }
    output
}

impl RemoveOutput {
    /// Appends `piece` to the clean text as the counterpart of `original`.
    fn push(&mut self, piece: &str, original: Range<usize>, verbatim: bool) {
        let clean_start = self.clean_text.len();
        self.clean_text.push_str(piece);
        self.offset_map.push(Segment {
            clean_start,
            clean_end: self.clean_text.len(),
            original_start: original.start,
            original_end: original.end,
            verbatim,
        });
    }
}

/// Translates an offset in a [`RemoveOutput`]'s clean text to an offset
/// in the original text, through its `offset_map`.
///
/// An offset inside a verbatim segment maps to the same byte of the
/// original; one inside a replacement maps to the start of the span it
/// replaced. Deletions have no clean bytes: an offset where text was
/// deleted maps to the original position after the deletion (that of the
/// clean byte at that offset). The end of the clean text (or anything past
/// it) maps to the end of the slice. An empty map — which
/// [`remove_comments`] never produces — carries no correspondence and maps
/// every offset to itself.
pub fn map_clean_offset(map: &[Segment], clean_offset: usize) -> usize {
    let index = map.partition_point(|segment| segment.clean_end <= clean_offset);
    match map.get(index) {
        Some(segment) if segment.verbatim => {
            segment.original_start + (clean_offset - segment.clean_start)
        }
        Some(segment) => segment.original_start,
        None => map
            .last()
            .map_or(clean_offset, |segment| segment.original_end),
    }
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
