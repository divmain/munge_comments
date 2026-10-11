//! Remove-mode anchor resolution and alignment: pairs the model's cited
//! regenerations with the file's target comments, and validates the
//! insertion points of the regenerations no original claims (`NEW`
//! candidates).
//!
//! A pure module: no I/O, no LLM, no clock. Every step follows the
//! Design's "Anchor resolution & alignment" rules:
//!
//! 1. **Resolution.** The whole-file clean text (`remove_comments` over
//!    all of the file's targets, no range) is built and parsed once; chunk
//!    texts are never parsed, since a sub-chunk of a class body is no
//!    standalone program. A citation from chunk *k* resolves inside chunk
//!    *k*'s clean range of that text: by `(name, nodeKind)` over
//!    [`list_declarations`], else by the first occurrence of the snippet's
//!    first non-blank line, with the nearest-to-snippet rule between
//!    several declaration matches and the `end_of_file` rule (last chunk
//!    only). Unresolvable citations are dropped with an
//!    `unresolvable-anchor` warning. A whole-file clean parse that is
//!    `has_error`/`too_deep` degrades to the snippet path. Resolved clean
//!    positions map back through the whole-file `map_clean_offset`.
//! 2. **Pairing.** Candidates are the file's (editable) target records.
//!    The acceptance rules gate each (citation, original) pair; acceptable
//!    pairs are scored and assigned greedily one-to-one, ties broken by
//!    position compatibility, offset distance, citation index and
//!    original id.
//! 3. **NEW candidates.** Unpaired resolved citations become insertion
//!    candidates in the original text, validated against token gaps, the
//!    shebang/BOM edges, functional-comment windows, slot occupancy and
//!    one-per-slot deduplication; invalid ones are dropped with an
//!    `invalid-insertion` warning.

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use crate::artifact::types::{FileWarning, FileWarningKind};
use crate::comments::classify::cgo_doc_targets;
use crate::comments::inventory::{Inventory, InventoryComment};
use crate::comments::redact::{Segment, TransformOpts, map_clean_offset, remove_comments};
use crate::comments::sanitize::normalize_comment_text;
use crate::comments::scan::{is_shebang_node, line_start_of, list_declarations};
use crate::comments::types::{Anchor, Classification, DeclInfo, Position};
use crate::errors::Error;
use crate::lang::adapter::{AnchorWrapper, LanguageAdapter, PositionalRuleId, Scope};
use crate::lang::ids::LanguageId;
use crate::lang::parse;
use crate::lang::types::{PlainNode, first_child_with_field};
use crate::llm::chunk::ChunkText;
use crate::llm::schemas::{CitedPosition, RemoveModeComment};

use super::heuristics::word_tokens;

/// The language-neutral virtual anchor kind of a comment at end of file.
pub const END_OF_FILE: &str = "end_of_file";

/// The snippet-Jaccard floor for pairing a citation with an original when
/// a name is missing on either side. A fixed constant, independent of
/// `--similarity-threshold` (which gates only heuristic redundancy).
pub const SNIPPET_JACCARD_MIN: f64 = 0.6;

/// The score of an `end_of_file` ↔ `end_of_file` pair: a name + kind
/// match.
const END_OF_FILE_SCORE: f64 = 3.0;

/// A regeneration no original claims, at a validated insertion point.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCandidate {
    /// The insertion offset in the original text: for `leading`, the start
    /// of the line holding the anchor's outermost wrapper (after a leading
    /// BOM on line 1); for `trailing`, the end of the anchor's line before
    /// its EOL.
    pub offset: usize,
    /// The final position, after the trailing → leading conversion (which
    /// may differ from `comment.position`, the model's citation).
    pub position: CitedPosition,
    /// The index (into the `chunk_texts` slice) of the chunk the citation
    /// came from.
    pub chunk: usize,
    /// The model's regeneration, its `comment` text normalized
    /// (`normalize_comment_text`) and its `position` set to the final
    /// position.
    pub comment: RemoveModeComment,
}

/// The result of [`align_remove_mode`].
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    /// Each paired target id with its regeneration (comment text
    /// normalized), in id (offset) order.
    pub pairs: Vec<(String, RemoveModeComment)>,
    /// The target ids no regeneration paired with, in id (offset) order.
    pub unmatched_originals: Vec<String>,
    /// The surviving NEW candidates, at most one per (offset, position),
    /// in citation order.
    pub new_candidates: Vec<NewCandidate>,
    /// `unresolvable-anchor` and `invalid-insertion` events, in citation
    /// order.
    pub warnings: Vec<FileWarning>,
}

