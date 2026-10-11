//! Java adapter data.

use crate::lang::adapter::{
    BlockDelimiters, DirectiveRow, DocConfig, DocDetect, DocPolicy, LanguageAdapter, Scope,
    SlotKind, SlotReject,
};
use crate::lang::adapters::rows::{LONE_CR, lit, raw};
use crate::lang::ids::LanguageId;

use Scope::{File, Inline, Next};

/// Java directive rows.
const DIRECTIVES: &[DirectiveRow] = &[
    // Linters and formatters: PMD, Checkstyle, Eclipse/IntelliJ formatter tags, Spotless, IntelliJ inspections. File-scoped toggles bracket a region; inline markers apply to the current line.
    lit("NOPMD", "linter", "directive:nopmd", Inline),
    lit("CHECKSTYLE:OFF", "linter", "directive:checkstyle-off", File),
    lit("CHECKSTYLE:ON", "linter", "directive:checkstyle-on", File),
    lit(
        "@formatter:off",
        "formatter",
        "directive:formatter-off",
        File,
    ),
    lit("@formatter:on", "formatter", "directive:formatter-on", File),
    lit("spotless:off", "formatter", "directive:spotless-off", File),
    lit("spotless:on", "formatter", "directive:spotless-on", File),
    lit("//noinspection", "linter", "directive:noinspection", Next).when_trailing(Inline),
    // Eclipse compiler markers: $NON-NLS- suppresses externalized-string warnings, $FALL-THROUGH$ marks intentional switch fallthrough.
    lit("$NON-NLS-", "compiler", "directive:non-nls", Inline),
    lit("$FALL-THROUGH$", "compiler", "directive:fall-through", Next),
    // javac decodes unicode escapes before lexing, so such a comment may be
    // code as far as javac is concerned.
    raw(
        r"\\u+[0-9a-fA-F]{4}",
        "byte-hazard",
        "unicode-escape",
        Inline,
    ),
    // tree-sitter-java's `//[^\n]*` swallows a lone CR; javac ends the line there, so flag it as a byte hazard.
    LONE_CR,
];

/// Anchorable declarations (Java grammar).
const DECLARATION_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "annotation_type_declaration",
    "method_declaration",
    "constructor_declaration",
    "field_declaration",
    "variable_declarator",
    "enum_constant",
];

/// Block-like containers (Java grammar).
const CONTAINER_KINDS: &[&str] = &[
    "block",
    "class_body",
    "interface_body",
    "enum_body",
    "constructor_body",
];

/// javac decodes `\uXXXX` (with repeatable `u`s) before lexing, so an escape
/// sequence in comment text could end the comment and inject code — written
/// text carrying one is refused in every slot kind.
const SLOT_REJECTS: &[SlotReject] = &[SlotReject {
    slots: &[SlotKind::Line, SlotKind::Block, SlotKind::Doc],
    pattern: r"\\u+[0-9a-fA-F]{4}",
    reason: "javac decodes unicode escapes before lexing; an escape in comment text could end the comment and inject code",
}];

/// The Java adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Java,
    comment_node_kinds: &["line_comment", "block_comment"],
    line_prefixes: &["//"],
    line_opener: r"^//[/!]*",
    block_delimiters: Some(BlockDelimiters {
        open: "/*",
        close: "*/",
    }),
    block_comments_nest: false,
    doc: Some(DocConfig {
        detect: DocDetect::JsdocPrefix {
            triple_slash_lines: true,
        },
        policy: DocPolicy::AlwaysProtected,
        // Functional tags alter API semantics rather than merely documenting it; javac honors the `@deprecated` Javadoc tag (deprecation flag, `-Xlint:dep-ann`), so such Javadoc is load-bearing.
        functional_tags: &["@deprecated"],
    }),
    directives: DIRECTIVES,
    positional_rules: &[],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: &["}", ")", "]"],
    name_fields: &["name", "declarator"],
    exported_rule: None,
    anchor_wrappers: &[],
    slot_rejects: SLOT_REJECTS,
    referenced_kinds: &[],
};
