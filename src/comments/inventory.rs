//! The inventory pipeline — the single entrypoint through which every
//! consumer (golden tests, both transforms, `plan`, `apply`'s firewall
//! re-scan, the verifier's re-classification) obtains a file's comments.
//!
//! [`inventory_file`] runs, in order: adapter lookup → [`scan_file`] →
//! [`classify_all`] → directive-adjacency protection → logical grouping →
//! id assignment → per-file style statistics. No consumer re-implements any
//! of these steps, so plan and apply always agree on ranges, groups and
//! classifications. The pipeline is synchronous and deterministic: a pure
//! function of `(text, file)`.

use std::collections::BTreeSet;
use std::ops::Range;

use serde::Serialize;

use crate::errors::Error;
use crate::lang::adapter::{LanguageAdapter, Scope};
use crate::lang::adapters::adapter_for;

use super::classify::{ClassifiedComment, ClassifyCtx, classify_all};
use super::scan::{line_start_of, scan_file};
use super::types::{Anchor, Classification, CommentKind, DeclInfo, Position};

pub use super::types::FileRef;

/// The tag directive-adjacency protection adds.
const DIRECTIVE_ADJACENT: &str = "directive-adjacent";
/// The tag that makes a prose comment non-editable (sole content of a JSX
/// `{/* … */}` container).
const JSX_EXPRESSION: &str = "jsx-expression";

/// One inventoried comment record: a classified comment (or a logical
/// group of adjacent line comments) with its stable id.
///
/// For a group, the fields are derived per the Design: `text` is the
/// source slice from the first member's opener to the last member's end;
/// `line`/`column` (and `prev_code_end`, `enclosing_top_level`) come from
/// the first member; `anchor`, `anchor_exported`, `position` and
/// `next_code_start` from the last (the group annotates what follows it);
/// `tags` are the union of the members' tags; `kind` is `line`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryComment {
    /// `C1`, `C2`, … in ascending offset order, assigned after grouping
    /// (gap-free).
    pub id: String,
    /// Syntactic kind (`line`/`block`/`doc`; always `line` for a group).
    pub kind: CommentKind,
    /// Start byte offset (a group's first member's opener).
    pub start: usize,
    /// End byte offset, exclusive (a group's last member's end).
    pub end: usize,
    /// 1-based line of `start`.
    pub line: u32,
    /// 1-based column of `start`, in Unicode scalar values.
    pub column: u32,
    /// Raw text `source[start..end]`, delimiters included.
    pub text: String,
    /// Leading / trailing / dangling.
    pub position: Position,
    /// The code this comment annotates.
    pub anchor: Anchor,
    /// Whether the anchor declaration is exported (JavaScript).
    pub anchor_exported: bool,
    /// Whether this comment is the sole content of a JSX `{/* … */}`.
    pub jsx_container_sole: bool,
    /// End of the last code token before this comment.
    pub prev_code_end: Option<usize>,
    /// Start of the first code token after this comment (`text.len()` when
    /// none).
    pub next_code_start: usize,
    /// The root's named child strictly containing this comment, if any.
    pub enclosing_top_level: Option<DeclInfo>,
    /// `functional` / `prose` / `doc-protected`, after adjacency
    /// protection.
    pub classification: Classification,
    /// Every tag this comment (or any group member) earned.
    pub tags: BTreeSet<String>,
    /// The most specific scope among matched directive rows.
    pub directive_scope: Option<Scope>,
    /// The largest line window among matched line-window rows.
    pub directive_line_window: Option<usize>,
    /// Each member's raw range (one entry for a lone comment; more only
    /// for groups).
    pub members: Vec<Range<usize>>,
    /// `prose` and not a JSX-container comment: the only records that are
    /// redacted, shown to the LLM as editable, and removable/rewritable.
    pub editable: bool,
}

/// Per-file comment-style statistics over the editable prose records,
/// injected into prompts (`prevailing`) and used for the kind of NEW
/// records (`prevailing_new`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleStats {
    /// Editable `line` records (a group counts once).
    pub line: u32,
    /// Editable `block` records.
    pub block: u32,
    /// Editable `doc` records.
    pub doc: u32,
    /// Mean char count (Unicode scalar values) of the records' raw text;
    /// `0.0` when there are none.
    pub avg_length: f64,
    /// Whether the language has line comments — decides the style of a
    /// file without editable prose.
    pub language_has_line_comments: bool,
}

impl StyleStats {
    /// The prevailing style: the argmax of the kind counts, ties broken
    /// `line` > `block` > `doc`. With no editable prose, the language's
    /// line style if it has one, else `block`.
    pub fn prevailing(&self) -> CommentKind {
        if self.line == 0 && self.block == 0 && self.doc == 0 {
            return self.language_default();
        }
        argmax(
            (CommentKind::Line, self.line),
            &[
                (CommentKind::Block, self.block),
                (CommentKind::Doc, self.doc),
            ],
        )
    }

