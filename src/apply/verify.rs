//! The semantic verifier: the hard gate before every write.
//!
//! [`verify_semantic_equality`] parses the original and the edited text
//! with the file's grammar and proves that the edit changed nothing but
//! comment text and whitespace. Five checks run over [`PlainTree`] data,
//! plus one [`inventory_file`] run on the edited text:
//!
//! 1. **Parse cleanliness.** The edited text must parse with no `ERROR` or
//!    `MISSING` node and within [`MAX_TREE_DEPTH`](crate::lang::types::MAX_TREE_DEPTH).
//! 2. **Token stream.** The leaf walk of visible tokens (kind + text), with
//!    comment-kind subtrees and grammar-internal hidden tokens
//!    (`_`-prefixed named kinds, e.g. Python's `_indent`, JavaScript's
//!    `_automatic_semicolon`) excluded, must be identical.
//! 3. **Tree shape.** The pre-order `(kind, depth)` sequence over every
//!    non-comment node must be identical. This is what sees a Python
//!    dedent, which leaves tokens and gaps untouched.
//! 4. **Gaps.** For every boundary between consecutive tokens, the text in
//!    between (comment interiors included) must agree on line-terminator
//!    presence — the ASI / Go-newline guard — and every gap (the leading
//!    and trailing ones too) must have an identical *residue*: the gap
//!    text with comment ranges cut out and whitespace removed. Syntax that
//!    sits in no leaf (tree-sitter-rust's raw-string delimiters) lands in a
//!    residue, so even leaf-less code text is covered.
//! 5. **Functional sequence (two-way).** The ordered functional comments
//!    of the edited text — each one's raw text *and* the tags naming its
//!    functional roles ([`FunctionalComment`]) — must equal the original's:
//!    nothing lost, altered, introduced or activated by position. Tags
//!    matter because one comment can be functional for several reasons: a
//!    vim modeline `# vim: set fileencoding=latin-1 :` on line 3 is
//!    functional (`modeline`) but not a PEP 263 declaration; moved onto
//!    line 1 by a removal, its text is unchanged, yet it gains
//!    `encoding-cookie` and changes how CPython decodes the file.
//!
//! **Token edges.** Some grammars lex the whitespace separating a token
//! from a following (or preceding) comment into the token itself:
//! tree-sitter-c's `preproc_arg` for `#define X 2 /* c */` is `2 `.
//! Removing that comment (and, per line hygiene, the whitespace before it)
//! changes the token's text without changing code. So when a comment node
//! touches a token's edge in either text, that edge's whitespace is not
//! part of the token's compared text in either; it belongs to the adjacent
//! gap, where the line-break and residue checks still see it. Edge
//! whitespace touched by no comment (next to a string delimiter, another
//! token) stays part of the token, so `" a"` → `"a"` is still a token
//! change.
//!
//! **Line breaks inside tokens.** Per the Design, a token's own trailing
//! line terminators count toward the following gap's line-break check
//! (tree-sitter-rust's `shebang` token includes its `\n`). The token's
//! text is still compared exactly, so this cannot hide a token change.
//!
//! Every walk here is iterative, so a tree just under the depth limit
//! cannot overflow the stack.
//!
//! Failures are [`Error::Verify`]; errors from [`lang::parse`] and
//! [`inventory_file`] propagate unchanged (an [`Error::Invariant`] stays
//! an invariant).

use std::collections::BTreeSet;
use std::ops::Range;

use crate::comments::hygiene::is_line_terminator;
use crate::comments::inventory::{Inventory, inventory_file};
use crate::comments::types::{Classification, FileRef};
use crate::errors::Error;
use crate::lang;
use crate::lang::adapter::LanguageAdapter;
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::lang::types::{PlainNode, PlainTree, leaves};

/// The longest token or residue excerpt quoted in a failure reason.
const EXCERPT_CHARS: usize = 40;

/// A functional comment as the functional-sequence check compares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionalComment {
    /// The record's raw text.
    pub text: String,
    /// Every tag the record earned; together they name its functional
    /// roles, positional ones (`encoding-cookie`, `modeline`, `license`,
    /// `directive-adjacent`, …) included.
    pub tags: BTreeSet<String>,
}

