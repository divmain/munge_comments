//! Comment removal with line hygiene: the single implementation shared by
//! remove mode (the clean text the model sees) and apply's `REMOVE`, so
//! the two are byte-identical by construction.
//!
//! [`compute_removal_edits`] turns a set of comments to delete into
//! [`Edit`]s against the original text. The rules are pure text surgery,
//! keyed off the original text and language-agnostic:
//!
//! - **Code** is any non-whitespace text other than a comment being
//!   removed. A comment that stays — kept, protected, or replaced by a
//!   rewrite — counts as code, so no removal edit ever touches it.
//! - **Emptied lines.** A line left holding only whitespace is deleted
//!   together with its line terminator. Removals sharing a line are judged
//!   on their combined result, so edits never overlap.
//! - **Trailing comments** (`foo(); // c`) take the horizontal whitespace
//!   between the code and the comment with them; the line terminator is
//!   always preserved.
//! - **Inline comments** (`a /* c */ b`) are deleted together with the
//!   horizontal whitespace on their right, so the left-side separator
//!   survives. If that would make two non-whitespace characters touch, a
//!   single space is inserted instead — unless the pair can never merge
//!   into one token (left `( [ { ,`, or right `) ] } , ;`).
//! - **Line-break-preserving removal.** When a removed inline span holds a
//!   line terminator and code sits on both sides of it, the gap between
//!   those tokens keeps one line break (`return /*` + line break + `*/ x`
//!   becomes `return` + line break + `x`): a comment containing a line
//!   break ends the statement under JavaScript's ASI and Go's semicolon
//!   insertion, so joining the lines would change the program.
//! - **Blank-line runs** collapse to one blank line only where a deleted
//!   line sat inside or adjacent to the run (measured at its pre-deletion
//!   position); pre-existing runs elsewhere are untouched.
//!
//! Lines are `\n`-terminated (a preceding `\r` belongs to the terminator,
//! so CRLF files stay CRLF), and line 1 starts after a leading BOM, which
//! is preserved.

use std::ops::Range;

use super::inventory::InventoryComment;
use super::scan::line_start_of;
use super::types::Edit;

/// Left neighbours that never merge with the following token.
const NEVER_MERGE_LEFT: [char; 4] = ['(', '[', '{', ','];
/// Right neighbours that never merge with the preceding token.
const NEVER_MERGE_RIGHT: [char; 5] = [')', ']', '}', ',', ';'];

/// The edits deleting every comment in `to_remove` from `text`, with the
/// line-hygiene rules of the module documentation applied: ascending by
/// start, non-overlapping (adjacent edits may touch), each a deletion or a
/// replacement of a strictly longer span.
///
/// A group record is removed member by member: only its members' ranges
/// are comment text, and the line structure between them is judged by the
/// same rules as for lone comments.
///
/// # Panics
///
/// If a member range is empty, out of bounds, off a `char` boundary, or
/// overlaps another (inventory records never do).
pub fn compute_removal_edits(text: &str, to_remove: &[&InventoryComment]) -> Vec<Edit> {
    let spans = removal_spans(text, to_remove);
    let mut edits = Vec::new();
    let mut deleted = Vec::new();
    let mut rewritten = Vec::new();
    for block in blocks(text, &spans) {
        if block.is_emptied(text) {
            edits.push(deletion(block.start..block.end));
            deleted.push(block.start..block.end);
        } else {
            edits.extend(block.edits(text));
            rewritten.push(block.start..block.end);
        }
    }
    edits.extend(blank_run_edits(text, &deleted, &rewritten));
    edits.sort_by_key(|edit| edit.start);
    edits
}

/// Every member range of the records to remove, ascending and validated.
fn removal_spans(text: &str, to_remove: &[&InventoryComment]) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = to_remove
        .iter()
        .flat_map(|comment| comment.members.iter().cloned())
        .collect();
    spans.sort_by_key(|span| span.start);
    for span in &spans {
        assert!(
            span.start < span.end
                && span.end <= text.len()
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end),
            "removal span {span:?} is not a non-empty char-boundary range of the text"
        );
    }
    for pair in spans.windows(2) {
        assert!(
            pair[0].end <= pair[1].start,
            "removal spans {:?} and {:?} overlap",
            pair[0],
            pair[1]
        );
    }
    spans
}

/// A pure deletion of `range`.
fn deletion(range: Range<usize>) -> Edit {
    Edit {
        start: range.start,
        length: range.len(),
        replacement: String::new(),
    }
}

/// Whether `c` terminates a line for the line-break rule: the terminators
/// the semantic verifier's line-break-boundary invariant counts.
pub(crate) fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Whitespace that does not terminate a line.
fn is_horizontal_space(c: char) -> bool {
    c.is_whitespace() && !is_line_terminator(c)
}