    /// The prevailing style for NEW records: the argmax over the `line`
    /// and `block` counts only — never `doc` — with ties going to `line`;
    /// with neither present, the language's line style if it has one, else
    /// `block`.
    pub fn prevailing_new(&self) -> CommentKind {
        if self.line == 0 && self.block == 0 {
            return self.language_default();
        }
        argmax(
            (CommentKind::Line, self.line),
            &[(CommentKind::Block, self.block)],
        )
    }

    fn language_default(&self) -> CommentKind {
        if self.language_has_line_comments {
            CommentKind::Line
        } else {
            CommentKind::Block
        }
    }

    /// Statistics over `comments`' editable records.
    fn over(comments: &[InventoryComment], adapter: &LanguageAdapter) -> Self {
        let mut stats = Self {
            line: 0,
            block: 0,
            doc: 0,
            avg_length: 0.0,
            language_has_line_comments: !adapter.line_prefixes.is_empty(),
        };
        let mut total_chars = 0usize;
        let mut count = 0usize;
        for comment in comments.iter().filter(|comment| comment.editable) {
            match comment.kind {
                CommentKind::Line => stats.line += 1,
                CommentKind::Block => stats.block += 1,
                CommentKind::Doc => stats.doc += 1,
            }
            total_chars += comment.text.chars().count();
            count += 1;
        }
        if count > 0 {
            stats.avg_length = total_chars as f64 / count as f64;
        }
        stats
    }
}

/// The first kind with the maximal count (`first`, then `rest`, are in
/// tie-break order: a later candidate wins only with a strictly larger
/// count).
fn argmax(first: (CommentKind, u32), rest: &[(CommentKind, u32)]) -> CommentKind {
    let mut best = first;
    for &candidate in rest {
        if candidate.1 > best.1 {
            best = candidate;
        }
    }
    best.0
}

/// A file's complete comment inventory.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    /// Every comment record (shebang excluded), in ascending offset order,
    /// with gap-free ids.
    pub comments: Vec<InventoryComment>,
    /// Whether the file starts with a shebang line (excluded from records,
    /// byte-preserved by construction).
    pub has_shebang: bool,
    /// Style statistics over the editable prose records.
    pub style_stats: StyleStats,
}

/// Inventories `text` under `file.language`: scan, classify, protect
/// directive-adjacent prose, group, assign ids and compute style
/// statistics. The language is always passed explicitly (from discovery at
/// plan time, from the artifact at apply time).
pub fn inventory_file(text: &str, file: FileRef) -> Result<Inventory, Error> {
    // 1. Adapter lookup.
    let adapter = adapter_for(file.language);
    let language = file.language;
    // 2. Scan (ranges, kinds, anchors).
    let file_name = file.file_name.clone();
    let scanned = scan_file(text, file)?;
    // 3. Classify (whole-file context).
    let mut classified = classify_all(
        scanned.comments,
        ClassifyCtx {
            text,
            file_name: &file_name,
            language,
            has_shebang: scanned.has_shebang,
        },
    )?;
    // 4. Directive-adjacency protection.
    protect_directive_adjacent(&mut classified);
    // 5. Logical grouping; 6. ids, after grouping so they are gap-free.
    let comments: Vec<InventoryComment> = group(classified, text)
        .into_iter()
        .enumerate()
        .map(|(index, members)| merge(members, index + 1, text))
        .collect();
    // 7. Per-file style statistics.
    let style_stats = StyleStats::over(&comments, adapter);
    Ok(Inventory {
        comments,
        has_shebang: scanned.has_shebang,
        style_stats,
    })
}

/// Reclassifies every prose comment that a next-scoped directive could be
/// re-targeted across as `functional` + `directive-adjacent`:
///
/// - for a directive with `directive_scope == Some(Next)`, every prose
///   comment starting between the directive and its `next_code_start`
///   (the first code token after it);
/// - for a line-window directive (`c8 ignore next N`), every prose comment
///   whose **start** line lies in the physical lines
///   `line + 1 ..= line + N` after the directive's line (blank and comment
///   lines included).
///
/// Comments are in ascending offset order and never overlap, so each scan
/// starts right after the directive and stops at the first comment past
/// its reach. Coverage is computed from the classifier's output alone
/// (newly protected comments are not directives themselves).
fn protect_directive_adjacent(comments: &mut [ClassifiedComment]) {
    let mut covered = vec![false; comments.len()];
    for (index, directive) in comments.iter().enumerate() {
        let following = comments.iter().enumerate().skip(index + 1);
        if directive.directive_scope == Some(Scope::Next) {
            for (position, _) in following
                .clone()
                .take_while(|(_, comment)| comment.start < directive.next_code_start)
            {
                covered[position] = true;
            }
        }
        if let Some(window) = directive.directive_line_window {
            let first = directive.line as usize + 1;
            let last = directive.line as usize + window;
            for (position, comment) in
                following.take_while(|(_, comment)| (comment.line as usize) <= last)
            {
                if comment.line as usize >= first {
                    covered[position] = true;
                }
            }
        }
    }
    for (comment, covered) in comments.iter_mut().zip(covered) {
        if covered && comment.classification == Classification::Prose {
            comment.classification = Classification::Functional;
            comment.tags.insert(DIRECTIVE_ADJACENT.to_string());
        }
    }
}