/// The ordered functional comments of `inventory`: what
/// [`verify_semantic_equality`] takes as `original_functional` (from the
/// firewall's [`inventory_file`] run) and compares against the edited
/// text's.
pub fn functional_sequence(inventory: &Inventory) -> Vec<FunctionalComment> {
    inventory
        .comments
        .iter()
        .filter(|comment| comment.classification == Classification::Functional)
        .map(|comment| FunctionalComment {
            text: comment.text.clone(),
            tags: comment.tags.clone(),
        })
        .collect()
}

/// Verifies that `edited` differs from `original` only in comment text and
/// whitespace, under `file.language`'s grammar. `original_functional` is
/// the original's [`functional_sequence`], from the firewall's
/// [`inventory_file`] run.
///
/// Returns [`Error::Verify`] naming the first failed check. An original
/// that does not parse cleanly cannot anchor a comparison and fails the
/// same way. Registry and inventory errors propagate unchanged.
pub fn verify_semantic_equality(
    original: &str,
    edited: &str,
    file: FileRef,
    original_functional: &[FunctionalComment],
) -> Result<(), Error> {
    let adapter = adapter_for(file.language);
    let original_tree = lang::parse(original, file.language)?;
    if original_tree.too_deep || original_tree.has_error {
        return Err(fail(
            "the original text does not parse cleanly, so there is nothing to verify against",
        ));
    }
    let edited_tree = lang::parse(edited, file.language)?;
    if edited_tree.too_deep {
        return Err(fail("the edited text exceeds the maximum tree depth"));
    }
    if edited_tree.has_error {
        return Err(fail("the edited text does not parse cleanly"));
    }
    let mut before = TokenView::new(original, &original_tree, adapter)?;
    let mut after = TokenView::new(edited, &edited_tree, adapter)?;
    settle_edges(&mut before, &mut after);
    compare_tokens(&before, &after)?;
    compare_shape(&original_tree.root, &edited_tree.root, adapter)?;
    compare_gaps(&before, &after)?;
    let edited_functional = functional_sequence(&inventory_file(edited, file)?);
    compare_functional(original_functional, &edited_functional)
}

/// The non-empty gap residues of `text` under `language`, in document
/// order: what the verifier's residue check compares, exposed so the set
/// of syntax the pinned grammars leave outside leaves can be inspected
/// (and snapshotted). A text that does not parse cleanly has no
/// verifiable gaps and yields [`Error::Verify`].
pub fn gap_residues(text: &str, language: LanguageId) -> Result<Vec<String>, Error> {
    let tree = lang::parse(text, language)?;
    if tree.too_deep || tree.has_error {
        return Err(fail("the text does not parse cleanly"));
    }
    // Edge settling only moves whitespace between a token and a gap, so residues do not depend on it; reusing the verifier's own view keeps the exposed residues in lockstep with the check they mirror.
    let view = TokenView::new(text, &tree, adapter_for(language))?;
    Ok((0..view.gap_count())
        .map(|index| view.residue(index))
        .filter(|residue| !residue.is_empty())
        .collect())
}

/// Builds a verification failure.
fn fail(reason: impl Into<String>) -> Error {
    Error::Verify(reason.into())
}

/// Whether `node` roots a comment subtree.
fn is_comment(node: &PlainNode, adapter: &LanguageAdapter) -> bool {
    adapter.comment_node_kinds.contains(&node.kind)
}

/// Whether `node` is a grammar-internal hidden token: a *named* kind with
/// tree-sitter's `_` prefix. Anonymous tokens are named by their literal
/// text, so `_` (Rust's wildcard) or `__attribute__` (C) stay visible.
fn is_hidden(node: &PlainNode) -> bool {
    node.named && node.kind.starts_with('_')
}

/// One compared token: its kind, its leaf range, whether a comment node
/// touches either edge, and its *core* range — the leaf range minus the
/// edge whitespace that belongs to the adjacent gap (see
/// [`settle_edges`]).
#[derive(Debug)]
struct Token {
    kind: &'static str,
    leaf: Range<usize>,
    /// A comment node ends exactly where the leaf starts.
    comment_before: bool,
    /// A comment node starts exactly where the leaf ends.
    comment_after: bool,
    core: Range<usize>,
}

/// A parsed text seen as the verifier compares it: its tokens in document
/// order and the ranges of its comment subtrees.
struct TokenView<'text> {
    text: &'text str,
    tokens: Vec<Token>,
    /// Ranges of comment-subtree roots, ascending and non-overlapping.
    comments: Vec<Range<usize>>,
}

