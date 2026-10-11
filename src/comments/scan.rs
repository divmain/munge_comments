//! The comment scanner: complete CST enumeration of every comment in a
//! file, with exact byte ranges, kinds, positions, anchors and the token
//! context classification is a pure function of.
//!
//! Enumeration is the design's CST walk with both production cross-checks
//! (soundness against the adapter's comment syntax; completeness against
//! the parse tree's compiled comment query — a violation is
//! [`Error::Invariant`]). The scanner does not classify, group, or assign
//! ids; those happen in the inventory pipeline.

use std::collections::HashMap;

use crate::errors::Error;
use crate::lang::adapter::{AnchorWrapper, DocDetect, ExportedRule, LanguageAdapter};
use crate::lang::adapters::adapter_for;
use crate::lang::parse;
use crate::lang::types::{PlainNode, first_child_with_field, leaves};

use super::types::{Anchor, CommentKind, DeclInfo, FileRef, Position, ScannedComment};

/// The scanner's output.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanOutput {
    /// Every enumerated comment (shebang excluded), in ascending offset
    /// order.
    pub comments: Vec<ScannedComment>,
    /// Whether the file starts with a shebang line (excluded from records,
    /// byte-preserved by construction, and not a code token).
    pub has_shebang: bool,
}

/// Scans `text` under `file.language`'s grammar and enumerates every
/// comment with its full record.
///
/// `file.file_name` is used only for extension-dependent rules such as
/// CommonJS export detection. Refuses a too-deep tree (walking the
/// childless placeholder would silently report no comments).
pub fn scan_file(text: &str, file: FileRef) -> Result<ScanOutput, Error> {
    let adapter = adapter_for(file.language);
    let tree = parse(text, file.language)?;
    if tree.too_deep {
        return Err(Error::Invariant(format!(
            "{}: parse tree exceeds MAX_TREE_DEPTH; refusing to scan",
            file.file_name,
        )));
    }

    // The walk: collect comment-kind nodes without descending into them.
    let mut found: Vec<&PlainNode> = Vec::new();
    collect_comments(&tree.root, adapter, &mut found);

    // Completeness cross-check, before range normalization: the walk's
    // ranges must equal the compiled query's captures — two independent
    // enumeration paths agreeing.
    let mut walk_ranges: Vec<(usize, usize)> =
        found.iter().map(|node| (node.start, node.end)).collect();
    walk_ranges.sort_unstable();
    let mut query_ranges: Vec<(usize, usize)> = tree
        .comment_query_ranges
        .iter()
        .map(|range| (range.start, range.end))
        .collect();
    query_ranges.sort_unstable();
    if walk_ranges != query_ranges {
        return Err(Error::Invariant(format!(
            "{}: scanner walk and comment query disagree ({} vs {} comment ranges)",
            file.file_name,
            walk_ranges.len(),
            query_ranges.len(),
        )));
    }

    // Shebang detection. A `#!` comment node at byte 0 (Python models the
    // shebang as a comment); JavaScript/TypeScript/TSX expose a distinct
    // `hash_bang_line` and Rust a `shebang` node, both at byte 0.
    let comment_shebang = found
        .iter()
        .position(|node| node.start == 0 && text[node.start..].starts_with("#!"));
    let node_shebang = tree
        .root
        .children
        .iter()
        .any(|child| child.start == 0 && matches!(child.kind, "hash_bang_line" | "shebang"));
    let has_shebang = comment_shebang.is_some() || node_shebang;

    // Code tokens: leaf tokens with comment subtrees, grammar-internal
    // hidden (`_`-prefixed) kinds, and the shebang excluded — the token
    // notion the position rules and the verifier share.
    let tokens: Vec<&PlainNode> = leaves(&tree.root, |node| {
        adapter.comment_node_kinds.contains(&node.kind)
            || node.kind.starts_with('_')
            || is_shebang_node(node)
    })
    .collect();

    // Parent links for every node, so anchor and export queries can walk
    // ancestors without threading paths through every helper.
    let parents = parent_map(&tree.root);

    // All normalized comment ranges up front (shebang excluded, EOL-trimmed so a comment's own line terminator is not excised): the snippet builder excises every comment on the anchor's lines, including ones not yet processed.
    let normalized_ranges: Vec<(usize, usize)> = found
        .iter()
        .enumerate()
        .filter(|(index, _)| comment_shebang != Some(*index))
        .map(|(_, node)| trim_eol(text, node.start, node.end))
        .collect();

    let mut comments: Vec<ScannedComment> = Vec::new();
    for (index, &node) in found.iter().enumerate() {
        if comment_shebang == Some(index) {
            continue;
        }
        // Range normalization: a record's range never contains its own
        // line terminator (some grammars include it).
        let (start, end) = trim_eol(text, node.start, node.end);
        let slice = &text[start..end];
        let kind = derive_kind(node, slice, adapter)
            .map_err(|message| Error::Invariant(format!("{}: {message}", file.file_name)))?;

        // 1-based line; 1-based column in Unicode scalar values.
        let line_start = line_start_of(text, start);
        let line = 1 + text[..start].matches('\n').count();
        let column = 1 + text[line_start..start].chars().count();

        // Nearest code token before/after the comment by byte offset; None/EOF when there is none.
        let prev_index = tokens.partition_point(|token| token.end <= start);
        let prev_code_end = if prev_index == 0 {
            None
        } else {
            Some(tokens[prev_index - 1].end)
        };
        let next_index = tokens.partition_point(|token| token.start < end);
        let next_token = tokens.get(next_index);
        let next_code_start = next_token.map_or(text.len(), |token| token.start);

        // Position, pinned: trailing iff non-whitespace non-comment
        // characters precede on the comment's own first line; else dangling
        // iff no following code token or the following token is an
        // anonymous closer; else leading.
        let position = if code_precedes_on_line(text, &comments, start, line_start) {
            Position::Trailing
        } else {
            match next_token {
                None => Position::Dangling,
                Some(token) => {
                    if !token.named && adapter.closing_tokens.contains(&token.kind) {
                        Position::Dangling
                    } else {
                        Position::Leading
                    }
                }
            }
        };

        // Anchor by position: leading uses the chain at the next code token, trailing the preceding node, dangling the enclosing node.
        let anchor_node: Option<&PlainNode> = match position {
            Position::Leading => {
                let chain = chain_at(&tree.root, next_code_start, adapter);
                match choose_from_chain(&chain, adapter) {
                    Some(chosen) => Some(apply_wrappers(chosen, adapter, &parents)),
                    // Empty or all-container chain (e.g. the token is a closing delimiter): fall back to the dangling rule's anchor; position stays leading.
                    None => dangling_anchor_node(node, &tree.root, adapter, &parents),
                }
            }
            Position::Trailing => {
                trailing_anchor_node(&tree.root, adapter, text, start, node.start_row)
                    .or_else(|| dangling_anchor_node(node, &tree.root, adapter, &parents))
            }
            Position::Dangling => dangling_anchor_node(node, &tree.root, adapter, &parents),
        };

        let anchor_name = anchor_node.and_then(|anchor| declaration_name(anchor, adapter, text));
        let anchor = match anchor_node {
            Some(node) => Anchor {
                node_kind: node.kind.to_string(),
                snippet: snippet_for(text, node.start_row, &normalized_ranges),
                name: anchor_name.clone(),
            },
            // The virtual anchor for dangling comments at EOF (and any
            // comment with no enclosing non-container node at all).
            None => Anchor {
                node_kind: "end_of_file".to_string(),
                name: None,
                snippet: String::new(),
            },
        };

        // Exported-ness of the anchor declaration (JavaScript only in v1).
        let anchor_exported = match adapter.exported_rule {
            Some(ExportedRule::JsModule) => js_module_exported(
                anchor_node,
                &tree.root,
                text,
                &file.file_name,
                anchor_name.as_deref(),
                &parents,
            ),
            None => false,
        };

        // Sole content of a `{/* … */}` JSX expression container,
        // structurally: the comment's parent is a `jsx_expression` with no
        // other named children.
        let jsx_container_sole = parents
            .get(&(node as *const PlainNode as usize))
            .is_some_and(|parent| {
                parent.kind == "jsx_expression"
                    && parent
                        .children
                        .iter()
                        .filter(|child| child.named && !std::ptr::eq(*child, node))
                        .count()
                        == 0
            });

        // The root's named child strictly containing this comment, for remove-mode citation matching.
        let enclosing_top_level = tree
            .root
            .children
            .iter()
            .find(|child| child.named && child.start < start && end < child.end)
            .map(|child| DeclInfo {
                node_kind: child.kind.to_string(),
                name: declaration_name(child, adapter, text),
                start: child.start,
                end: child.end,
            });

        comments.push(ScannedComment {
            kind,
            start,
            end,
            line: line as u32,
            column: column as u32,
            text: slice.to_string(),
            position,
            anchor,
            anchor_exported,
            jsx_container_sole,
            prev_code_end,
            next_code_start,
            enclosing_top_level,
        });
    }

    comments.sort_by_key(|comment| comment.start);
    Ok(ScanOutput {
        comments,
        has_shebang,
    })
}