/// One citation after resolution.
struct Citation {
    /// The chunk (slice index) it came from.
    chunk: usize,
    /// The model's regeneration.
    comment: RemoveModeComment,
    /// The resolved anchor position in the original text.
    original: usize,
}

/// Aligns a remove-mode file's regenerations with its target comments.
///
/// `text` is the original file text, inventoried as `inventory` under
/// `adapter`'s language; `targets` the ids hidden from the model;
/// `chunk_texts` the remove-mode chunk texts the model saw; and
/// `regenerations` each cited comment with the index (into
/// `chunk_texts`) of the chunk that produced it. Citation order is
/// chunk order, then the order the model returned them within a chunk.
///
/// # Errors
///
/// [`Error::Invariant`] when a regeneration names a chunk outside
/// `chunk_texts`, when a chunk text is not a remove-mode text, or when a
/// chunk's clean text is not the matching slice of the whole-file clean
/// text (all caller bugs); any error of the grammar registry.
pub fn align_remove_mode(
    text: &str,
    adapter: &LanguageAdapter,
    inventory: &Inventory,
    targets: &BTreeSet<String>,
    chunk_texts: &[ChunkText],
    mut regenerations: Vec<(usize, RemoveModeComment)>,
) -> Result<Alignment, Error> {
    let mut warnings = Vec::new();

    // Resolution, against the whole-file clean text parsed once.
    let clean = remove_comments(
        text,
        inventory,
        TransformOpts {
            targets: Some(targets),
            range: None,
        },
    );
    let clean_ranges = chunk_clean_ranges(&clean.clean_text, &clean.offset_map, chunk_texts)?;
    let clean_tree = parse(&clean.clean_text, adapter.id)?;
    let declarations = if clean_tree.has_error || clean_tree.too_deep {
        None
    } else {
        Some(list_declarations(
            &clean_tree.root,
            &clean.clean_text,
            adapter,
        ))
    };
    regenerations.sort_by_key(|(chunk, _)| *chunk);
    let mut citations = Vec::new();
    for (chunk, mut comment) in regenerations {
        let Some(range) = clean_ranges.get(chunk) else {
            return Err(Error::Invariant(format!(
                "a regeneration names chunk {chunk}, but the file has {} chunk texts",
                chunk_texts.len()
            )));
        };
        let is_last = chunk_texts[chunk].chunk.original_end == text.len();
        let resolver = Resolver {
            clean_text: &clean.clean_text,
            declarations: declarations.as_deref(),
            range: range.clone(),
            is_last,
        };
        match resolver.resolve(&comment.anchor) {
            Some(clean_offset) => {
                comment.comment = normalize_comment_text(&comment.comment, adapter);
                citations.push(Citation {
                    chunk,
                    comment,
                    original: map_clean_offset(&clean.offset_map, clean_offset),
                });
            }
            None => warnings.push(FileWarning {
                kind: FileWarningKind::UnresolvableAnchor,
                detail: format!("chunk {chunk}: {}", describe(&comment.anchor)),
            }),
        }
    }

    // Pairing against the targets the clean text hides.
    let originals: Vec<&InventoryComment> = inventory
        .comments
        .iter()
        .filter(|comment| comment.editable && targets.contains(&comment.id))
        .collect();
    let assignment = assign(&citations, &originals, adapter);
    let mut paired_citations = vec![false; citations.len()];
    let mut paired_originals = vec![None; originals.len()];
    for (citation, original) in assignment {
        paired_citations[citation] = true;
        paired_originals[original] = Some(citation);
    }
    let mut pairs = Vec::new();
    let mut unmatched_originals = Vec::new();
    for (original, paired) in originals.iter().zip(&paired_originals) {
        match paired {
            Some(citation) => {
                pairs.push((original.id.clone(), citations[*citation].comment.clone()))
            }
            None => unmatched_originals.push(original.id.clone()),
        }
    }

    // NEW candidates from the unpaired citations, in citation order.
    let unpaired = citations
        .into_iter()
        .zip(paired_citations)
        .filter(|(_, paired)| !paired)
        .map(|(citation, _)| citation);
    let new_candidates = new_candidates(text, adapter, inventory, unpaired, &mut warnings)?;

    Ok(Alignment {
        pairs,
        unmatched_originals,
        new_candidates,
        warnings,
    })
}

