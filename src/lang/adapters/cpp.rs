//! C++ adapter data: the C vocabulary plus C++ declarations and containers.

use crate::lang::adapter::{BlockDelimiters, LanguageAdapter};
use crate::lang::ids::LanguageId;

/// Anchorable declarations (C++ grammar): the C set plus C++ declarations.
const DECLARATION_KINDS: &[&str] = &[
    "function_definition",
    "declaration",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
    "type_definition",
    "field_declaration",
    "enumerator",
    "preproc_def",
    "preproc_function_def",
    "class_specifier",
    "alias_declaration",
    "namespace_definition",
    "template_declaration",
];

/// Block-like containers (C++ grammar): the C set plus `declaration_list`.
const CONTAINER_KINDS: &[&str] = &[
    "compound_statement",
    "field_declaration_list",
    "enumerator_list",
    "declaration_list",
];

/// The C++ adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Cpp,
    comment_node_kinds: &["comment"],
    line_prefixes: &["//"],
    line_opener: r"^//[/!]*",
    block_delimiters: Some(BlockDelimiters {
        open: "/*",
        close: "*/",
    }),
    block_comments_nest: false,
    doc: None,
    directives: super::c::DIRECTIVES,
    positional_rules: &[],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: &["}", ")", "]"],
    name_fields: &["name", "declarator"],
    exported_rule: None,
    anchor_wrappers: &[],
    slot_rejects: super::c::SLOT_REJECTS,
    referenced_kinds: &[],
};