/// Lists every declaration in the tree (full-depth walk), named per the
/// anchor naming rule — the vocabulary remove-mode citations are matched
/// against, and a scanner test oracle.
///
/// `text` is the source the tree was parsed from (names are node text).
pub fn list_declarations(root: &PlainNode, text: &str, adapter: &LanguageAdapter) -> Vec<DeclInfo> {
    let mut declarations = Vec::new();
    visit_declarations(root, text, adapter, &mut declarations);
    declarations
}

fn visit_declarations(
    node: &PlainNode,
    text: &str,
    adapter: &LanguageAdapter,
    out: &mut Vec<DeclInfo>,
) {
    if adapter.declaration_kinds.contains(&node.kind) {
        out.push(DeclInfo {
            node_kind: node.kind.to_string(),
            name: declaration_name(node, adapter, text),
            start: node.start,
            end: node.end,
        });
    }
    for child in &node.children {
        visit_declarations(child, text, adapter, out);
    }
}

/// The walk: pre-order over all nodes, collecting comment kinds and not
/// descending into them (some grammars give comment nodes internal
/// children).
fn collect_comments<'tree>(
    node: &'tree PlainNode,
    adapter: &LanguageAdapter,
    found: &mut Vec<&'tree PlainNode>,
) {
    if adapter.comment_node_kinds.contains(&node.kind) {
        found.push(node);
        return;
    }
    for child in &node.children {
        collect_comments(child, adapter, found);
    }
}

/// A node-level shebang: the dedicated `hash_bang_line`/`shebang` nodes at
/// byte 0.
pub(crate) fn is_shebang_node(node: &PlainNode) -> bool {
    node.start == 0 && matches!(node.kind, "hash_bang_line" | "shebang")
}

/// Trims trailing line terminators (`\r\n`, `\n`, `\r`) off a comment
/// node's range.
fn trim_eol(text: &str, start: usize, mut end: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    while end > start {
        match bytes[end - 1] {
            b'\n' | b'\r' => end -= 1,
            _ => break,
        }
    }
    (start, end)
}