/// A human-readable rendering of a cited anchor, for warnings.
fn describe(anchor: &Anchor) -> String {
    let first_line = anchor
        .snippet
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    match &anchor.name {
        Some(name) => format!("{} `{name}` ({first_line:?})", anchor.node_kind),
        None => format!("{} ({first_line:?})", anchor.node_kind),
    }
}

/// Each chunk's range in the whole-file clean text. A chunk's clean text
/// is, by construction of the range transform, the slice of the
/// whole-file clean text starting where the chunk's original start maps
/// to; that is checked, not assumed.
fn chunk_clean_ranges(
    clean_text: &str,
    map: &[Segment],
    chunk_texts: &[ChunkText],
) -> Result<Vec<Range<usize>>, Error> {
    chunk_texts
        .iter()
        .enumerate()
        .map(|(index, chunk_text)| {
            if chunk_text.offset_map.is_none() {
                return Err(Error::Invariant(format!(
                    "chunk text {index} is not a remove-mode text"
                )));
            }
            let start = clean_offset_of(map, chunk_text.chunk.original_start);
            let range = start..start + chunk_text.prompt_text.len();
            if clean_text.get(range.clone()) != Some(chunk_text.prompt_text.as_str()) {
                return Err(Error::Invariant(format!(
                    "chunk text {index}'s clean text is not the matching slice of the \
                     whole-file clean text (this is a bug)"
                )));
            }
            Ok(range)
        })
        .collect()
}

/// The clean-text offset where a range starting at original offset
/// `original` starts: the inverse of [`map_clean_offset`] at range starts.
/// A removal edit belongs to the range its start lies in, so an edit
/// straddling `original` contributes its replacement before it.
fn clean_offset_of(map: &[Segment], original: usize) -> usize {
    for segment in map {
        if original <= segment.original_start {
            return segment.clean_start;
        }
        if original < segment.original_end {
            return if segment.verbatim {
                segment.clean_start + (original - segment.original_start)
            } else {
                segment.clean_end
            };
        }
    }
    map.last().map_or(original, |segment| segment.clean_end)
}

/// Resolves citations from one chunk against the whole-file clean text.
struct Resolver<'a> {
    clean_text: &'a str,
    /// The whole-file clean tree's declarations; `None` when the clean
    /// parse is `has_error`/`too_deep` (snippet-only resolution).
    declarations: Option<&'a [DeclInfo]>,
    /// The chunk's range in the clean text.
    range: Range<usize>,
    /// Whether the chunk is the file's last.
    is_last: bool,
}

impl Resolver<'_> {
    /// The clean offset `anchor` resolves to, or `None` when it is
    /// unresolvable.
    fn resolve(&self, anchor: &Anchor) -> Option<usize> {
        if anchor.node_kind == END_OF_FILE {
            return self.is_last.then_some(self.clean_text.len());
        }
        let occurrence = self.snippet_occurrence(&anchor.snippet);
        let matches: Vec<&DeclInfo> = self
            .declarations
            .unwrap_or_default()
            .iter()
            .filter(|declaration| {
                self.range.contains(&declaration.start)
                    && declaration.node_kind == anchor.node_kind
                    && declaration.name == anchor.name
            })
            .collect();
        let chosen = match occurrence {
            // Nearest to the snippet occurrence; `min_by_key` keeps the
            // first (in document order) of equally near matches.
            Some(at) => matches
                .iter()
                .min_by_key(|declaration| declaration.start.abs_diff(at)),
            None => matches.first(),
        };
        chosen.map(|declaration| declaration.start).or(occurrence)
    }

    /// The first exact occurrence, inside the chunk's clean range, of the
    /// snippet's first non-blank line (trimmed, so a citation that drops
    /// or re-indents leading whitespace still matches), as the clean
    /// offset of its first character.
    fn snippet_occurrence(&self, snippet: &str) -> Option<usize> {
        let needle = snippet
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        let haystack = self.clean_text.get(self.range.clone())?;
        haystack.find(needle).map(|at| self.range.start + at)
    }
}

