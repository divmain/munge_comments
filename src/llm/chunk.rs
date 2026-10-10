//! Large-file chunking: splitting a file's original text into chunks that
//! each fit the `--max-file-chars` budget, and building each chunk's prompt
//! text.
//!
//! [`chunk_file`] cuts along top-level declaration boundaries of the
//! **original** text (the root's named children), descending into
//! container bodies (class bodies, namespace/impl/module bodies, …) for
//! segments that are still too large, and packs the segments greedily.
//! Every boundary is a line start that cuts through neither a comment nor
//! a node, so the transforms' `range` contract holds and every comment
//! lies in exactly one chunk. [`build_chunk_texts`] then runs the redact or
//! remove transform over each chunk's original byte range: the text itself
//! is never sliced before transforming, so offsets stay original-text
//! offsets.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::artifact::types::{FileWarning, FileWarningKind};
use crate::comments::inventory::{Inventory, InventoryComment};
use crate::comments::redact::{
    Segment, TransformOpts, has_placeholder_collision, redact, remove_comments,
};
use crate::comments::scan::declaration_name;
use crate::config::Mode;
use crate::errors::Error;
use crate::lang::adapter::{AnchorWrapper, LanguageAdapter};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::lang::registry;
use crate::lang::types::{PlainNode, first_child_with_field};

/// The longest outline line kept, in Unicode scalar values; longer lines
/// (minified code) are cut and end in `…`, so the outline can never
/// duplicate a one-line file into every chunk prompt.
const OUTLINE_LINE_MAX_CHARS: usize = 160;

/// One chunk of a file: a range of the original text, cut at line
/// boundaries that split neither a comment nor a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 0-based position of the chunk in the file's chunk order.
    pub index: usize,
    /// Start byte offset in the original text (absolute).
    pub original_start: usize,
    /// End byte offset in the original text (absolute, exclusive).
    pub original_end: usize,
    /// The ids of the inventory records lying inside the chunk, in
    /// ascending offset order.
    pub comment_ids: Vec<String>,
    /// The file outline (one line per top-level node) carried by every
    /// chunk prompt of a chunked file; empty for a file within the budget,
    /// whose single chunk shows the whole file anyway.
    pub outline: String,
}

/// A chunk together with the prompt text the model sees for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkText {
    /// The chunk.
    pub chunk: Chunk,
    /// The redacted (redact mode) or clean (remove mode) text of the
    /// chunk's original range.
    pub prompt_text: String,
    /// Where the chunk starts in the original text.
    pub original_base: usize,
    /// Remove mode: the clean-text → original-text segments of this chunk
    /// (absolute original offsets). `None` in redact mode.
    pub offset_map: Option<Vec<Segment>>,
    /// Redact mode: each placeholder id in this chunk → its record's
    /// original byte range. `None` in remove mode.
    pub id_map: Option<BTreeMap<String, Range<usize>>>,
}

/// A cut of some span: a byte range plus the segment starters it holds.
struct Piece<'tree> {
    range: Range<usize>,
    starters: Vec<&'tree PlainNode>,
}

/// A packable unit after descent: a byte range, flagged when it is over
/// the budget and could not be split further.
struct Leaf {
    range: Range<usize>,
    unsplittable: bool,
}

/// The shared inputs of segmentation.
struct Ctx<'a> {
    text: &'a str,
    adapter: &'a LanguageAdapter,
    comments: &'a [InventoryComment],
    max_chars: usize,
}