/// Kind derivation, folding in the soundness cross-check: the trimmed
/// comment text must start with a line prefix, or start with the block
/// opener and end with the block closer — otherwise the adapter and the
/// grammar disagree, an invariant. Doc detection (JSDoc prefix, Java
/// `///`, Rust markers) overrides line/block.
fn derive_kind(
    node: &PlainNode,
    slice: &str,
    adapter: &LanguageAdapter,
) -> Result<CommentKind, String> {
    if let Some(doc) = &adapter.doc {
        match doc.detect {
            DocDetect::JsdocPrefix { triple_slash_lines } => {
                let jsdoc = slice.starts_with("/**") && slice.as_bytes().get(3) != Some(&b'/');
                // javac: any `//` + `/` is a Markdown doc line, so a
                // fourth `/` does not disqualify it (unlike `/**/`).
                let triple = triple_slash_lines && slice.starts_with("///");
                if jsdoc || triple {
                    return Ok(CommentKind::Doc);
                }
            }
            DocDetect::RustMarkers => {
                if node.children.iter().any(|child| {
                    matches!(
                        child.kind,
                        "outer_doc_comment_marker" | "inner_doc_comment_marker"
                    )
                }) {
                    return Ok(CommentKind::Doc);
                }
            }
        }
    }
    if adapter.line_prefixes.iter().any(|p| slice.starts_with(p)) {
        return Ok(CommentKind::Line);
    }
    if let Some(delimiters) = adapter.block_delimiters
        && slice.starts_with(delimiters.open)
    {
        if slice.ends_with(delimiters.close) {
            return Ok(CommentKind::Block);
        }
        return Err(format!(
            "comment at {}..{} starts with the block opener but does not end with the closer",
            node.start, node.end,
        ));
    }
    Err(format!(
        "comment at {}..{} matches no comment syntax of this language",
        node.start, node.end,
    ))
}

/// The byte offset where the line containing `offset` starts. Line 1
/// starts *after* a leading U+FEFF: the BOM is an encoding mark, neither
/// code nor indentation, so it never makes a comment trailing, never
/// breaks a group's indent, and never counts toward a column.
pub(crate) fn line_start_of(text: &str, offset: usize) -> usize {
    match text[..offset].rfind('\n') {
        Some(at) => at + 1,
        None => bom_len(text).min(offset),
    }
}

/// The length of a leading U+FEFF (0 when there is none).
fn bom_len(text: &str) -> usize {
    if text.starts_with('\u{FEFF}') {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    }
}

/// Whether non-whitespace, non-comment characters precede `start` on the
/// line starting at `line_start`. Already-normalized comments (from the
/// current scan, in offset order) cover their ranges; characters inside
/// them do not count as code.
fn code_precedes_on_line(
    text: &str,
    earlier: &[ScannedComment],
    start: usize,
    line_start: usize,
) -> bool {
    for (index, character) in text[line_start..start].char_indices() {
        let at = line_start + index;
        if earlier
            .iter()
            .any(|comment| comment.start <= at && at < comment.end)
        {
            continue;
        }
        if !character.is_whitespace() {
            return true;
        }
    }
    false
}

/// Child → parent links for every node in the tree.
fn parent_map(root: &PlainNode) -> HashMap<usize, &PlainNode> {
    let mut parents = HashMap::new();
    fn visit<'tree>(node: &'tree PlainNode, parents: &mut HashMap<usize, &'tree PlainNode>) {
        for child in &node.children {
            parents.insert(child as *const PlainNode as usize, node);
            visit(child, parents);
        }
    }
    visit(root, &mut parents);
    parents
}

/// Whether any ancestor of `node` (excluding `node` itself and the root)
/// has the given kind.
fn has_ancestor_kind(
    node: &PlainNode,
    root: &PlainNode,
    parents: &HashMap<usize, &PlainNode>,
    kind: &str,
) -> bool {
    let mut current = node;
    loop {
        let Some(&parent) = parents.get(&(current as *const PlainNode as usize)) else {
            return false;
        };
        if parent.kind == kind {
            return true;
        }
        if std::ptr::eq(parent, root) {
            return false;
        }
        current = parent;
    }
}

/// The chain of named nodes starting at `offset`, outermost → innermost,
/// skipping comment kinds, grammar-internal hidden kinds, and shebang
/// nodes. The outermost member is found anywhere in the tree (the token may
/// be indented, so it need not be a root child); the rest descend within
/// it.
fn chain_at<'tree>(
    root: &'tree PlainNode,
    offset: usize,
    adapter: &LanguageAdapter,
) -> Vec<&'tree PlainNode> {
    fn usable(node: &PlainNode, adapter: &LanguageAdapter) -> bool {
        node.named
            && !adapter.comment_node_kinds.contains(&node.kind)
            && !node.kind.starts_with('_')
            && !is_shebang_node(node)
    }
    // Pre-order search: the first node starting at the offset is the outermost one (its ancestors, visited earlier, start earlier); the token may be indented, so it need not be a root child.
    fn shallowest_starting<'tree>(
        node: &'tree PlainNode,
        offset: usize,
        adapter: &LanguageAdapter,
    ) -> Option<&'tree PlainNode> {
        if node.start == offset && usable(node, adapter) {
            return Some(node);
        }
        for child in &node.children {
            if let Some(hit) = shallowest_starting(child, offset, adapter) {
                return Some(hit);
            }
        }
        None
    }
    let Some(mut level) = shallowest_starting(root, offset, adapter) else {
        return Vec::new();
    };
    let mut chain = vec![level];
    while let Some(child) = level
        .children
        .iter()
        .find(|child| child.start == offset && usable(child, adapter))
    {
        chain.push(child);
        level = child;
    }
    chain
}

/// The chain rule: the first declaration-kind node, else the outermost
/// non-container node; `None` when the chain is empty or all containers.
fn choose_from_chain<'tree>(
    chain: &[&'tree PlainNode],
    adapter: &LanguageAdapter,
) -> Option<&'tree PlainNode> {
    chain
        .iter()
        .copied()
        .find(|node| adapter.declaration_kinds.contains(&node.kind))
        .or_else(|| {
            chain
                .iter()
                .copied()
                .find(|node| !adapter.container_kinds.contains(&node.kind))
        })
}

/// Applies the adapter's anchor wrappers to a chain-chosen node:
/// `Field` descends into the wrapper's field child; `PrefixSibling`
/// moves to the next named sibling that is neither a wrapper kind nor a
/// comment. A non-matching (or field-less) wrapper leaves the node as-is.
fn apply_wrappers<'tree>(
    node: &'tree PlainNode,
    adapter: &LanguageAdapter,
    parents: &HashMap<usize, &'tree PlainNode>,
) -> &'tree PlainNode {
    for wrapper in adapter.anchor_wrappers {
        match *wrapper {
            AnchorWrapper::Field { kind, field } => {
                if node.kind == kind
                    && let Some(child) = first_child_with_field(node, field)
                {
                    return child;
                }
            }
            AnchorWrapper::PrefixSibling { kind } => {
                if node.kind == kind
                    && let Some(&parent) = parents.get(&(node as *const PlainNode as usize))
                {
                    let mut after = false;
                    for sibling in &parent.children {
                        if std::ptr::eq(sibling, node) {
                            after = true;
                            continue;
                        }
                        if !after {
                            continue;
                        }
                        if sibling.named
                            && sibling.kind != kind
                            && !adapter.comment_node_kinds.contains(&sibling.kind)
                        {
                            return sibling;
                        }
                    }
                }
            }
        }
    }
    node
}