/// The acceptance rule and score of pairing a citation's anchor with an
/// original's: `None` when the pair is not acceptable, else
/// `2·(name match) + 1·(nodeKind match) + jaccard(snippet token sets)`.
fn pair_score(cited: &Anchor, original: &Anchor, adapter: &LanguageAdapter) -> Option<f64> {
    if cited.node_kind == END_OF_FILE && original.node_kind == END_OF_FILE {
        return Some(END_OF_FILE_SCORE);
    }
    let kind_match = cited.node_kind == original.node_kind;
    let jaccard = snippet_jaccard(&cited.snippet, &original.snippet);
    let kind_score = if kind_match { 1.0 } else { 0.0 };
    match (&cited.name, &original.name) {
        (Some(cited_name), Some(original_name)) => {
            // Different names are different declarations, whatever the
            // kind or snippet.
            (cited_name == original_name).then_some(2.0 + kind_score + jaccard)
        }
        _ => {
            // Statement-level anchors: the model cannot be expected to
            // know a tree-sitter kind outside the prompt's vocabulary.
            let statement_level = !adapter
                .declaration_kinds
                .contains(&cited.node_kind.as_str())
                || !adapter
                    .declaration_kinds
                    .contains(&original.node_kind.as_str());
            (jaccard >= SNIPPET_JACCARD_MIN && (kind_match || statement_level))
                .then_some(kind_score + jaccard)
        }
    }
}