/// Splits `text` (inventoried as `inventory` under `language`) into chunks
/// of at most `max_chars` Unicode scalar values each, per the Chunking
/// rule:
///
/// 1. **Segments.** The root's named children start segments (comment
///    nodes and prefix-sibling wrappers such as Rust attributes do not:
///    they travel with the node they precede). Each segment ends at the
///    start of the line after its node and that node's same-line trailing
///    comments; two nodes sharing a line share a segment.
/// 2. **Descent.** A segment over the budget whose node has a container
///    body (after unwrapping the adapter's field wrappers) is re-cut at the
///    body's children by the same rule, recursively. The text before the
///    first child (the header) travels with the first sub-segment and the
///    text after the last child (the closing brace) with the last.
/// 3. **Packing.** Segments are packed greedily, in order, while a chunk's
///    length stays within the budget. A segment still over the budget is
///    sent whole as its own chunk, with an `oversized-chunk` warning.
/// 4. **EOF tail.** The text after the last top-level node (dangling EOF
///    comments) always joins the last chunk, even past the budget.
///
/// A file within the budget yields one chunk covering it, without an
/// outline. A chunked file's chunks all carry the same outline.
///
/// With `range`, only segments intersecting it are packed, each clamped
/// to it, and offsets stay absolute: this re-splits one chunk (at a
/// smaller budget) without slicing the text. The chunks then cover exactly
/// `range`.
///
/// # Errors
///
/// [`Error::Invariant`] for a zero budget or a `range` that is not a
/// char-boundary range of `text`; any error of the grammar registry.
pub fn chunk_file(
    text: &str,
    language: LanguageId,
    inventory: &Inventory,
    max_chars: usize,
    range: Option<Range<usize>>,
) -> Result<(Vec<Chunk>, Vec<FileWarning>), Error> {
    if max_chars == 0 {
        return Err(Error::Invariant("chunk budget must be at least 1".into()));
    }
    let span = range.unwrap_or(0..text.len());
    if !(span.start <= span.end
        && span.end <= text.len()
        && text.is_char_boundary(span.start)
        && text.is_char_boundary(span.end))
    {
        return Err(Error::Invariant(format!(
            "chunk range {span:?} is not a char-boundary range of the text"
        )));
    }
    if char_len(text, 0..text.len()) <= max_chars {
        let chunk = make_chunk(0, span, inventory, String::new());
        return Ok((vec![chunk], Vec::new()));
    }

    let adapter = adapter_for(language);
    let tree = registry::parse(text, language)?;
    let ctx = Ctx {
        text,
        adapter,
        comments: &inventory.comments,
        max_chars,
    };
    let root_starters = starters(&tree.root, adapter);
    let outline = outline(&ctx, &root_starters);

    // The root level: segments up to the last starter's cut, then the
    // EOF tail.
    let tail_start = match root_starters.last() {
        Some(last) => Some(ctx.cut_after(last.end))
            .filter(|&cut| ctx.cuttable(cut))
            .unwrap_or(text.len()),
        // No code at all: the whole file is the tail.
        None => 0,
    };
    let mut leaves = Vec::new();
    for piece in ctx.pieces(&root_starters, 0..tail_start) {
        ctx.descend(piece, &mut leaves);
    }

    // Packing, restricted to the requested span.
    let mut chunks: Vec<Range<usize>> = Vec::new();
    let mut warnings = Vec::new();
    let mut current: Option<(Range<usize>, usize)> = None;
    for leaf in leaves {
        let Some(range) = clamp(&leaf.range, &span) else {
            continue;
        };
        let length = char_len(text, range.clone());
        if leaf.unsplittable && length > max_chars {
            chunks.extend(current.take().map(|(range, _)| range));
            warnings.push(ctx.oversized_warning(chunks.len(), &range, length));
            chunks.push(range);
            continue;
        }
        current = match current.take() {
            Some((open, open_len)) if open_len + length <= max_chars => {
                Some((open.start..range.end, open_len + length))
            }
            Some((open, _)) => {
                chunks.push(open);
                Some((range, length))
            }
            None => Some((range, length)),
        };
    }
    chunks.extend(current.map(|(range, _)| range));
    if let Some(tail) = clamp(&(tail_start..text.len()), &span) {
        match chunks.last_mut() {
            Some(last) => last.end = tail.end,
            None => chunks.push(tail),
        }
    }
    if chunks.is_empty() {
        chunks.push(span);
    }

    let chunks = chunks
        .into_iter()
        .enumerate()
        .map(|(index, range)| make_chunk(index, range, inventory, outline.clone()))
        .collect();
    Ok((chunks, warnings))
}

/// A chunk over `range`, listing the inventory records inside it.
fn make_chunk(index: usize, range: Range<usize>, inventory: &Inventory, outline: String) -> Chunk {
    let comment_ids = inventory
        .comments
        .iter()
        .filter(|comment| range.start <= comment.start && comment.end <= range.end)
        .map(|comment| comment.id.clone())
        .collect();
    Chunk {
        index,
        original_start: range.start,
        original_end: range.end,
        comment_ids,
        outline,
    }
}

/// `range ∩ span`, when non-empty.
fn clamp(range: &Range<usize>, span: &Range<usize>) -> Option<Range<usize>> {
    let clamped = range.start.max(span.start)..range.end.min(span.end);
    (clamped.start < clamped.end).then_some(clamped)
}

/// The length of `text[range]` in Unicode scalar values.
fn char_len(text: &str, range: Range<usize>) -> usize {
    text[range].chars().count()
}

