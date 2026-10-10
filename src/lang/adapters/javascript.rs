//! JavaScript adapter data.

use crate::lang::adapter::{
    AnchorWrapper, DocConfig, DocDetect, DocPolicy, ExportedRule, LanguageAdapter, PositionalRuleId,
};
use crate::lang::adapters::js_family;
use crate::lang::ids::LanguageId;

/// Anchorable declarations (JavaScript grammar).
const DECLARATION_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "class_declaration",
    "method_definition",
    "field_definition",
    "lexical_declaration",
    "variable_declaration",
    "variable_declarator",
];

/// Block-like containers (JavaScript grammar).
const CONTAINER_KINDS: &[&str] = &["statement_block", "class_body"];

/// The JavaScript adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Javascript,
    comment_node_kinds: js_family::COMMENT_NODE_KINDS,
    line_prefixes: js_family::LINE_PREFIXES,
    line_opener: js_family::LINE_OPENER,
    block_delimiters: Some(js_family::BLOCK_DELIMITERS),
    block_comments_nest: false,
    doc: Some(DocConfig {
        detect: DocDetect::JsdocPrefix {
            triple_slash_lines: false,
        },
        policy: DocPolicy::ExportProtected,
        functional_tags: js_family::CHECKJS_DOC_TAGS,
    }),
    directives: js_family::JS_DIRECTIVES,
    positional_rules: &[PositionalRuleId::SourceMapEof],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: js_family::CLOSING_TOKENS,
    name_fields: &["name"],
    exported_rule: Some(ExportedRule::JsModule),
    anchor_wrappers: &[AnchorWrapper::Field {
        kind: "export_statement",
        field: "declaration",
    }],
    slot_rejects: &[],
    referenced_kinds: js_family::REFERENCED_KINDS,
};