/// The Jaccard similarity of two snippets' word-token sets (the
/// comment-similarity tokenizer); two empty sets score 0.
fn snippet_jaccard(a: &str, b: &str) -> f64 {
    let a = word_tokens(a);
    let b = word_tokens(b);
    let union = a.union(&b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / union as f64
}

/// Whether a cited position suits an original's: a cited `leading` suits
/// a `leading` or `dangling` original, a cited `trailing` a `trailing`
/// one.
fn positions_compatible(cited: CitedPosition, original: Position) -> bool {
    match cited {
        CitedPosition::Leading => matches!(original, Position::Leading | Position::Dangling),
        CitedPosition::Trailing => original == Position::Trailing,
    }
}

/// The distance from an original offset to a comment's range (0 inside
/// it).
fn distance_to(offset: usize, comment: &InventoryComment) -> usize {
    if offset < comment.start {
        comment.start - offset
    } else {
        offset.saturating_sub(comment.end)
    }
}

/// Greedy one-to-one assignment over the acceptable (citation, original)
/// pairs, highest score first; equal scores are broken by position
/// compatibility, ascending offset distance, citation index and original
/// id (offset order) — a strict total order. Returns `(citation index,
/// original index)` pairs.
fn assign(
    citations: &[Citation],
    originals: &[&InventoryComment],
    adapter: &LanguageAdapter,
) -> Vec<(usize, usize)> {
    struct Candidate {
        score: f64,
        compatible: bool,
        distance: usize,
        citation: usize,
        original: usize,
    }
    let mut candidates = Vec::new();
    for (citation_index, citation) in citations.iter().enumerate() {
        for (original_index, original) in originals.iter().enumerate() {
            if let Some(score) = pair_score(&citation.comment.anchor, &original.anchor, adapter) {
                candidates.push(Candidate {
                    score,
                    compatible: positions_compatible(citation.comment.position, original.position),
                    distance: distance_to(citation.original, original),
                    citation: citation_index,
                    original: original_index,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.compatible.cmp(&a.compatible))
            .then(a.distance.cmp(&b.distance))
            .then(a.citation.cmp(&b.citation))
            .then(a.original.cmp(&b.original))
    });
    let mut citation_taken = vec![false; citations.len()];
    let mut original_taken = vec![false; originals.len()];
    let mut assignment = Vec::new();
    for candidate in candidates {
        if citation_taken[candidate.citation] || original_taken[candidate.original] {
            continue;
        }
        citation_taken[candidate.citation] = true;
        original_taken[candidate.original] = true;
        assignment.push((candidate.citation, candidate.original));
    }
    assignment
}

/// One leaf of the original tree, for token-gap validation: comment
/// subtrees are atomic leaves, and grammar-internal leaves count too
/// (they are real text when they are not zero-width).
struct Leaf {
    range: Range<usize>,
    /// A code token in the scanner's sense: not a comment, not a
    /// grammar-internal (`_`-prefixed) kind, not the shebang.
    code: bool,
}

/// The original text's leaves in document order (iterative pre-order).
fn original_leaves(root: &PlainNode, adapter: &LanguageAdapter) -> Vec<Leaf> {
    let mut leaves = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let comment = adapter.comment_node_kinds.contains(&node.kind);
        if comment || node.children.is_empty() {
            if !std::ptr::eq(node, root) {
                leaves.push(Leaf {
                    range: node.start..node.end,
                    code: !comment && !node.kind.starts_with('_') && !is_shebang_node(node),
                });
            }
            continue;
        }
        stack.extend(node.children.iter().rev());
    }
    leaves
}

/// Why an insertion point is invalid.
type Rejection = String;

/// The original-text facts NEW validation consults, computed once.
struct Slots<'a> {
    text: &'a str,
    adapter: &'a LanguageAdapter,
    inventory: &'a Inventory,
    root: PlainNode,
    leaves: Vec<Leaf>,
    /// The earliest valid insertion offset: after a leading BOM, and after
    /// line 1's EOL in a file with a shebang.
    earliest: usize,
    /// Half-open original ranges no insertion may fall in, each with its
    /// reason: the functional-comment exclusions.
    forbidden: Vec<(Range<usize>, &'static str)>,
}

impl<'a> Slots<'a> {
    fn new(
        text: &'a str,
        adapter: &'a LanguageAdapter,
        inventory: &'a Inventory,
    ) -> Result<Slots<'a>, Error> {
        let tree = parse(text, adapter.id)?;
        if tree.too_deep {
            return Err(Error::Invariant(
                "an inventoried file's parse tree exceeds MAX_TREE_DEPTH (this is a bug)".into(),
            ));
        }
        let leaves = original_leaves(&tree.root, adapter);
        let bom = if text.starts_with('\u{FEFF}') {
            '\u{FEFF}'.len_utf8()
        } else {
            0
        };
        let earliest = if inventory.has_shebang {
            text.find('\n').map_or(text.len(), |at| at + 1)
        } else {
            bom
        };
        let mut slots = Slots {
            text,
            adapter,
            inventory,
            root: tree.root,
            leaves,
            earliest,
            forbidden: Vec::new(),
        };
        slots.forbidden = slots.functional_exclusions();
        Ok(slots)
    }

    /// The ranges where an insertion would change what a functional
    /// comment applies to.
    fn functional_exclusions(&self) -> Vec<(Range<usize>, &'static str)> {
        let text = self.text;
        let mut forbidden = Vec::new();
        for comment in &self.inventory.comments {
            // Between a next-scoped directive and the code it governs.
            if comment.directive_scope == Some(Scope::Next) {
                forbidden.push((
                    comment.end..comment.next_code_start + 1,
                    "between a next-line directive and its code",
                ));
            }
            // Inside a line-window directive's window: the comment's own
            // line's remainder plus the `window` lines after it.
            if let Some(window) = comment.directive_line_window {
                let mut end = comment.end;
                let mut lines = 0;
                while lines <= window {
                    match text[end..].find('\n') {
                        Some(at) => {
                            end += at + 1;
                            lines += 1;
                        }
                        None => {
                            end = text.len() + 1;
                            break;
                        }
                    }
                }
                forbidden.push((comment.end..end, "inside a line-window directive's window"));
            }
            // Between a `//go:` directive and the declaration it annotates.
            if self.adapter.id == LanguageId::Go
                && comment.classification == Classification::Functional
                && comment
                    .members
                    .iter()
                    .any(|member| text[member.clone()].starts_with("//go:"))
            {
                forbidden.push((
                    comment.end..comment.next_code_start + 1,
                    "between a //go: directive and its declaration",
                ));
            }
        }
        let rules = self.adapter.positional_rules;
        // Immediately before `import "C"`: a comment there joins the cgo
        // preamble.
        if rules.contains(&PositionalRuleId::CgoPreamble) {
            for target in cgo_doc_targets(&self.root, text) {
                let previous_end = self
                    .leaves
                    .iter()
                    .filter(|leaf| leaf.code && leaf.range.end <= target)
                    .map(|leaf| leaf.range.end)
                    .next_back()
                    .unwrap_or(0);
                forbidden.push((previous_end..target + 1, "immediately before import \"C\""));
            }
        }
        // Inside a Go example function's output block: from the line of its
        // first output comment to the function's end.
        if rules.contains(&PositionalRuleId::GoExampleOutput) {
            let mut seen = HashSet::new();
            for comment in &self.inventory.comments {
                if let Some(function) = &comment.enclosing_top_level
                    && comment.tags.contains("example-output")
                    && seen.insert(function.start)
                {
                    forbidden.push((
                        line_start_of(text, comment.start)..function.end,
                        "inside a Go example's output block",
                    ));
                }
            }
        }
        forbidden
    }
}

impl Slots<'_> {
    /// The start of the line holding `offset` (after a leading BOM on
    /// line 1) and the end of that line before its EOL.
    fn line_bounds(&self, offset: usize) -> Range<usize> {
        let start = line_start_of(self.text, offset);
        let rest = &self.text[offset..];
        let mut end = offset + rest.find('\n').unwrap_or(rest.len());
        if self.text[..end].ends_with('\r') {
            end -= 1;
        }
        start..end
    }

    /// Whether the line spanning `line` ends in (or inside) a comment.
    fn line_ends_in_comment(&self, line: &Range<usize>) -> bool {
        let trimmed_end = line.start + self.text[line.clone()].trim_end().len();
        trimmed_end > line.start
            && self
                .inventory
                .comments
                .iter()
                .any(|comment| comment.start < trimmed_end && trimmed_end <= comment.end)
    }

    /// The start of the outermost wrapper of the code at `at`: climbs
    /// from the shallowest node starting at `at` through the adapter's
    /// `Field` wrappers (`export_statement`, `decorated_definition`) and
    /// back over a run of `PrefixSibling` nodes (Rust attributes). `at`
    /// itself when no node starts there.
    fn outermost_start(&self, at: usize) -> usize {
        let mut path = vec![&self.root];
        loop {
            let node = path[path.len() - 1];
            if node.start == at && path.len() > 1 {
                break;
            }
            match node
                .children
                .iter()
                .find(|child| child.start <= at && at < child.end)
            {
                Some(child) => path.push(child),
                None => return at,
            }
        }
        let mut index = path.len() - 1;
        let mut start = at;
        loop {
            let current = path[index];
            let parent = path[index - 1];
            if let Some(first) = self.prefix_run_start(parent, current) {
                start = start.min(first);
            }
            let wrapped = index >= 2
                && self.adapter.anchor_wrappers.iter().any(|wrapper| {
                    matches!(*wrapper, AnchorWrapper::Field { kind, field }
                        if parent.kind == kind
                            && first_child_with_field(parent, field)
                                .is_some_and(|child| std::ptr::eq(child, current)))
                });
            if !wrapped {
                return start;
            }
            start = start.min(parent.start);
            index -= 1;
        }
    }

    /// The start of the run of `PrefixSibling` wrapper nodes directly
    /// preceding `current` among `parent`'s children (comments between
    /// them skipped), if any.
    fn prefix_run_start(&self, parent: &PlainNode, current: &PlainNode) -> Option<usize> {
        let prefix_kinds: Vec<&str> = self
            .adapter
            .anchor_wrappers
            .iter()
            .filter_map(|wrapper| match *wrapper {
                AnchorWrapper::PrefixSibling { kind } => Some(kind),
                AnchorWrapper::Field { .. } => None,
            })
            .collect();
        if prefix_kinds.is_empty() {
            return None;
        }
        let position = parent
            .children
            .iter()
            .position(|child| std::ptr::eq(child, current))?;
        let mut first = None;
        for sibling in parent.children[..position].iter().rev() {
            if self.adapter.comment_node_kinds.contains(&sibling.kind) {
                continue;
            }
            if !prefix_kinds.contains(&sibling.kind) {
                break;
            }
            first = Some(sibling.start);
        }
        first
    }

    /// Whether `offset` lies in a token gap: inside no leaf (a comment is
    /// one leaf), with only whitespace between it and the surrounding
    /// leaves (a leading BOM counts as the file's edge). Leaf-less syntax
    /// such as raw-string delimiters is not whitespace, so it is never a
    /// gap.
    fn in_token_gap(&self, offset: usize) -> bool {
        let next = self.leaves.partition_point(|leaf| leaf.range.end <= offset);
        if self
            .leaves
            .get(next)
            .is_some_and(|leaf| leaf.range.start < offset)
        {
            return false;
        }
        let bom = if self.text.starts_with('\u{FEFF}') {
            '\u{FEFF}'.len_utf8()
        } else {
            0
        };
        let before = next
            .checked_sub(1)
            .map_or(bom, |index| self.leaves[index].range.end.max(bom));
        let after = self
            .leaves
            .get(next)
            .map_or(self.text.len(), |leaf| leaf.range.start);
        offset >= before
            && self.text[before..offset].chars().all(char::is_whitespace)
            && self.text[offset..after].chars().all(char::is_whitespace)
    }

    /// The start of the first code token at or after `offset` (the end of
    /// the text when there is none): the `next_code_start` a comment
    /// inserted at `offset` would have.
    fn next_code_start(&self, offset: usize) -> usize {
        self.leaves
            .iter()
            .find(|leaf| leaf.code && leaf.range.start >= offset)
            .map_or(self.text.len(), |leaf| leaf.range.start)
    }

    /// Places an unpaired citation: its insertion offset and final
    /// position, or why it has no valid slot.
    fn place(&self, citation: &Citation) -> Result<(usize, CitedPosition), Rejection> {
        if citation.comment.anchor.node_kind == END_OF_FILE {
            return Err("NEW comments are never inserted at end_of_file".into());
        }
        let line = self.line_bounds(citation.original);
        let mut position = citation.comment.position;
        // Appending after a comment would corrupt it (`// eslint-disable-line
        // x // new`): such a trailing NEW goes above the line instead.
        if position == CitedPosition::Trailing && self.line_ends_in_comment(&line) {
            position = CitedPosition::Leading;
        }
        let offset = match position {
            CitedPosition::Leading => {
                line_start_of(self.text, self.outermost_start(citation.original))
            }
            CitedPosition::Trailing => line.end,
        };
        if offset < self.earliest {
            return Err(
                "the slot lies before the end of the shebang line or inside the BOM".into(),
            );
        }
        if !self.in_token_gap(offset) {
            return Err("the slot is not a token gap".into());
        }
        if let Some((_, reason)) = self
            .forbidden
            .iter()
            .find(|(range, _)| range.contains(&offset))
        {
            return Err(format!("the slot lies {reason}"));
        }
        let occupied = match position {
            CitedPosition::Leading => {
                let code = self.next_code_start(offset);
                self.inventory.comments.iter().any(|comment| {
                    comment.position == Position::Leading && comment.next_code_start == code
                })
            }
            CitedPosition::Trailing => {
                let line = self.line_bounds(offset);
                self.inventory.comments.iter().any(|comment| {
                    comment.position == Position::Trailing && line.contains(&comment.start)
                })
            }
        };
        if occupied {
            return Err("the slot already holds a comment".into());
        }
        Ok((offset, position))
    }
}