impl<'text> TokenView<'text> {
    /// Collects `tree`'s comment ranges and tokens, each token's core
    /// still its whole leaf range. The root is the whole file, never a
    /// token: a childless root (an empty or all-whitespace file) yields no
    /// tokens. A leaf range that is not a slice of `text`, or that starts
    /// before the previous leaf ends, is an invariant violation.
    fn new(
        text: &'text str,
        tree: &PlainTree,
        adapter: &LanguageAdapter,
    ) -> Result<TokenView<'text>, Error> {
        let comments = comment_ranges(&tree.root, adapter);
        let mut tokens: Vec<Token> = Vec::new();
        if !tree.root.children.is_empty() {
            let skip = |node: &PlainNode| is_comment(node, adapter) || is_hidden(node);
            for leaf in leaves(&tree.root, skip) {
                let ordered = tokens
                    .last()
                    .is_none_or(|previous| previous.leaf.end <= leaf.start);
                if !ordered || text.get(leaf.start..leaf.end).is_none() {
                    return Err(Error::Invariant(format!(
                        "the {} leaf's range {}..{} is out of order or not a valid slice of \
                         the parsed text (this is a bug)",
                        leaf.kind, leaf.start, leaf.end,
                    )));
                }
                tokens.push(Token {
                    kind: leaf.kind,
                    leaf: leaf.start..leaf.end,
                    comment_before: comments
                        .binary_search_by_key(&leaf.start, |comment| comment.end)
                        .is_ok(),
                    comment_after: comments
                        .binary_search_by_key(&leaf.end, |comment| comment.start)
                        .is_ok(),
                    core: leaf.start..leaf.end,
                });
            }
        }
        Ok(TokenView {
            text,
            tokens,
            comments,
        })
    }

    /// Sets token `index`'s core: its leaf range minus trailing whitespace
    /// when `trim_end`, then minus leading whitespace when `trim_start`.
    fn set_core(&mut self, index: usize, trim_start: bool, trim_end: bool) {
        let leaf = self.tokens[index].leaf.clone();
        let mut end = leaf.end;
        if trim_end {
            end = leaf.start + self.text[leaf.clone()].trim_end().len();
        }
        let mut start = leaf.start;
        if trim_start {
            start = end - self.text[leaf.start..end].trim_start().len();
        }
        self.tokens[index].core = start..end;
    }

    /// The compared text of token `index`.
    fn token_text(&self, index: usize) -> &'text str {
        &self.text[self.tokens[index].core.clone()]
    }

    /// The number of gaps: one before each token and one after the last
    /// (a token-less text is a single gap).
    fn gap_count(&self) -> usize {
        self.tokens.len() + 1
    }

    /// Gap `index`: from the end of token `index - 1` (or the start of the
    /// text) to the start of token `index` (or the end of the text).
    fn gap(&self, index: usize) -> Range<usize> {
        let start = match index {
            0 => 0,
            _ => self.tokens[index - 1].core.end,
        };
        let end = self
            .tokens
            .get(index)
            .map_or(self.text.len(), |token| token.core.start);
        start..end
    }

    /// Whether the boundary before token `index` (`1..tokens.len()`)
    /// contains a line terminator: in the gap itself, comment interiors
    /// included, or among the previous token's own trailing whitespace.
    fn has_line_break(&self, index: usize) -> bool {
        let previous = self.token_text(index - 1);
        let own_trailing = &previous[previous.trim_end().len()..];
        self.text[self.gap(index)].contains(is_line_terminator)
            || own_trailing.contains(is_line_terminator)
    }

    /// Gap `index`'s residue: its text with every comment range cut out
    /// and all whitespace removed.
    fn residue(&self, index: usize) -> String {
        let gap = self.gap(index);
        let mut residue = String::new();
        let mut cursor = gap.start;
        let first = self
            .comments
            .partition_point(|comment| comment.end <= gap.start);
        for comment in self.comments[first..]
            .iter()
            .take_while(|comment| comment.start < gap.end)
        {
            if comment.start > cursor {
                push_non_whitespace(&mut residue, &self.text[cursor..comment.start]);
            }
            cursor = cursor.max(comment.end);
        }
        if cursor < gap.end {
            push_non_whitespace(&mut residue, &self.text[cursor..gap.end]);
        }
        residue
    }
}

/// Appends `segment`'s non-whitespace characters to `out`.
fn push_non_whitespace(out: &mut String, segment: &str) {
    out.extend(
        segment
            .chars()
            .filter(|character| !character.is_whitespace()),
    );
}

