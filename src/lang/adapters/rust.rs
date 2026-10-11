//! Rust adapter data.

use crate::lang::adapter::{
    AnchorWrapper, BlockDelimiters, DirectiveRow, DocConfig, DocDetect, DocPolicy, LanguageAdapter,
    Scope, SlotKind, SlotReject,
};
use crate::lang::adapters::rows::{lit, raw, raw_family};
use crate::lang::ids::LanguageId;

/// Rust directive rows.
const DIRECTIVES: &[DirectiveRow] = &[
    // clippy's `undocumented_unsafe_blocks` looks for a `SAFETY:` comment
    // directly above the unsafe block.
    lit("SAFETY:", "lint", "directive:safety", Scope::Next),
    // compiletest (rustc's UI-test harness): `//@` header directives, optionally revision-qualified as `//@[rev]`, and `//~` error annotations, both read only at the comment's opener.
    raw_family(
        r"^//@(?:\[[^\]]*\])?\s*(?P<word>[A-Za-z][\w-]*)?",
        "test-harness",
        "directive:compiletest-",
        Scope::File,
    ),
    raw(
        r"^//~",
        "test-harness",
        "directive:compiletest-annotation",
        Scope::Inline,
    ),
];

/// Anchorable declarations (Rust grammar).
const DECLARATION_KINDS: &[&str] = &[
    "function_item",
    "function_signature_item",
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "impl_item",
    "type_item",
    "const_item",
    "static_item",
    "mod_item",
    "macro_definition",
    "field_declaration",
    "enum_variant",
    "let_declaration",
];

/// Block-like containers (Rust grammar).
const CONTAINER_KINDS: &[&str] = &[
    "block",
    "declaration_list",
    "field_declaration_list",
    "enum_variant_list",
];

/// Node kinds named by Rust rule code: the doc-marker children (doc
/// detection is structural, never text), the shebang node, and the
/// attribute items that wrap-anchor to the declaration they precede.
const REFERENCED_KINDS: &[&str] = &[
    "outer_doc_comment_marker",
    "inner_doc_comment_marker",
    "shebang",
    "attribute_item",
];

/// Block comments nest in Rust, so an inner `/*` would unbalance the
/// comment once written back.
const SLOT_REJECTS: &[SlotReject] = &[SlotReject {
    slots: &[SlotKind::Block, SlotKind::Doc],
    pattern: r"/\*",
    reason: "block comments nest in Rust; an inner `/*` would unbalance the comment",
}];

/// The Rust adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Rust,
    comment_node_kinds: &["line_comment", "block_comment"],
    line_prefixes: &["//"],
    line_opener: r"^//[/!]*",
    block_delimiters: Some(BlockDelimiters {
        open: "/*",
        close: "*/",
    }),
    block_comments_nest: true,
    doc: Some(DocConfig {
        detect: DocDetect::RustMarkers,
        policy: DocPolicy::AlwaysProtected,
        functional_tags: &[],
    }),
    directives: DIRECTIVES,
    positional_rules: &[],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: &["}", ")", "]"],
    name_fields: &["name"],
    exported_rule: None,
    anchor_wrappers: &[AnchorWrapper::PrefixSibling {
        kind: "attribute_item",
    }],
    slot_rejects: SLOT_REJECTS,
    referenced_kinds: REFERENCED_KINDS,
};
