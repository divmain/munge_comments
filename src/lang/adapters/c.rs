//! C adapter data.

use crate::lang::adapter::{
    BlockDelimiters, DirectiveRow, LanguageAdapter, Scope, SlotKind, SlotReject,
};
use crate::lang::adapters::rows::{LONE_CR, family, lit, lit_family, raw, re};
use crate::lang::ids::LanguageId;

use Scope::{File, Inline, Next};

/// C and C++ directive rows (shared by the cpp adapter).
pub const DIRECTIVES: &[DirectiveRow] = &[
    // clang-tidy.
    lit("NOLINT", "linter", "directive:nolint", Inline),
    lit("NOLINTNEXTLINE", "linter", "directive:nolintnextline", Next),
    lit("NOLINTBEGIN", "linter", "directive:nolintbegin", File),
    lit("NOLINTEND", "linter", "directive:nolintend", File),
    lit(
        "clang-format off",
        "formatter",
        "directive:clang-format-off",
        File,
    ),
    lit(
        "clang-format on",
        "formatter",
        "directive:clang-format-on",
        File,
    ),
    // Fallthrough annotations: GCC's `-Wimplicit-fallthrough=3` comment regexes, verbatim from the GCC manual, each matched against the whole comment; cover compiler/linter spellings (`-fallthrough`, `@fallthrough@`, `lint -fallthrough`) and free-form FALLTHROUGH variants.
    re(
        r"^(?:-fallthrough)$",
        "compiler",
        "directive:fallthrough",
        Next,
    ),
    re(
        r"^(?:@fallthrough@)$",
        "compiler",
        "directive:fallthrough",
        Next,
    ),
    re(
        r"^(?:lint -fallthrough[ \t]*)$",
        "compiler",
        "directive:fallthrough",
        Next,
    ),
    re(
        r"^(?:[ \t.!]*(ELSE,? |INTENTIONAL(LY)? )?FALL(S | |-)?THR(OUGH|U)[ \t.!]*(-[^\n\r]*)?)$",
        "compiler",
        "directive:fallthrough",
        Next,
    ),
    re(
        r"^(?:[ \t.!]*(Else,? |Intentional(ly)? )?Fall((s | |-)[Tt]|t)hr(ough|u)[ \t.!]*(-[^\n\r]*)?)$",
        "compiler",
        "directive:fallthrough",
        Next,
    ),
    re(
        r"^(?:[ \t.!]*([Ee]lse,? |[Ii]ntentional(ly)? )?fall(s | |-)?thr(ough|u)[ \t.!]*(-[^\n\r]*)?)$",
        "compiler",
        "directive:fallthrough",
        Next,
    ),
    // include-what-you-use, cppcheck, PC-lint, Coverity.
    lit_family("IWYU pragma:", "linter", "directive:iwyu-pragma-", Inline),
    lit(
        "cppcheck-suppress",
        "linter",
        "directive:cppcheck-suppress",
        Next,
    )
    .when_trailing(Inline),
    raw(
        r"^(?://|/\*)lint(?:\s|\*/|$)",
        "linter",
        "directive:lint",
        File,
    ),
    lit("coverity[", "linter", "directive:coverity", Next),
    // lcov / gcovr coverage exclusions: `_LINE` markers exclude their line,
    // `_START`/`_STOP` delimit regions (`_BR_` variants: branches only).
    family(
        r"\b(?P<word>(?:LCOV|GCOVR)_EXCL_(?:BR_)?LINE)\b",
        "coverage",
        "directive:",
        Inline,
    ),
    family(
        r"\b(?P<word>(?:LCOV|GCOVR)_EXCL_(?:BR_)?(?:START|STOP))\b",
        "coverage",
        "directive:",
        File,
    ),
    // A `//` comment ending a line in a backslash plus horizontal
    // whitespace, or a `??/` trigraph: GCC/Clang splice the next line into
    // the comment, but tree-sitter-c ends the comment there. (A bare `\`
    // before the newline is excluded: tree-sitter-c already continues that
    // comment.) `\r?` covers CRLF line ends.
    raw(
        r"(?s)\A//.*(?:\\[ \t\f\v]+|\?\?/[ \t\f\v]*)\r?(?m:$)",
        "byte-hazard",
        "line-splice",
        Inline,
    ),
    // A lone CR: tree-sitter-c's comment token swallows it, the compiler
    // ends the line there.
    LONE_CR,
];

/// Anchorable declarations (C grammar).
pub const DECLARATION_KINDS: &[&str] = &[
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
];

/// Block-like containers (C grammar).
pub const CONTAINER_KINDS: &[&str] = &[
    "compound_statement",
    "field_declaration_list",
    "enumerator_list",
];

/// Slot rejects shared by C and C++:
///
/// - a line comment ending in a backslash (or trigraph `??/`, with optional
///   horizontal whitespace) would splice the next source line into the
///   comment once written back;
/// - `/*` inside a block or doc comment triggers `-Wcomment` (part of
///   `-Wall`, fatal under `-Werror`).
pub const SLOT_REJECTS: &[SlotReject] = &[
    SlotReject {
        slots: &[SlotKind::Line],
        pattern: r"(?m)(\\|\?\?/)[ \t\f\v]*$",
        reason: "a line-splice (backslash or trigraph before the newline) would swallow the next source line",
    },
    SlotReject {
        slots: &[SlotKind::Block, SlotKind::Doc],
        pattern: r"/\*",
        reason: "`/*` inside a block comment triggers -Wcomment, fatal under -Werror",
    },
];

/// The C adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::C,
    comment_node_kinds: &["comment"],
    line_prefixes: &["//"],
    line_opener: r"^//[/!]*",
    block_delimiters: Some(BlockDelimiters {
        open: "/*",
        close: "*/",
    }),
    block_comments_nest: false,
    doc: None,
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