/// The ranges of every comment-subtree root under `root`, in document
/// order (pre-order, not descending into comments; iterative).
fn comment_ranges(root: &PlainNode, adapter: &LanguageAdapter) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_comment(node, adapter) {
            ranges.push(node.start..node.end);
        } else {
            stack.extend(node.children.iter().rev());
        }
    }
    ranges
}

/// Decides which token edge whitespace belongs to the gaps, jointly for
/// the two views (pairing tokens by index): an edge of token `i` is
/// trimmed in both views when a comment node touches that edge in either
/// one. Whitespace there separates code from a comment — or did, before
/// the comment was removed — so it is gap whitespace even where the
/// grammar lexed it into the token (tree-sitter-c's `preproc_arg` `2 `
/// before `/* c */`, tree-sitter-rust's `shebang` with its `\n` before a
/// line-2 comment). A comment never touches a literal's content directly,
/// so string contents keep their edge whitespace. Unpaired tokens (the
/// counts differ, which fails the token check) keep their own decision.
fn settle_edges(before: &mut TokenView<'_>, after: &mut TokenView<'_>) {
    for index in 0..before.tokens.len().max(after.tokens.len()) {
        let (a, b) = (before.tokens.get(index), after.tokens.get(index));
        let trim_start = a.is_some_and(|token| token.comment_before)
            || b.is_some_and(|token| token.comment_before);
        let trim_end = a.is_some_and(|token| token.comment_after)
            || b.is_some_and(|token| token.comment_after);
        if a.is_some() {
            before.set_core(index, trim_start, trim_end);
        }
        if b.is_some() {
            after.set_core(index, trim_start, trim_end);
        }
    }
}

/// Check 2: identical token streams (kind + compared text).
fn compare_tokens(before: &TokenView<'_>, after: &TokenView<'_>) -> Result<(), Error> {
    let shared = before.tokens.len().min(after.tokens.len());
    for index in 0..shared {
        let (kind_a, kind_b) = (before.tokens[index].kind, after.tokens[index].kind);
        let (text_a, text_b) = (before.token_text(index), after.token_text(index));
        if kind_a != kind_b || text_a != text_b {
            return Err(fail(format!(
                "token-stream mismatch at token {index} (line {}): {kind_a} {} became {kind_b} {}",
                line_of(before.text, before.tokens[index].core.start),
                excerpt(text_a),
                excerpt(text_b),
            )));
        }
    }
    if before.tokens.len() != after.tokens.len() {
        return Err(fail(format!(
            "token-stream mismatch: the original has {} tokens and the edited text {}",
            before.tokens.len(),
            after.tokens.len(),
        )));
    }
    Ok(())
}

/// The pre-order `(kind, depth)` sequence of every non-comment node under
/// `root` (iterative). Comment subtrees are skipped whole, so depth counts
/// non-comment ancestors only.
fn shape(root: &PlainNode, adapter: &LanguageAdapter) -> Vec<(&'static str, usize)> {
    let mut sequence = Vec::new();
    let mut stack = vec![(root, 0_usize)];
    while let Some((node, depth)) = stack.pop() {
        if is_comment(node, adapter) {
            continue;
        }
        sequence.push((node.kind, depth));
        stack.extend(node.children.iter().rev().map(|child| (child, depth + 1)));
    }
    sequence
}

/// Check 3: identical tree shapes.
fn compare_shape(
    before: &PlainNode,
    after: &PlainNode,
    adapter: &LanguageAdapter,
) -> Result<(), Error> {
    let (shape_a, shape_b) = (shape(before, adapter), shape(after, adapter));
    if let Some(index) = shape_a.iter().zip(&shape_b).position(|(a, b)| a != b) {
        let ((kind_a, depth_a), (kind_b, depth_b)) = (shape_a[index], shape_b[index]);
        return Err(fail(format!(
            "tree-shape mismatch at node {index}: {kind_a} at depth {depth_a} became \
             {kind_b} at depth {depth_b}"
        )));
    }
    if shape_a.len() != shape_b.len() {
        return Err(fail(format!(
            "tree-shape mismatch: the original has {} nodes and the edited text {}",
            shape_a.len(),
            shape_b.len(),
        )));
    }
    Ok(())
}