/// Validates the unpaired citations' insertion points, in citation order:
/// invalid ones and all but the first per (offset, position) are dropped
/// with an `invalid-insertion` warning.
fn new_candidates(
    text: &str,
    adapter: &LanguageAdapter,
    inventory: &Inventory,
    unpaired: impl Iterator<Item = Citation>,
    warnings: &mut Vec<FileWarning>,
) -> Result<Vec<NewCandidate>, Error> {
    let mut unpaired = unpaired.peekable();
    if unpaired.peek().is_none() {
        return Ok(Vec::new());
    }
    let slots = Slots::new(text, adapter, inventory)?;
    let mut taken = HashSet::new();
    let mut candidates = Vec::new();
    for citation in unpaired {
        let placed = slots.place(&citation).and_then(|(offset, position)| {
            if taken.insert((offset, position)) {
                Ok((offset, position))
            } else {
                Err("an earlier NEW already claims the slot".into())
            }
        });
        match placed {
            Ok((offset, position)) => {
                let mut comment = citation.comment;
                comment.position = position;
                candidates.push(NewCandidate {
                    offset,
                    position,
                    chunk: citation.chunk,
                    comment,
                });
            }
            Err(reason) => warnings.push(FileWarning {
                kind: FileWarningKind::InvalidInsertion,
                detail: format!(
                    "chunk {}: {} {}: {reason}",
                    citation.chunk,
                    match citation.comment.position {
                        CitedPosition::Leading => "leading",
                        CitedPosition::Trailing => "trailing",
                    },
                    describe(&citation.comment.anchor)
                ),
            }),
        }
    }
    Ok(candidates)
}

#[cfg(test)]
#[path = "align_tests.rs"]
mod tests;