/// The dangling rule: the innermost enclosing named node (below the root)
/// that is neither a container nor a comment nor grammar-internal; `None`
/// when there is none (the virtual `end_of_file` anchor then).
fn dangling_anchor_node<'tree>(
    comment: &PlainNode,
    root: &PlainNode,
    adapter: &LanguageAdapter,
    parents: &HashMap<usize, &'tree PlainNode>,
) -> Option<&'tree PlainNode> {
    let mut current = comment;
    loop {
        let &parent = parents.get(&(current as *const PlainNode as usize))?;
        if std::ptr::eq(parent, root) {
            return None;
        }
        if parent.named
            && !adapter.container_kinds.contains(&parent.kind)
            && !adapter.comment_node_kinds.contains(&parent.kind)
            && !parent.kind.starts_with('_')
            && !is_shebang_node(parent)
        {
            return Some(parent);
        }
        current = parent;
    }
}

/// The trailing rule: the outermost named node (root excluded) that is
/// neither a container nor a comment, ends at or before the comment, and
/// ends on the comment's own first line. Pre-order search returns the
/// outermost candidate first.
///
/// A node whose range includes its own line terminator (C/C++ preprocessor
/// directives end after their `\n`) ends on the line it terminates, not at
/// column 0 of the next one — otherwise every trailing comment on the line
/// after an `#include`/`#define` would anchor to that directive.
fn trailing_anchor_node<'tree>(
    root: &'tree PlainNode,
    adapter: &LanguageAdapter,
    text: &str,
    comment_start: usize,
    comment_row: usize,
) -> Option<&'tree PlainNode> {
    fn visit<'tree>(
        node: &'tree PlainNode,
        adapter: &LanguageAdapter,
        text: &str,
        comment_start: usize,
        comment_row: usize,
    ) -> Option<&'tree PlainNode> {
        if node.named
            && !adapter.container_kinds.contains(&node.kind)
            && !adapter.comment_node_kinds.contains(&node.kind)
            && !node.kind.starts_with('_')
            && !is_shebang_node(node)
            && node.end <= comment_start
            && last_row(node, text) == comment_row
        {
            return Some(node);
        }
        for child in &node.children {
            if let Some(hit) = visit(child, adapter, text, comment_start, comment_row) {
                return Some(hit);
            }
        }
        None
    }
    visit(root, adapter, text, comment_start, comment_row)
}

/// The 0-based row of a node's last character, its own trailing line
/// terminator(s) excluded.
fn last_row(node: &PlainNode, text: &str) -> usize {
    let (_, end) = trim_eol(text, node.start, node.end);
    node.end_row - text[end..node.end].matches('\n').count()
}

/// The naming rule: for each name field, the anchor's own field child;
/// descend while the reached child itself carries a listed field; the name
/// is the finally reached child's text. An anchor with no listed field
/// (a wrapper shape like `lexical_declaration`) retries on its first child
/// in `declaration_kinds`.
pub(crate) fn declaration_name(
    node: &PlainNode,
    adapter: &LanguageAdapter,
    text: &str,
) -> Option<String> {
    for field in adapter.name_fields {
        if let Some(child) = first_child_with_field(node, field) {
            let mut reached = child;
            loop {
                let mut descended = false;
                for field in adapter.name_fields {
                    if let Some(next) = first_child_with_field(reached, field) {
                        reached = next;
                        descended = true;
                        break;
                    }
                }
                if !descended {
                    break;
                }
            }
            return Some(text[reached.start..reached.end].to_string());
        }
    }
    if let Some(child) = node
        .children
        .iter()
        .find(|child| child.named && adapter.declaration_kinds.contains(&child.kind))
    {
        return declaration_name(child, adapter, text);
    }
    None
}

/// The anchor snippet: the first 3 lines of the original text starting at
/// the anchor's line, with every comment range on those lines excised,
/// each line right-trimmed, blank lines dropped.
fn snippet_for(text: &str, anchor_row: usize, comment_ranges: &[(usize, usize)]) -> String {
    let mut lines = Vec::new();
    let mut row = 0;
    // Line 1 starts after a leading BOM (see [`line_start_of`]).
    let mut offset = bom_len(text);
    for raw_line in text[offset..].split('\n') {
        let line_start = offset;
        let line_end = line_start + raw_line.len();
        offset = line_end + 1;
        if row < anchor_row {
            row += 1;
            continue;
        }
        // The window is the first 3 raw lines from the anchor's line;
        // blanks are dropped only after excision and trimming.
        if row >= anchor_row + 3 {
            break;
        }
        // Excise the portions of comment ranges intersecting this line.
        let mut covered: Vec<(usize, usize)> = comment_ranges
            .iter()
            .filter_map(|&(start, end)| {
                let from = start.max(line_start);
                let to = end.min(line_end);
                (from < to).then_some((from, to))
            })
            .collect();
        covered.sort_unstable();
        let mut kept = String::new();
        let mut cursor = line_start;
        for (from, to) in covered {
            if from > cursor {
                kept.push_str(&text[cursor..from]);
            }
            cursor = to.max(cursor);
        }
        kept.push_str(&text[cursor.min(line_end)..line_end]);
        let trimmed = kept.trim_end();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
        row += 1;
    }
    lines.join("\n")
}

