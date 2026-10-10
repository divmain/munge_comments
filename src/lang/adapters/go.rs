//! Go adapter data.

use crate::lang::adapter::{
    BlockDelimiters, DirectiveRow, LanguageAdapter, PositionalRuleId, Scope,
};
use crate::lang::adapters::rows::{lit, lit_family, re};
use crate::lang::ids::LanguageId;

use Scope::{File, Inline, Next};

/// Go directive rows.
const DIRECTIVES: &[DirectiveRow] = &[
    // Toolchain directives: every `//go:` compiler directive (`build`,
    // `generate`, `embed`, `noinline`, `linkname`, …), the legacy
    // `// +build` constraint, `//line` / `/*line` position directives, and
    // cgo's `//export`.
    lit_family("//go:", "compiler", "directive:go-", File),
    lit("// +build", "compiler", "directive:build", File),
    lit("//line ", "compiler", "directive:line", File),
    lit("/*line ", "compiler", "directive:line", File),
    lit("//export ", "cgo", "directive:export", Next),
    // Linters: golangci-lint, GoLand, staticcheck.
    lit("//nolint", "linter", "directive:nolint", Next).when_trailing(Inline),
    lit("//noinspection", "linter", "directive:noinspection", Next).when_trailing(Inline),
    lit("//lint:ignore", "linter", "directive:lint-ignore", Next).when_trailing(Inline),
    lit(
        "//lint:file-ignore",
        "linter",
        "directive:lint-file-ignore",
        File,
    ),
    // A `Deprecated:` paragraph (staticcheck SA1019 and gopls key off it).
    re(
        r"(?m)^Deprecated: ",
        "deprecation",
        "directive:deprecated",
        Next,
    ),
    // The generated-file marker, which must be the whole comment.
    re(
        r"^Code generated .* DO NOT EDIT\.$",
        "generated",
        "directive:go-generated",
        File,
    ),
];

/// Anchorable declarations (Go grammar).
const DECLARATION_KINDS: &[&str] = &[
    "function_declaration",
    "method_declaration",
    "type_declaration",
    "type_spec",
    "type_alias",
    "const_declaration",
    "const_spec",
    "var_declaration",
    "var_spec",
    "field_declaration",
    "method_elem",
];

/// Block-like containers (Go grammar).
const CONTAINER_KINDS: &[&str] = &["block", "statement_list", "field_declaration_list"];

/// Node kinds named by Go rule code: both `import "C"` shapes for the cgo
/// preamble rule (grouped and single-spec imports), plus the string literal
/// the preamble detection compares against.
const REFERENCED_KINDS: &[&str] = &[
    "import_declaration",
    "import_spec",
    "import_spec_list",
    "interpreted_string_literal",
];

/// The Go adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Go,
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
    positional_rules: &[
        PositionalRuleId::CgoPreamble,
        PositionalRuleId::GoExampleOutput,
    ],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: &["}", ")", "]"],
    name_fields: &["name"],
    exported_rule: None,
    anchor_wrappers: &[],
    slot_rejects: &[],
    referenced_kinds: REFERENCED_KINDS,
};
