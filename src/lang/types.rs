//! The plain-data tree contract: the only tree representation code outside
//! the grammar registry ever sees.
//!
//! The scanner, anchor extraction, chunker, aligner and verifier all consume
//! [`PlainNode`]/[`PlainTree`]; offsets are UTF-8 byte offsets into the
//! original text, rows are 0-based and columns count bytes.

use std::ops::Range;

/// Maximum tree depth the tool will walk. A deeper tree is refused up front
/// (see [`PlainTree::too_deep`]) rather than risk overflowing a recursive
/// consumer's stack.
pub const MAX_TREE_DEPTH: usize = 1024;

/// A parse-tree node as plain data: no grammar objects, no lifetimes beyond
/// `'static` kind/field names.
#[derive(Debug, Clone, PartialEq)]
pub struct PlainNode {
    /// Node type name, from the language's symbol table.
    pub kind: &'static str,
    /// Numeric node kind id (tree-sitter symbol).
    pub kind_id: u16,
    /// Whether this node is a named node.
    pub named: bool,
    /// Start byte offset (UTF-8) in the parsed text.
    pub start: usize,
    /// End byte offset (UTF-8) in the parsed text.
    pub end: usize,
    /// 0-based start row.
    pub start_row: usize,
    /// 0-based start column, in bytes.
    pub start_col: usize,
    /// 0-based end row.
    pub end_row: usize,
    /// Field under which the parent holds this child, if any.
    pub field_name: Option<&'static str>,
    /// Whether this node is an `ERROR` node.
    pub is_error: bool,
    /// Whether this node is a `MISSING` node.
    pub is_missing: bool,
    /// All children, named and anonymous, extras included, in document
    /// order.
    pub children: Vec<PlainNode>,
}

/// A whole parse tree as plain data.
#[derive(Debug, Clone, PartialEq)]
pub struct PlainTree {
    /// The root node.
    pub root: PlainNode,
    /// Whether the tree contains any `ERROR` or `MISSING` node.
    pub has_error: bool,
    /// Whether the source tree exceeded [`MAX_TREE_DEPTH`]; the root is
    /// then childless and consumers must refuse rather than walk.
    pub too_deep: bool,
    /// Comment-node ranges found by the compiled completeness query, used
    /// to cross-check the scanner's own walk.
    pub comment_query_ranges: Vec<Range<usize>>,
}

/// The first child held under `field`, recovering
/// `child_by_field_name` semantics on plain data.
pub fn first_child_with_field<'a>(node: &'a PlainNode, field: &str) -> Option<&'a PlainNode> {
    node.children
        .iter()
        .find(|child| child.field_name == Some(field))
}

/// A pre-order iterator over the leaf nodes of `node`'s subtree, skipping
/// every subtree whose root satisfies `skip` (that node is neither yielded
/// nor descended into). Leaves are nodes with no children, named or
/// anonymous alike.
pub fn leaves<'a>(
    node: &'a PlainNode,
    skip: impl Fn(&PlainNode) -> bool + 'a,
) -> impl Iterator<Item = &'a PlainNode> + 'a {
    let mut stack = vec![node];
    std::iter::from_fn(move || {
        while let Some(current) = stack.pop() {
            if skip(current) {
                continue;
            }
            if current.children.is_empty() {
                return Some(current);
            }
            // Push in reverse so the next pop is the first child:
            // pre-order, left to right.
            stack.extend(current.children.iter().rev());
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(kind: &'static str, field: Option<&'static str>) -> PlainNode {
        PlainNode {
            kind,
            kind_id: 1,
            named: true,
            start: 0,
            end: 1,
            start_row: 0,
            start_col: 0,
            end_row: 0,
            field_name: field,
            is_error: false,
            is_missing: false,
            children: Vec::new(),
        }
    }

    fn branch(kind: &'static str, children: Vec<PlainNode>) -> PlainNode {
        PlainNode {
            children,
            ..leaf(kind, None)
        }
    }

    #[test]
    fn first_child_with_field_returns_the_first_matching_child() {
        let a = leaf("a", Some("left"));
        let b = leaf("b", Some("left"));
        let c = leaf("c", Some("right"));
        let node = branch("parent", vec![a, b, c]);
        assert_eq!(first_child_with_field(&node, "left").unwrap().kind, "a");
        assert_eq!(first_child_with_field(&node, "right").unwrap().kind, "c");
        assert!(first_child_with_field(&node, "absent").is_none());
    }

    #[test]
    fn leaves_walks_pre_order_and_yields_only_childless_nodes() {
        let x = leaf("x", None);
        let inner = branch("inner", vec![leaf("y", None), leaf("z", None)]);
        let root = branch("root", vec![x, inner, leaf("w", None)]);
        let kinds: Vec<_> = leaves(&root, |_| false).map(|node| node.kind).collect();
        assert_eq!(kinds, ["x", "y", "z", "w"]);
    }

    #[test]
    fn leaves_skips_whole_subtrees() {
        let inner = branch("inner", vec![leaf("y", None), leaf("z", None)]);
        let root = branch("root", vec![leaf("x", None), inner, leaf("w", None)]);
        let kinds: Vec<_> = leaves(&root, |node| node.kind == "inner")
            .map(|node| node.kind)
            .collect();
        assert_eq!(kinds, ["x", "w"]);
    }

    #[test]
    fn leaves_can_skip_the_root_itself() {
        let root = branch("root", vec![leaf("x", None)]);
        assert_eq!(leaves(&root, |_| true).count(), 0);
    }
}