/// The named children of `node` that start segments: everything but
/// comment nodes and prefix-sibling wrappers (which travel with the node
/// they precede).
fn starters<'tree>(node: &'tree PlainNode, adapter: &LanguageAdapter) -> Vec<&'tree PlainNode> {
    node.children
        .iter()
        .filter(|child| {
            child.named
                && !adapter.comment_node_kinds.contains(&child.kind)
                && !adapter.anchor_wrappers.iter().any(|wrapper| {
                    matches!(wrapper, AnchorWrapper::PrefixSibling { kind } if *kind == child.kind)
                })
        })
        .collect()
}

impl<'tree> Ctx<'_> {
    /// The cut after a node ending at `end`: the start of the line after
    /// `end` extended over the node's same-line trailing comment records
    /// (a record starting on that line is absorbed whole, and the line it
    /// ends on is then checked again), or the end of the text.
    fn cut_after(&self, mut end: usize) -> usize {
        loop {
            let line_end = self.text[end..]
                .find('\n')
                .map_or(self.text.len(), |at| end + at);
            let first = self.comments.partition_point(|comment| comment.start < end);
            match self.comments.get(first) {
                Some(comment) if comment.start < line_end => end = comment.end,
                _ => return (line_end + 1).min(self.text.len()),
            }
        }
    }

    /// Whether `at` cuts through no inventory record.
    fn cuttable(&self, at: usize) -> bool {
        let first = self.comments.partition_point(|comment| comment.end <= at);
        self.comments
            .get(first)
            .is_none_or(|comment| comment.start >= at)
    }

