//! TSX adapter data.

use crate::lang::adapter::{
    AnchorWrapper, DocConfig, DocDetect, DocPolicy, LanguageAdapter, PositionalRuleId,
};
use crate::lang::adapters::js_family;
use crate::lang::ids::LanguageId;

/// Anchorable declarations (TSX grammar; the TypeScript set).
const DECLARATION_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_signature",
    "class_declaration",
    "abstract_class_declaration",
    "method_definition",
    "method_signature",
    "abstract_method_signature",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
    "module",
    "internal_module",
    "lexical_declaration",
    "variable_declaration",
    "variable_declarator",
    "public_field_definition",
    "property_signature",
];

/// Block-like containers (TSX grammar; the TypeScript set).
const CONTAINER_KINDS: &[&str] = &[
    "statement_block",
    "class_body",
    "interface_body",
    "object_type",
    "enum_body",
];

/// The TSX adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Tsx,
    comment_node_kinds: js_family::COMMENT_NODE_KINDS,
    line_prefixes: js_family::LINE_PREFIXES,
    line_opener: js_family::LINE_OPENER,
    block_delimiters: Some(js_family::BLOCK_DELIMITERS),
    block_comments_nest: false,
    doc: Some(DocConfig {
        detect: DocDetect::JsdocPrefix {
            triple_slash_lines: false,
        },
        policy: DocPolicy::Prose,
        functional_tags: js_family::TS_DOC_TAGS,
    }),
    directives: js_family::TS_DIRECTIVES,
    positional_rules: &[PositionalRuleId::SourceMapEof],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: js_family::CLOSING_TOKENS,
    name_fields: &["name"],
    exported_rule: None,
    anchor_wrappers: &[AnchorWrapper::Field {
        kind: "export_statement",
        field: "declaration",
    }],
    slot_rejects: &[],
    referenced_kinds: js_family::REFERENCED_KINDS,
};