/// Check 4: per gap, line-terminator presence (interior boundaries only:
/// the leading and trailing gaps separate no tokens) and residue equality
/// (every gap). Both views have the same tokens by now.
fn compare_gaps(before: &TokenView<'_>, after: &TokenView<'_>) -> Result<(), Error> {
    for index in 0..before.gap_count() {
        let interior = index > 0 && index < before.tokens.len();
        if interior && before.has_line_break(index) != after.has_line_break(index) {
            return Err(fail(format!(
                "line-break boundary flipped at token boundary {index} (line {})",
                line_of(before.text, before.gap(index).start),
            )));
        }
        let (residue_a, residue_b) = (before.residue(index), after.residue(index));
        if residue_a != residue_b {
            return Err(fail(format!(
                "uncovered text changed at token boundary {index} (line {}): {} became {}",
                line_of(before.text, before.gap(index).start),
                excerpt(&residue_a),
                excerpt(&residue_b),
            )));
        }
    }
    Ok(())
}

/// Check 5: the functional-comment sequences (texts and tags) are equal.
fn compare_functional(
    original: &[FunctionalComment],
    edited: &[FunctionalComment],
) -> Result<(), Error> {
    if original == edited {
        return Ok(());
    }
    let index = original
        .iter()
        .zip(edited)
        .position(|(a, b)| a != b)
        .unwrap_or(original.len().min(edited.len()));
    let describe = |comments: &[FunctionalComment]| {
        comments.get(index).map_or_else(
            || String::from("none"),
            |comment| format!("{} {:?}", excerpt(&comment.text), comment.tags),
        )
    };
    Err(fail(format!(
        "functional-comment mismatch: the original has {} functional comments and the edited \
         text {}; at #{index}, {} became {}",
        original.len(),
        edited.len(),
        describe(original),
        describe(edited),
    )))
}

/// The 1-based line of byte `offset` in `text` (`\r\n` counts once).
fn line_of(text: &str, offset: usize) -> usize {
    let before = text.get(..offset).unwrap_or(text);
    let mut line = 1;
    let mut characters = before.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' && characters.peek() == Some(&'\n') {
            continue;
        }
        if is_line_terminator(character) {
            line += 1;
        }
    }
    line
}