    /// Cuts `span` between consecutive `starters`: at the line after each
    /// starter (and its same-line trailing comments), provided the cut
    /// falls strictly inside the span, at or before the next starter, and
    /// through no comment. Text before the first cut belongs to the first
    /// piece, text after the last cut to the last.
    fn pieces(&self, starters: &[&'tree PlainNode], span: Range<usize>) -> Vec<Piece<'tree>> {
        let mut pieces: Vec<Piece<'tree>> = Vec::new();
        let mut open = Piece {
            range: span.start..span.end,
            starters: Vec::new(),
        };
        for (index, starter) in starters.iter().enumerate() {
            open.starters.push(starter);
            let Some(next) = starters.get(index + 1) else {
                break;
            };
            let cut = self.cut_after(starter.end);
            if span.start < cut && cut < span.end && cut <= next.start && self.cuttable(cut) {
                let rest = Piece {
                    range: cut..span.end,
                    starters: Vec::new(),
                };
                let mut done = std::mem::replace(&mut open, rest);
                done.range.end = cut;
                pieces.push(done);
            }
        }
        pieces.push(open);
        pieces
    }

    /// Appends the leaves of `piece` to `out`: the piece itself when it
    /// fits the budget, else its sub-pieces (recursively) when its single
    /// starter has a container body with at least one cut, else the piece
    /// flagged unsplittable.
    fn descend(&self, piece: Piece<'tree>, out: &mut Vec<Leaf>) {
        if char_len(self.text, piece.range.clone()) <= self.max_chars {
            out.push(Leaf {
                range: piece.range,
                unsplittable: false,
            });
            return;
        }
        let sub_pieces = match piece.starters.as_slice() {
            [starter] => self.body(starter).map(|body| {
                let children = starters(body, self.adapter);
                self.pieces(&children, piece.range.clone())
            }),
            _ => None,
        };
        match sub_pieces {
            Some(sub_pieces) if sub_pieces.len() > 1 => {
                for sub_piece in sub_pieces {
                    self.descend(sub_piece, out);
                }
            }
            _ => out.push(Leaf {
                range: piece.range,
                unsplittable: true,
            }),
        }
    }

    /// The container body of `node`: after unwrapping the adapter's field
    /// wrappers (`export_statement` → its declaration, Python's
    /// `decorated_definition` → its definition), the first direct child
    /// whose kind is a container kind.
    fn body(&self, mut node: &'tree PlainNode) -> Option<&'tree PlainNode> {
        while let Some(inner) =
            self.adapter
                .anchor_wrappers
                .iter()
                .find_map(|wrapper| match *wrapper {
                    AnchorWrapper::Field { kind, field } if node.kind == kind => {
                        first_child_with_field(node, field)
                    }
                    _ => None,
                })
        {
            node = inner;
        }
        node.children
            .iter()
            .find(|child| self.adapter.container_kinds.contains(&child.kind))
    }

    /// The `oversized-chunk` warning for chunk `index` over `range`.
    fn oversized_warning(&self, index: usize, range: &Range<usize>, length: usize) -> FileWarning {
        let first_line = line_of(self.text, range.start);
        let last_line = line_of(self.text, range.end.saturating_sub(1).max(range.start));
        FileWarning {
            kind: FileWarningKind::OversizedChunk,
            detail: format!(
                "chunk {index} (lines {first_line}-{last_line}, {length} chars) exceeds the \
                 {}-char budget and has no container body to split at; sent whole",
                self.max_chars
            ),
        }
    }
}

/// The 1-based line holding byte `offset`.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// The file outline: one line per root starter, its declaration name (if
/// any) and its first source line from the node's start, with every
/// comment range excised (hidden prose never leaks through the outline),
/// whitespace-trimmed and capped at [`OUTLINE_LINE_MAX_CHARS`].
fn outline(ctx: &Ctx<'_>, root_starters: &[&PlainNode]) -> String {
    let mut lines = Vec::with_capacity(root_starters.len());
    for node in root_starters {
        let line_end = ctx.text[node.start..]
            .find('\n')
            .map_or(ctx.text.len(), |at| node.start + at);
        let mut kept = String::new();
        let mut cursor = node.start;
        let first = ctx
            .comments
            .partition_point(|comment| comment.end <= node.start);
        for comment in &ctx.comments[first..] {
            if comment.start >= line_end {
                break;
            }
            let from = comment.start.max(cursor);
            if from > cursor {
                kept.push_str(&ctx.text[cursor..from]);
            }
            cursor = cursor.max(comment.end.min(line_end));
        }
        kept.push_str(&ctx.text[cursor.min(line_end)..line_end]);
        let source = cap(kept.trim());
        let line = match declaration_name(node, ctx.adapter, ctx.text) {
            Some(name) => format!("{name}: {source}"),
            None => source,
        };
        lines.push(line);
    }
    lines.join("\n")
}

/// `line` cut to [`OUTLINE_LINE_MAX_CHARS`] scalar values, ending in `…`
/// when cut.
fn cap(line: &str) -> String {
    match line.char_indices().nth(OUTLINE_LINE_MAX_CHARS) {
        Some((at, _)) => format!("{}…", line[..at].trim_end()),
        None => line.to_string(),
    }
}

/// Builds each chunk's prompt text by running the `mode` transform over
/// the chunk's original byte range (with `targets`, as for the whole
/// file): redact mode yields the blind text and its id map, remove mode
/// the clean text and its offset map (absolute original offsets, from
/// `original_base`). Chunk ranges never cut a comment, so each chunk's
/// prompt text hides exactly the targets among its own records, and a
/// remove-mode chunk's clean text is exactly the matching slice of the
/// whole-file clean text.
///
/// `mode` is the file's *effective* mode: a file whose text collides with
/// the placeholder syntax is processed in remove mode.
///
/// # Errors
///
/// [`Error::Invariant`] (a caller bug) for redact mode on a text with a
/// placeholder collision ([`has_placeholder_collision`]), or for a chunk
/// whose range is not a char-boundary range of `text`.
pub fn build_chunk_texts(
    text: &str,
    inventory: &Inventory,
    adapter: &LanguageAdapter,
    chunks: &[Chunk],
    mode: Mode,
    targets: Option<&BTreeSet<String>>,
) -> Result<Vec<ChunkText>, Error> {
    if mode == Mode::Redact && has_placeholder_collision(text) {
        return Err(Error::Invariant(
            "redact mode requested for a file with a placeholder collision; \
             its effective mode is remove"
                .into(),
        ));
    }
    if let Some(chunk) = chunks.iter().find(|chunk| {
        !(chunk.original_start <= chunk.original_end
            && chunk.original_end <= text.len()
            && text.is_char_boundary(chunk.original_start)
            && text.is_char_boundary(chunk.original_end))
    }) {
        return Err(Error::Invariant(format!(
            "chunk {} range {}..{} is not a char-boundary range of the text",
            chunk.index, chunk.original_start, chunk.original_end
        )));
    }
    let texts = chunks
        .iter()
        .map(|chunk| {
            let opts = TransformOpts {
                targets,
                range: Some(chunk.original_start..chunk.original_end),
            };
            match mode {
                Mode::Redact => {
                    let output = redact(text, inventory, adapter, opts);
                    ChunkText {
                        chunk: chunk.clone(),
                        prompt_text: output.blind_text,
                        original_base: chunk.original_start,
                        offset_map: None,
                        id_map: Some(output.id_map),
                    }
                }
                Mode::Remove => {
                    let output = remove_comments(text, inventory, opts);
                    ChunkText {
                        chunk: chunk.clone(),
                        prompt_text: output.clean_text,
                        original_base: output.original_base,
                        offset_map: Some(output.offset_map),
                        id_map: None,
                    }
                }
            }
        })
        .collect();
    Ok(texts)
}

#[cfg(test)]
#[path = "chunk_tests.rs"]
mod tests;