/// Whether a classified comment is editable: prose, and not the sole
/// content of a JSX expression container.
fn is_editable(classification: Classification, tags: &BTreeSet<String>) -> bool {
    classification == Classification::Prose && !tags.contains(JSX_EXPRESSION)
}

/// Whether `comment` may be a member of a logical group: an editable
/// `line`-kind comment.
fn groupable(comment: &ClassifiedComment) -> bool {
    comment.kind == CommentKind::Line && is_editable(comment.classification, &comment.tags)
}

/// Whether `next` continues the group ending in `previous`: both are
/// groupable, `previous` is preceded on its line only by horizontal
/// whitespace (its indent — so a trailing comment never starts or extends
/// a group), and the text between them is exactly one line terminator
/// followed by that same indent. Adjacent lines, the same indent column,
/// and the absence of anything else in between (a blank line, code, an
/// indent change, a trailing comment) all follow.
fn continues_group(previous: &ClassifiedComment, next: &ClassifiedComment, text: &str) -> bool {
    if !groupable(previous) || !groupable(next) {
        return false;
    }
    let line_start = line_start_of(text, previous.start);
    let indent = &text[line_start..previous.start];
    if !indent
        .chars()
        .all(|character| matches!(character, ' ' | '\t'))
    {
        return false;
    }
    let between = &text[previous.end..next.start];
    let after_terminator = between
        .strip_prefix("\r\n")
        .or_else(|| between.strip_prefix('\n'));
    after_terminator == Some(indent)
}

/// A run of comments forming one record: a lone comment (`rest` empty) or
/// a logical group (`first` followed by `rest`).
struct Run {
    first: ClassifiedComment,
    rest: Vec<ClassifiedComment>,
}

impl Run {
    fn last(&self) -> &ClassifiedComment {
        self.rest.last().unwrap_or(&self.first)
    }
}

/// Partitions the classified comments (ascending offset order) into runs:
/// a logical group of adjacent editable line comments, or a lone comment.
fn group(comments: Vec<ClassifiedComment>, text: &str) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for comment in comments {
        if let Some(run) = runs.last_mut()
            && continues_group(run.last(), &comment, text)
        {
            run.rest.push(comment);
            continue;
        }
        runs.push(Run {
            first: comment,
            rest: Vec::new(),
        });
    }
    runs
}

/// Builds the record for a run with id `C{number}`. A lone comment keeps
/// its fields; a group's fields are derived per the Design (see
/// [`InventoryComment`]).
fn merge(run: Run, number: usize, text: &str) -> InventoryComment {
    let id = format!("C{number}");
    let Run { first, rest } = run;
    let members: Vec<Range<usize>> = std::iter::once(&first)
        .chain(&rest)
        .map(|member| member.start..member.end)
        .collect();
    let Some(last) = rest.last() else {
        let editable = is_editable(first.classification, &first.tags);
        return InventoryComment {
            id,
            kind: first.kind,
            start: first.start,
            end: first.end,
            line: first.line,
            column: first.column,
            text: first.text,
            position: first.position,
            anchor: first.anchor,
            anchor_exported: first.anchor_exported,
            jsx_container_sole: first.jsx_container_sole,
            prev_code_end: first.prev_code_end,
            next_code_start: first.next_code_start,
            enclosing_top_level: first.enclosing_top_level,
            classification: first.classification,
            tags: first.tags,
            directive_scope: first.directive_scope,
            directive_line_window: first.directive_line_window,
            members,
            editable,
        };
    };
    // Every member is editable prose (the grouping precondition), so the
    // group is too, and no member carries a directive scope or window.
    let mut tags = first.tags;
    for member in &rest {
        tags.extend(member.tags.iter().cloned());
    }
    InventoryComment {
        id,
        kind: CommentKind::Line,
        start: first.start,
        end: last.end,
        line: first.line,
        column: first.column,
        text: text[first.start..last.end].to_string(),
        position: last.position,
        anchor: last.anchor.clone(),
        anchor_exported: last.anchor_exported,
        jsx_container_sole: false,
        prev_code_end: first.prev_code_end,
        next_code_start: last.next_code_start,
        enclosing_top_level: first.enclosing_top_level,
        classification: Classification::Prose,
        editable: is_editable(Classification::Prose, &tags),
        tags,
        directive_scope: None,
        directive_line_window: None,
        members,
    }
}

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod tests;