fn is_blank(slice: &str) -> bool {
    slice.chars().all(char::is_whitespace)
}

/// The end of the line containing `offset`: `(content_end, end)`, where
/// `content_end` excludes the line terminator (`\n` or `\r\n`) and `end`
/// includes it (both `text.len()` on an unterminated last line).
fn line_end_from(text: &str, offset: usize) -> (usize, usize) {
    match text[offset..].find('\n') {
        Some(found) => {
            let newline = offset + found;
            let content_end = if newline > offset && text.as_bytes()[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };
            (content_end, newline + 1)
        }
        None => (text.len(), text.len()),
    }
}

/// The whole lines touched by a run of removal spans, where each span
/// after the first starts on the line the previous one ends on. Every line
/// break inside a block therefore lies inside a removal span, and the kept
/// text between spans never crosses a line.
struct Block {
    /// Start of the first line (after a leading BOM on line 1).
    start: usize,
    /// End of the last line's content (its terminator excluded).
    content_end: usize,
    /// End of the last line, terminator included.
    end: usize,
    /// The removal spans, ascending.
    spans: Vec<Range<usize>>,
}

/// Groups `spans` (ascending) into blocks.
fn blocks(text: &str, spans: &[Range<usize>]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for span in spans {
        let (content_end, end) = line_end_from(text, span.end);
        match blocks.last_mut() {
            Some(block) if span.start <= block.content_end => {
                block.content_end = content_end;
                block.end = end;
                block.spans.push(span.clone());
            }
            _ => blocks.push(Block {
                start: line_start_of(text, span.start),
                content_end,
                end,
                spans: vec![span.clone()],
            }),
        }
    }
    blocks
}

impl Block {
    /// The kept (non-removed) pieces of the block's text, in order: before
    /// the first span, between consecutive spans, after the last span.
    fn kept<'t>(&self, text: &'t str) -> impl Iterator<Item = &'t str> {
        let starts = std::iter::once(self.start).chain(self.spans.iter().map(|span| span.end));
        let ends = self
            .spans
            .iter()
            .map(|span| span.start)
            .chain(std::iter::once(self.content_end));
        starts.zip(ends).map(move |(start, end)| &text[start..end])
    }

    /// Whether the block holds no code once its spans are removed.
    fn is_emptied(&self, text: &str) -> bool {
        self.kept(text).all(is_blank)
    }

    /// The maximal runs of spans separated only by whitespace, as
    /// `start..end` of the run: each is removed as one unit.
    fn clusters(&self, text: &str) -> Vec<Range<usize>> {
        let mut clusters: Vec<Range<usize>> = Vec::new();
        for span in &self.spans {
            match clusters.last_mut() {
                Some(cluster) if is_blank(&text[cluster.end..span.start]) => {
                    cluster.end = span.end;
                }
                _ => clusters.push(span.clone()),
            }
        }
        clusters
    }

    /// The edits removing the spans of a block that keeps code. Clusters
    /// are separated by code, so only the first can lack code on its left
    /// and only the last can lack code on its right.
    fn edits(&self, text: &str) -> Vec<Edit> {
        let clusters = self.clusters(text);
        let last = clusters.len() - 1;
        clusters
            .iter()
            .enumerate()
            .map(|(index, cluster)| {
                let code_left = index > 0 || !is_blank(&text[self.start..cluster.start]);
                let code_right = index < last || !is_blank(&text[cluster.end..self.content_end]);
                // Code on the left ends at `code_end`; code on the right
                // starts at `code_start`, past the right-side whitespace.
                let code_end = text[..cluster.start]
                    .trim_end_matches(is_horizontal_space)
                    .len();
                let code_start = text.len()
                    - text[cluster.end..]
                        .trim_start_matches(is_horizontal_space)
                        .len();
                match (code_left, code_right) {
                    // Trailing: the whitespace before the comment and any
                    // after it go too; the line terminator stays.
                    (true, false) => deletion(code_end..self.content_end),
                    (true, true) => inline_edit(text, cluster, code_end, code_start),
                    // Leading (a block that keeps code has code beside
                    // every cluster): the indentation stays.
                    (false, _) => deletion(cluster.start..code_start),
                }
            })
            .collect()
    }
}

/// The edit removing a cluster with code on both sides of it on its line.
fn inline_edit(text: &str, cluster: &Range<usize>, code_end: usize, code_start: usize) -> Edit {
    let removed = &text[cluster.clone()];
    if removed.contains(is_line_terminator) {
        // Keep one line break between the two tokens: the left line ends
        // like a line whose trailing comment was removed, and the right
        // line starts like one whose leading comment was removed.
        let replacement = format!(
            "{}{}",
            kept_line_break(text, cluster),
            last_line_indent(removed)
        );
        return Edit {
            start: code_end,
            length: code_start - code_end,
            replacement,
        };
    }
    let left = text[..cluster.start].chars().next_back();
    let right = text[code_start..].chars().next();
    let replacement = match (left, right) {
        (Some(left), Some(right)) if touch_needs_space(left, right) => " ",
        _ => "",
    };
    Edit {
        start: cluster.start,
        length: code_start - cluster.start,
        replacement: replacement.to_string(),
    }
}