/// `text` quoted for a failure reason, cut to [`EXCERPT_CHARS`].
fn excerpt(text: &str) -> String {
    let mut cut: String = text.chars().take(EXCERPT_CHARS).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    format!("{cut:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic node over `range`.
    fn node(
        kind: &'static str,
        named: bool,
        range: Range<usize>,
        children: Vec<PlainNode>,
    ) -> PlainNode {
        PlainNode {
            kind,
            kind_id: 1,
            named,
            start: range.start,
            end: range.end,
            start_row: 0,
            start_col: 0,
            end_row: 0,
            field_name: None,
            is_error: false,
            is_missing: false,
            children,
        }
    }

    fn tree(root: PlainNode) -> PlainTree {
        PlainTree {
            root,
            has_error: false,
            too_deep: false,
            comment_query_ranges: Vec::new(),
        }
    }

    /// Both views of a real parse, edges settled.
    fn views<'a>(
        language: LanguageId,
        original: &'a str,
        edited: &'a str,
        trees: &'a (PlainTree, PlainTree),
    ) -> (TokenView<'a>, TokenView<'a>) {
        let adapter = adapter_for(language);
        let mut before = TokenView::new(original, &trees.0, adapter).unwrap();
        let mut after = TokenView::new(edited, &trees.1, adapter).unwrap();
        settle_edges(&mut before, &mut after);
        (before, after)
    }

    #[test]
    fn hidden_tokens_are_named_underscore_kinds_only() {
        assert!(is_hidden(&node("_newline", true, 0..1, Vec::new())));
        assert!(is_hidden(&node(
            "_automatic_semicolon",
            true,
            0..0,
            Vec::new()
        )));
        assert!(!is_hidden(&node("_", false, 0..1, Vec::new())));
        assert!(!is_hidden(&node("__attribute__", false, 0..13, Vec::new())));
        assert!(!is_hidden(&node("identifier", true, 0..1, Vec::new())));
    }

    #[test]
    fn hidden_token_text_folds_into_the_surrounding_gap() {
        // `a` `_newline` `b`, as a grammar materializing the hidden token would present it; the hidden `_newline` leaf is skipped, so its text folds into gap 1, which still reports the break.
        let text = "a\nb";
        let root = node(
            "module",
            true,
            0..3,
            vec![
                node("identifier", true, 0..1, Vec::new()),
                node("_newline", true, 1..2, Vec::new()),
                node("identifier", true, 2..3, Vec::new()),
            ],
        );
        let tree = tree(root);
        let view = TokenView::new(text, &tree, adapter_for(LanguageId::Python)).unwrap();
        let kinds: Vec<_> = view.tokens.iter().map(|token| token.kind).collect();
        assert_eq!(kinds, ["identifier", "identifier"]);
        assert_eq!(view.gap(1), 1..2);
        assert!(view.has_line_break(1));
    }

    #[test]
    fn a_childless_root_has_no_tokens() {
        let tree = tree(node("source_file", true, 0..0, Vec::new()));
        let view = TokenView::new("", &tree, adapter_for(LanguageId::Rust)).unwrap();
        assert!(view.tokens.is_empty());
        assert_eq!(view.gap_count(), 1);
    }

    #[test]
    fn residues_cut_comments_and_whitespace() {
        let text = "fn f() { let s = r#\"x\"#; /* c */ }\n";
        let tree = lang::parse(text, LanguageId::Rust).unwrap();
        let view = TokenView::new(text, &tree, adapter_for(LanguageId::Rust)).unwrap();
        let residues: Vec<String> = (0..view.gap_count())
            .map(|index| view.residue(index))
            .filter(|residue| !residue.is_empty())
            .collect();
        assert_eq!(residues, ["r#\"", "\"#"]);
    }

    #[test]
    fn the_python_dedent_pair_fails_only_the_shape_check() {
        let original = "if a:\n    x()\n    # c\n    y()\nz()\n";
        let edited = "if a:\n    x()\n    # c\ny()\nz()\n";
        let trees = (
            lang::parse(original, LanguageId::Python).unwrap(),
            lang::parse(edited, LanguageId::Python).unwrap(),
        );
        assert!(!trees.1.has_error);
        let (before, after) = views(LanguageId::Python, original, edited, &trees);
        compare_tokens(&before, &after).expect("tokens are unchanged");
        compare_gaps(&before, &after).expect("gaps are unchanged");
        let adapter = adapter_for(LanguageId::Python);
        let Err(Error::Verify(reason)) = compare_shape(&trees.0.root, &trees.1.root, adapter)
        else {
            panic!("the dedent must fail the shape check");
        };
        assert!(reason.starts_with("tree-shape mismatch"), "{reason}");
    }

    #[test]
    fn edge_whitespace_is_settled_jointly() {
        // The shebang touches a comment only in the original; its `\n` leaves the compared text in both views. The shebang token lexes its own newline, so dropping the line-2 comment must leave both texts and the boundary intact.
        let original = "#!/bin/sh\n// c\nfn main() {}\n";
        let edited = "#!/bin/sh\nfn main() {}\n";
        let trees = (
            lang::parse(original, LanguageId::Rust).unwrap(),
            lang::parse(edited, LanguageId::Rust).unwrap(),
        );
        let (before, after) = views(LanguageId::Rust, original, edited, &trees);
        assert_eq!(before.token_text(0), "#!/bin/sh");
        assert_eq!(after.token_text(0), "#!/bin/sh");
        assert!(before.has_line_break(1) && after.has_line_break(1));
    }

    #[test]
    fn a_tokens_own_trailing_line_break_counts_for_the_boundary() {
        // The shebang token keeps its trailing newline, so gap 1 is empty yet the boundary still counts as a break.
        let text = "#!/bin/sh\nfn main() {}\n";
        let tree = lang::parse(text, LanguageId::Rust).unwrap();
        let view = TokenView::new(text, &tree, adapter_for(LanguageId::Rust)).unwrap();
        assert_eq!(view.token_text(0), "#!/bin/sh\n");
        assert!(view.gap(1).is_empty());
        assert!(view.has_line_break(1));
    }

    #[test]
    fn line_numbers_count_crlf_once() {
        assert_eq!(line_of("a\r\nb\rc\u{2028}d", 0), 1);
        assert_eq!(line_of("a\r\nb\rc\u{2028}d", 3), 2);
        assert_eq!(line_of("a\r\nb\rc\u{2028}d", 5), 3);
        assert_eq!(line_of("a\r\nb\rc\u{2028}d", 9), 4);
    }

    #[test]
    fn excerpts_are_cut() {
        assert_eq!(excerpt("ab"), "\"ab\"");
        let long = "x".repeat(EXCERPT_CHARS + 5);
        assert_eq!(
            excerpt(&long),
            format!("{:?}", "x".repeat(EXCERPT_CHARS) + "…")
        );
    }
}