/// JavaScript's `JsModule` exported-ness rule over the anchor declaration
/// (after wrapper unwrapping): an `export_statement` ancestor; or, in
/// `.js`/`.cjs` files, the anchor's name being exported via any CommonJS
/// shape; or the anchor itself being the top-level
/// `module.exports`/`module.exports.<name>`/`exports.<name>` assignment
/// the comment is written on.
fn js_module_exported(
    anchor: Option<&PlainNode>,
    root: &PlainNode,
    text: &str,
    file_name: &str,
    anchor_name: Option<&str>,
    parents: &HashMap<usize, &PlainNode>,
) -> bool {
    let Some(anchor) = anchor else {
        return false;
    };
    if has_ancestor_kind(anchor, root, parents, "export_statement") {
        return true;
    }
    if !(file_name.ends_with(".js") || file_name.ends_with(".cjs")) {
        return false;
    }
    // JSDoc written directly on the export assignment.
    if anchor.kind == "expression_statement"
        && root
            .children
            .iter()
            .any(|child| std::ptr::eq(child, anchor))
        && let Some(target) = assignment_target(anchor)
        && is_module_exports_target(target, text)
    {
        return true;
    }
    // The anchor's name exported via any CommonJS shape.
    let Some(name) = anchor_name else {
        return false;
    };
    for statement in &root.children {
        if statement.kind != "expression_statement" {
            continue;
        }
        let Some(assignment) = statement
            .children
            .iter()
            .find(|child| child.named && child.kind == "assignment_expression")
        else {
            continue;
        };
        let (Some(lhs), Some(rhs)) = (
            first_child_with_field(assignment, "left"),
            first_child_with_field(assignment, "right"),
        ) else {
            continue;
        };
        // module.exports.X = … / exports.X = …
        if let Some(exported) = member_exports_name(lhs, text)
            && exported == name
        {
            return true;
        }
        if is_module_exports(lhs, text) {
            // module.exports = X
            if rhs.kind == "identifier" && node_text(rhs, text) == name {
                return true;
            }
            // module.exports = { …, X, … } (shorthand or X: X)
            if rhs.kind == "object" {
                for property in &rhs.children {
                    if property.kind == "shorthand_property_identifier"
                        && node_text(property, text) == name
                    {
                        return true;
                    }
                    if property.kind == "pair" {
                        let (Some(key), Some(value)) = (
                            first_child_with_field(property, "key"),
                            first_child_with_field(property, "value"),
                        ) else {
                            continue;
                        };
                        if is_plain_identifier(key)
                            && is_plain_identifier(value)
                            && node_text(key, text) == name
                            && node_text(value, text) == name
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// The assignment target of a top-level `expression_statement`, if it is
/// an assignment.
fn assignment_target(statement: &PlainNode) -> Option<&PlainNode> {
    let assignment = statement
        .children
        .iter()
        .find(|child| child.named && child.kind == "assignment_expression")?;
    first_child_with_field(assignment, "left")
}

/// `module.exports`, `module.exports.<name>` or `exports.<name>`.
fn is_module_exports_target(target: &PlainNode, text: &str) -> bool {
    is_module_exports(target, text) || member_exports_name(target, text).is_some()
}

/// `module.exports` exactly (object `module`, property `exports`). The
/// property is a `property_identifier` in tree-sitter-javascript.
fn is_module_exports(node: &PlainNode, text: &str) -> bool {
    if node.kind != "member_expression" {
        return false;
    }
    match (
        first_child_with_field(node, "object"),
        first_child_with_field(node, "property"),
    ) {
        (Some(object), Some(property)) => {
            object.kind == "identifier"
                && node_text(object, text) == "module"
                && is_plain_identifier(property)
                && node_text(property, text) == "exports"
        }
        _ => false,
    }
}

/// An identifier-shaped node (`identifier` or `property_identifier`).
fn is_plain_identifier(node: &PlainNode) -> bool {
    matches!(node.kind, "identifier" | "property_identifier")
}

/// The exported name of an `exports.<X>` or `module.exports.<X>` member
/// expression, if the shape matches.
fn member_exports_name<'a>(node: &PlainNode, text: &'a str) -> Option<&'a str> {
    if node.kind != "member_expression" {
        return None;
    }
    let (object, property) = (
        first_child_with_field(node, "object")?,
        first_child_with_field(node, "property")?,
    );
    if !is_plain_identifier(property) {
        return None;
    }
    if object.kind == "identifier" && node_text(object, text) == "exports" {
        return Some(node_text(property, text));
    }
    if is_module_exports(object, text) {
        return Some(node_text(property, text));
    }
    None
}

fn node_text<'a>(node: &PlainNode, text: &'a str) -> &'a str {
    &text[node.start..node.end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ids::LanguageId;

    fn scan(text: &str, language: LanguageId, file_name: &str) -> ScanOutput {
        scan_file(
            text,
            FileRef {
                language,
                file_name: file_name.to_string(),
            },
        )
        .expect("test snippets parse and scan")
    }

    fn texts(output: &ScanOutput) -> Vec<&str> {
        output.comments.iter().map(|c| c.text.as_str()).collect()
    }

    fn js(text: &str) -> ScanOutput {
        scan(text, LanguageId::Javascript, "a.js")
    }

    #[test]
    fn js_strings_urls_templates_and_regexes_yield_no_phantom_comments() {
        let text = "const url = \"http://x//y\"; // real\n\
                    const t = `a ${x /* in tpl */} b`;\n\
                    const r = /\\/\\//;\n\
                    const u = 'proto://x#y';\n";
        let output = js(text);
        assert_eq!(texts(&output), ["// real", "/* in tpl */"]);
        // Byte offsets are exact: each comment starts where it appears.
        assert_eq!(
            output.comments[0].start,
            text.find("// real").expect("present"),
        );
        assert_eq!(output.comments[0].line, 1);
        assert_eq!(output.comments[1].line, 2);

        // Python: '#' inside a string literal is not a comment.
        let text = "s = \"# no comment\"  # yes\n";
        let output = scan(text, LanguageId::Python, "a.py");
        assert_eq!(texts(&output), ["# yes"]);
    }

    #[test]
    fn multiline_and_raw_strings_yield_no_phantom_comments() {
        // Python triple-quoted strings (incl. docstrings) hide `#` text.
        let text = "def f():\n    \"\"\"Doc # not a comment.\"\"\"\n    s = '''a\n# nor this\n'''  # real\n    return s\n";
        let output = scan(text, LanguageId::Python, "a.py");
        assert_eq!(texts(&output), ["# real"]);

        // Rust raw strings hide `//` and `/* */` text.
        let text = "fn main() {\n    let s = r#\"// not a comment /* nor this */\"#; // real\n}\n";
        let output = scan(text, LanguageId::Rust, "a.rs");
        assert_eq!(texts(&output), ["// real"]);

        // Go raw (backquoted) strings hide `//` and `/* */` text, even
        // across lines.
        let text =
            "package p\n\nvar s = `// not a comment\n/* nor this */`\n\n// real\nvar t = 1\n";
        let output = scan(text, LanguageId::Go, "a.go");
        assert_eq!(texts(&output), ["// real"]);
        assert_eq!(
            output.comments[0].start,
            text.find("// real").expect("present"),
        );
    }

    #[test]
    fn tsx_jsx_text_is_not_comments_but_containers_are() {
        let text = "const a = <div>// not a comment</div>;\n\
                    const b = <div>{/* sole */}</div>;\n\
                    const c = <T,>(x) => x; // generic arrow\n";
        let output = scan(text, LanguageId::Tsx, "a.tsx");
        assert_eq!(texts(&output), ["/* sole */", "// generic arrow"]);
        let sole = &output.comments[0];
        assert!(sole.jsx_container_sole);
        // Code (`const b = <div>{`) precedes the container comment on its
        // own line: pinned trailing.
        assert_eq!(sole.position, Position::Trailing);
        let trailing = &output.comments[1];
        assert!(!trailing.jsx_container_sole);
        assert_eq!(trailing.position, Position::Trailing);

        // An own-line container comment: the container's `{` still
        // precedes it on its line, so the pinned position rule says
        // trailing — `jsx_container_sole` is orthogonal to position.
        let own_line = "const b = <div>\n  {/* sole */}\n</div>;\n";
        let output = scan(own_line, LanguageId::Tsx, "a.tsx");
        assert_eq!(texts(&output), ["/* sole */"]);
        assert!(output.comments[0].jsx_container_sole);
        assert_eq!(output.comments[0].position, Position::Trailing);
    }

    #[test]
    fn rust_block_comments_nest_c_comments_do_not() {
        let rust = "/* outer /* inner */ still */\nfn f() {}\n";
        let output = scan(rust, LanguageId::Rust, "a.rs");
        assert_eq!(texts(&output), ["/* outer /* inner */ still */"]);

        let c = "/* /* */ x;\n";
        let output = scan(c, LanguageId::C, "a.c");
        assert_eq!(texts(&output), ["/* /* */"]);
    }

    #[test]
    fn empty_comments_and_eof_comments_are_enumerated() {
        let output = js("x;\n//\n/** doc */\n");
        assert_eq!(texts(&output), ["//", "/** doc */"]);
        assert_eq!(output.comments[1].kind, CommentKind::Doc);

        // The acceptance pair: a trailing /** doc */ plus // x at EOF.
        let text = "x = 1; /** doc */ // x";
        let output = js(text);
        assert_eq!(texts(&output), ["/** doc */", "// x"]);
        // Both have code before them on the line (the `{`-free code
        // `x = 1;`): both trailing, and both enumerated to the very end of
        // the file.
        for comment in &output.comments {
            assert_eq!(comment.position, Position::Trailing);
            assert_eq!(comment.next_code_start, text.len());
        }
        assert_eq!(output.comments[0].kind, CommentKind::Doc);
    }

    #[test]
    fn a_leading_bom_is_neither_code_nor_column() {
        // Line 1 starts after the BOM: a comment right after it is a
        // leading comment at column 1, anchored to what follows — not a
        // trailing comment of an invisible "code" character.
        let text = "\u{FEFF}// header\nfunction f() {}\n";
        let output = js(text);
        let comment = &output.comments[0];
        assert_eq!(comment.start, '\u{FEFF}'.len_utf8());
        assert_eq!((comment.line, comment.column), (1, 1));
        assert_eq!(comment.position, Position::Leading);
        assert_eq!(comment.anchor.node_kind, "function_declaration");
        assert_eq!(comment.anchor.name.as_deref(), Some("f"));

        // An anchor on line 1 gets a BOM-free snippet.
        let output = js("\u{FEFF}let x = 1; // c\n");
        assert_eq!(output.comments[0].position, Position::Trailing);
        assert_eq!(output.comments[0].anchor.snippet, "let x = 1;");
    }

    #[test]
    fn bom_crlf_and_multibyte_text_keep_byte_offsets_exact() {
        let text = "\u{FEFF}const s = '日本語'; // c\r\nlet y = 2; // d\r\n";
        let output = js(text);
        assert_eq!(texts(&output), ["// c", "// d"]);
        let first = &output.comments[0];
        assert_eq!(first.start, text.find("// c").expect("present"));
        // The CRLF is trimmed out of the range: end == start + len("// c").
        assert_eq!(first.end, first.start + 4);
        assert_eq!(first.line, 1);
        // Column counts Unicode scalar values from the line's start, and
        // line 1 starts after the BOM: "const s = '日本語'; " is 17
        // scalars (the closing quote included).
        assert_eq!(first.column, 18);
        assert_eq!(output.comments[1].line, 2);
    }

    #[test]
    fn position_rules_and_their_anchors() {
        // Leading, anchored to the following declaration.
        let output = js("// leading\nfunction f() {}\n");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Leading);
        assert_eq!(comment.anchor.node_kind, "function_declaration");
        assert_eq!(comment.anchor.name.as_deref(), Some("f"));
        assert_eq!(comment.anchor.snippet, "function f() {}");

        // Trailing, anchored to the outermost node ending on its line.
        let output = js("foo(); // trailing\n");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Trailing);
        assert_eq!(comment.anchor.node_kind, "expression_statement");
        assert_eq!(comment.anchor.name, None);

        // Dangling before an anonymous closing token, anchored to the
        // enclosing non-container node.
        let output = js("function f() {\n  x();\n  // dangling\n}\n");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Dangling);
        assert_eq!(comment.anchor.node_kind, "function_declaration");
        assert_eq!(comment.anchor.name.as_deref(), Some("f"));

        // Dangling at EOF: the virtual end_of_file anchor.
        let output = js("x;\n// eof\n");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Dangling);
        assert_eq!(comment.anchor.node_kind, "end_of_file");

        // Leading across blank lines.
        let output = js("// far\n\n\nx = 1;\n");
        assert_eq!(output.comments[0].position, Position::Leading);
    }

    #[test]
    fn mts_and_cts_scan_under_the_typescript_grammar() {
        for name in ["a.mts", "a.cts"] {
            let output = scan("// c\nexport const x = 1;\n", LanguageId::Typescript, name);
            assert_eq!(texts(&output), ["// c"]);
            // export_statement unwraps to lexical_declaration, named via
            // its variable_declarator child.
            assert_eq!(output.comments[0].anchor.node_kind, "lexical_declaration");
            assert_eq!(output.comments[0].anchor.name.as_deref(), Some("x"));
        }
    }

    #[test]
    fn python_shebang_is_excluded_and_not_a_code_token() {
        let text = "#!/usr/bin/env python3\n# normal\nx = 1\n";
        let output = scan(text, LanguageId::Python, "a.py");
        assert!(output.has_shebang);
        assert_eq!(texts(&output), ["# normal"]);
        let comment = &output.comments[0];
        assert_eq!(comment.prev_code_end, None, "the shebang is not code");
        assert_eq!(comment.position, Position::Leading);
        assert_eq!(comment.anchor.node_kind, "assignment");
        // Python's `assignment` has no `name` field (its target is `left`),
        // so the naming rule yields no name here.
        assert_eq!(comment.anchor.name, None);

        // Indented comment-only lines inside a suite.
        let output = scan(
            "def f():\n    # inner\n    x = 1\n",
            LanguageId::Python,
            "a.py",
        );
        assert_eq!(texts(&output), ["# inner"]);
        assert_eq!(output.comments[0].position, Position::Leading);
        assert_eq!(output.comments[0].anchor.node_kind, "assignment");
    }

    #[test]
    fn go_build_tag_lines_are_comments() {
        let text = "//go:build linux\n// +build linux\n\npackage main\n\nfunc main() {}\n";
        let output = scan(text, LanguageId::Go, "a.go");
        assert_eq!(texts(&output), ["//go:build linux", "// +build linux"]);
        assert!(
            output
                .comments
                .iter()
                .all(|c| c.position == Position::Leading)
        );
    }

    #[test]
    fn go_example_function_body_is_the_enclosing_top_level() {
        let text =
            "package main\n\nfunc ExampleFoo() {\n\tfmt.Println(1)\n\t// Output:\n\t// 1\n}\n";
        let output = scan(text, LanguageId::Go, "a.go");
        let output_comment = &output.comments[0];
        let enclosing = output_comment
            .enclosing_top_level
            .as_ref()
            .expect("enclosed by the example function");
        assert_eq!(enclosing.node_kind, "function_declaration");
        assert_eq!(enclosing.name.as_deref(), Some("ExampleFoo"));
    }

    #[test]
    fn rust_shebang_node_is_excluded_and_doc_ranges_are_trimmed() {
        let text = "#!/usr/bin/env rust\n// code\nfn main() {}\n";
        let output = scan(text, LanguageId::Rust, "a.rs");
        assert!(output.has_shebang);
        assert_eq!(texts(&output), ["// code"]);
        assert_eq!(output.comments[0].prev_code_end, None);

        let text = "/// doc\nfn f() {}\n";
        let output = scan(text, LanguageId::Rust, "a.rs");
        let comment = &output.comments[0];
        assert_eq!(comment.kind, CommentKind::Doc);
        assert_eq!(comment.text, "/// doc");
        assert_eq!(comment.end, comment.start + "/// doc".len());

        // //// is not a doc comment in Rust (node structure, not text).
        let output = scan("//// not doc\nfn f() {}\n", LanguageId::Rust, "a.rs");
        assert_eq!(output.comments[0].kind, CommentKind::Line);
    }

    #[test]
    fn comment_after_a_js_hashbang_has_no_previous_code_token() {
        let text = "#!/usr/bin/env node\n// real\nx;\n";
        let output = js(text);
        assert!(output.has_shebang);
        assert_eq!(texts(&output), ["// real"]);
        assert_eq!(output.comments[0].prev_code_end, None);
    }

    #[test]
    fn leading_before_an_anonymous_non_closing_token_uses_the_dangling_rule() {
        // The next token is `}` — anonymous, but Python has no closing
        // tokens, so the comment is leading; no named node starts at `}`,
        // so the chain is empty and the dangling rule's anchor applies
        // while the position stays leading.
        let text = "x = {\n# c\n}\n";
        let output = scan(text, LanguageId::Python, "a.py");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Leading);
        assert_eq!(comment.anchor.node_kind, "dictionary");
    }

    #[test]
    fn c_define_line_comments_are_swallowed_but_trailing_blocks_are_real() {
        let output = scan("#define MAX 100 // swallowed\n", LanguageId::C, "a.c");
        assert_eq!(texts(&output), Vec::<&str>::new());

        let output = scan("#define MAX 100 /* trailing */\n", LanguageId::C, "a.c");
        assert_eq!(texts(&output), ["/* trailing */"]);
    }

    #[test]
    fn trailing_anchors_skip_preprocessor_lines_ending_in_their_newline() {
        // A preprocessor directive's range ends after its `\n` (row + 1,
        // column 0); it still ends on its own line, so the trailing comment
        // on the next line anchors to that line's code.
        let output = scan("#include <stdio.h>\nint x; // c\n", LanguageId::C, "a.c");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Trailing);
        assert_eq!(comment.anchor.node_kind, "declaration");
        assert_eq!(comment.anchor.name.as_deref(), Some("x"));

        // A trailing block on the next `#define` line never anchors to the
        // previous directive.
        let output = scan("#define A 1\n#define B 2 /* c */\n", LanguageId::Cpp, "a.h");
        let comment = &output.comments[0];
        assert_eq!(comment.position, Position::Trailing);
        assert_ne!(comment.anchor.name.as_deref(), Some("A"));
        assert!(comment.anchor.snippet.starts_with("#define B 2"));
    }

    #[test]
    fn declaration_first_anchors_unwrap_wrappers() {
        // Exported TS function: export_statement → function_declaration.
        let output = scan(
            "// c\nexport function f() {}\n",
            LanguageId::Typescript,
            "a.ts",
        );
        assert_eq!(output.comments[0].anchor.node_kind, "function_declaration");
        assert_eq!(output.comments[0].anchor.name.as_deref(), Some("f"));

        // Decorated Python def: decorated_definition → function_definition.
        let output = scan(
            "# c\n@decorator\ndef f():\n    pass\n",
            LanguageId::Python,
            "a.py",
        );
        assert_eq!(output.comments[0].anchor.node_kind, "function_definition");
        assert_eq!(output.comments[0].anchor.name.as_deref(), Some("f"));

        // Rust attributes: attribute_item run → the following struct_item.
        let output = scan(
            "// c\n#[derive(Debug)]\nstruct S;\n",
            LanguageId::Rust,
            "a.rs",
        );
        assert_eq!(output.comments[0].anchor.node_kind, "struct_item");
        assert_eq!(output.comments[0].anchor.name.as_deref(), Some("S"));

        // C function names come through the declarator field chain.
        let output = scan(
            "/* c */\nint main(void) { return 0; }\n",
            LanguageId::C,
            "a.c",
        );
        assert_eq!(output.comments[0].anchor.node_kind, "function_definition");
        assert_eq!(output.comments[0].anchor.name.as_deref(), Some("main"));
    }

    #[test]
    fn js_anchor_exported_covers_every_pinned_js_module_shape() {
        let exported = |text: &str, file_name: &str| -> bool {
            let output = scan(text, LanguageId::Javascript, file_name);
            output.comments[0].anchor_exported
        };

        // ESM export: true.
        assert!(exported("// c\nexport function f() {}\n", "a.js"));
        // Unexported: false.
        assert!(!exported("// c\nfunction g() {}\n", "a.js"));
        // module.exports.x = …
        assert!(exported(
            "// c\nfunction x() {}\nmodule.exports.x = x;\n",
            "a.js"
        ));
        // exports.x = …
        assert!(exported("// c\nfunction x() {}\nexports.x = x;\n", "a.js"));
        assert!(exported(
            "// c\nfunction x() {}\nmodule.exports = x;\n",
            "a.js"
        ));
        // module.exports = { x } (shorthand)
        assert!(exported(
            "// c\nfunction x() {}\nmodule.exports = { x };\n",
            "a.js"
        ));
        // JSDoc written directly on the export assignment.
        assert!(exported("/** jsdoc */\nmodule.exports.f = f;\n", "a.js"));
        // The CommonJS shapes do not export anything in an .mjs file.
        assert!(!exported(
            "// c\nfunction x() {}\nmodule.exports.x = x;\n",
            "a.mjs"
        ));
        assert!(!exported(
            "// c\nfunction x() {}\nmodule.exports = x;\n",
            "a.mjs"
        ));
    }

    #[test]
    fn java_doc_kinds_are_detected() {
        // javac reads any `//` followed by a third `/` as a Markdown doc
        // line, so `////` is doc in Java (unlike Rust).
        let text = "/// markdown doc\n//// also doc\n/** javadoc */\n// plain\nclass A {}\n";
        let output = scan(text, LanguageId::Java, "A.java");
        let kinds: Vec<CommentKind> = output.comments.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                CommentKind::Doc,
                CommentKind::Doc,
                CommentKind::Doc,
                CommentKind::Line
            ]
        );
    }

    #[test]
    fn cpp_raw_string_in_a_header_has_no_phantom_comment() {
        let text = "auto s = R\"(a\" /* x */ )\";\nint y;\n";
        let output = scan(text, LanguageId::Cpp, "header.h");
        assert_eq!(
            texts(&output),
            Vec::<&str>::new(),
            "the /* x */ inside the raw string must not enumerate",
        );
    }

    #[test]
    fn snippets_excise_comments_right_trim_and_drop_blanks() {
        let text = "// above\nfunction f() {\n  x(); // trailing\n  // dangling\n}\n";
        let output = js(text);
        assert_eq!(output.comments[0].anchor.snippet, "function f() {\n  x();");
    }

    #[test]
    fn list_declarations_names_every_declaration() {
        let text = "const x = 1;\nfunction f() {}\nclass C {}\n";
        let output = js(text);
        let tree = crate::lang::parse(text, LanguageId::Javascript).expect("parses");
        let adapter = adapter_for(LanguageId::Javascript);
        let declarations = list_declarations(&tree.root, text, adapter);
        let shapes: Vec<(&str, Option<&str>)> = declarations
            .iter()
            .map(|d| (d.node_kind.as_str(), d.name.as_deref()))
            .collect();
        assert!(shapes.contains(&("lexical_declaration", Some("x"))));
        assert!(shapes.contains(&("function_declaration", Some("f"))));
        assert!(shapes.contains(&("class_declaration", Some("C"))));
        // Sanity against the scan itself: every comment's anchor name is a
        // declaration the listing knows.
        assert_eq!(output.comments.len(), 0);
    }

    #[test]
    fn a_too_deep_tree_is_refused() {
        let text = format!("x = {}1{};", "(".repeat(2000), ")".repeat(2000));
        let err = scan_file(
            &text,
            FileRef {
                language: LanguageId::Javascript,
                file_name: "deep.js".to_string(),
            },
        )
        .expect_err("too-deep trees are refused");
        assert_eq!(err.exit_code(), 7);
    }
}