/// Whether joining `left` and `right` directly could merge two tokens.
fn touch_needs_space(left: char, right: char) -> bool {
    !left.is_whitespace()
        && !right.is_whitespace()
        && !NEVER_MERGE_LEFT.contains(&left)
        && !NEVER_MERGE_RIGHT.contains(&right)
}

/// The line break kept for a cluster spanning lines: its last `\n`-based
/// line break as written (`\r\n` or `\n`). A cluster whose only line
/// terminators are lone `\r`, U+2028 or U+2029 gets the file's line-break
/// style instead.
fn kept_line_break(text: &str, cluster: &Range<usize>) -> &'static str {
    match text[cluster.clone()].rfind('\n') {
        Some(newline) => crlf_or_lf(text, cluster.start + newline, cluster.start),
        None => match text.find('\n') {
            Some(newline) => crlf_or_lf(text, newline, 0),
            None => "\n",
        },
    }
}

/// `\r\n` when the `\n` at `newline` is preceded by a `\r` at or after
/// `floor`, else `\n`.
fn crlf_or_lf(text: &str, newline: usize, floor: usize) -> &'static str {
    if newline > floor && text.as_bytes()[newline - 1] == b'\r' {
        "\r\n"
    } else {
        "\n"
    }
}

/// The leading horizontal whitespace of the removed text's last line (the
/// indentation that line had); empty when the removed text has no `\n`.
fn last_line_indent(removed: &str) -> &str {
    match removed.rfind('\n') {
        Some(newline) => {
            let last_line = &removed[newline + 1..];
            let indent = last_line.len() - last_line.trim_start_matches(is_horizontal_space).len();
            &last_line[..indent]
        }
        None => "",
    }
}

/// How a line of the original text fares under the removal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineFate {
    /// Deleted: it held only whitespace and removed comments.
    Deleted,
    /// Whitespace-only and untouched by any removal.
    Blank,
    /// Anything else: code, or a line a removal rewrites.
    Other,
}

/// The edits collapsing each run of blank lines that a deleted line sat
/// inside or adjacent to (both measured on the original text) to its
/// first blank line. `deleted` and `rewritten` are the ascending,
/// line-aligned ranges of the emptied and the rewritten blocks.
fn blank_run_edits(text: &str, deleted: &[Range<usize>], rewritten: &[Range<usize>]) -> Vec<Edit> {
    let mut edits = Vec::new();
    // The blank lines of the current run of blank-or-deleted lines, and
    // whether the run holds a deleted line.
    let mut run_blanks: Vec<Range<usize>> = Vec::new();
    let mut run_has_deletion = false;
    let mut flush = |blanks: &mut Vec<Range<usize>>, has_deletion: &mut bool| {
        if *has_deletion {
            edits.extend(blanks.iter().skip(1).cloned().map(deletion));
        }
        blanks.clear();
        *has_deletion = false;
    };
    let (mut next_deleted, mut next_rewritten) = (0, 0);
    // Line 1 starts after a leading BOM.
    let mut start = if text.starts_with('\u{FEFF}') {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    };
    while start < text.len() {
        let (content_end, end) = line_end_from(text, start);
        let fate = if advance_to(deleted, &mut next_deleted, start) {
            LineFate::Deleted
        } else if advance_to(rewritten, &mut next_rewritten, start) {
            LineFate::Other
        } else if is_blank(&text[start..content_end]) {
            LineFate::Blank
        } else {
            LineFate::Other
        };
        match fate {
            LineFate::Deleted => run_has_deletion = true,
            LineFate::Blank => run_blanks.push(start..end),
            LineFate::Other => flush(&mut run_blanks, &mut run_has_deletion),
        }
        start = end;
    }
    flush(&mut run_blanks, &mut run_has_deletion);
    edits
}

/// Whether the line starting at `offset` lies in one of the ascending
/// `ranges`, advancing the cursor `next` past ranges ending at or before
/// it (offsets are visited in ascending order).
fn advance_to(ranges: &[Range<usize>], next: &mut usize, offset: usize) -> bool {
    while ranges.get(*next).is_some_and(|range| range.end <= offset) {
        *next += 1;
    }
    ranges.get(*next).is_some_and(|range| range.start <= offset)
}

#[cfg(test)]
#[path = "hygiene_tests.rs"]
mod tests;
