//! Python adapter data.

use crate::lang::adapter::{AnchorWrapper, DirectiveRow, LanguageAdapter, PositionalRuleId, Scope};
use crate::lang::adapters::rows::{LONE_CR, family, re};
use crate::lang::ids::LanguageId;

use Scope::{File, Inline, Next};

/// Anchorable declarations (Python grammar).
const DECLARATION_KINDS: &[&str] = &["function_definition", "class_definition", "assignment"];

/// Block-like containers (Python grammar).
const CONTAINER_KINDS: &[&str] = &["block"];

/// Python directive rows. Tool comments are matched at the start of the
/// comment body or after a further `#` (`# noqa  # type: ignore` chains),
/// as the tools themselves read them.
const DIRECTIVES: &[DirectiveRow] = &[
    // Type checkers: `# type: ignore[…]` and `# type: <annotation>` (honored
    // by mypy and pyright), file/inline configuration comments.
    family(
        r"(?:^|#)\s*type:\s*(?P<word>ignore\b)?",
        "type-checker",
        "directive:type-",
        Inline,
    ),
    family(
        r"(?:^|#)\s*mypy:\s*(?P<word>[A-Za-z][\w-]*)?",
        "type-checker",
        "directive:mypy-",
        File,
    ),
    family(
        r"(?:^|#)\s*pyright:\s*(?P<word>[A-Za-z][\w-]*)?",
        "type-checker",
        "directive:pyright-",
        File,
    )
    .when_trailing(Inline),
    family(
        r"(?:^|#)\s*pyre-(?P<word>ignore|fixme)(?:[^-\w]|$)",
        "type-checker",
        "directive:pyre-",
        Next,
    )
    .when_trailing(Inline),
    family(
        r"(?:^|#)\s*pyre-(?P<word>strict|unsafe|ignore-all-errors)\b",
        "type-checker",
        "directive:pyre-",
        File,
    ),
    family(
        r"(?:^|#)\s*pytype:\s*(?P<word>disable|enable)\s*=",
        "type-checker",
        "directive:pytype-",
        File,
    )
    .when_trailing(Inline),
    // Linters: flake8/ruff `noqa` (case-insensitive, as both tools read it),
    // pylint.
    re(r"(?i)(?:^|#)\s*noqa\b", "linter", "directive:noqa", Inline),
    re(
        r"(?i)(?:^|#)\s*flake8[:=]\s*noqa\b",
        "linter",
        "directive:flake8-noqa",
        File,
    ),
    re(
        r"(?i)(?:^|#)\s*ruff\s*:\s*noqa\b",
        "linter",
        "directive:ruff-noqa",
        File,
    ),
    family(
        r"(?:^|#)\s*pylint:\s*(?P<word>[A-Za-z][\w-]*)",
        "linter",
        "directive:pylint-",
        File,
    )
    .when_trailing(Inline),
    re(
        r"(?:^|#)\s*pylint:\s*disable-next\s*=",
        "linter",
        "directive:pylint-disable-next",
        Next,
    ),
    // Formatters and import sorters.
    family(
        r"(?:^|#)\s*fmt:\s*(?P<word>off|on)\b",
        "formatter",
        "directive:fmt-",
        File,
    ),
    family(
        r"(?:^|#)\s*fmt:\s*(?P<word>skip)\b",
        "formatter",
        "directive:fmt-",
        Inline,
    ),
    family(
        r"(?:^|#)\s*yapf:\s*(?P<word>disable|enable)\b",
        "formatter",
        "directive:yapf-",
        File,
    ),
    family(
        r"(?:^|#)\s*isort:\s*(?P<word>[A-Za-z][\w-]*)",
        "formatter",
        "directive:isort-",
        File,
    )
    .when_trailing(Inline),
    // coverage.py's default exclusion and partial-branch regexes.
    family(
        r"(?:^|#)\s*(?:pragma|PRAGMA)[:\s]?\s*(?:no|NO)\s*(?P<word>cover|COVER|branch|BRANCH)",
        "coverage",
        "directive:pragma-no-",
        Inline,
    ),
    family(
        r"^\s*cython:\s*(?P<word>[A-Za-z]\w*)",
        "compiler",
        "directive:cython-",
        File,
    ),
    // tree-sitter-python's comment token swallows a lone CR; CPython ends
    // the line there and executes what follows.
    LONE_CR,
];

/// The Python adapter.
pub const ADAPTER: LanguageAdapter = LanguageAdapter {
    id: LanguageId::Python,
    comment_node_kinds: &["comment"],
    line_prefixes: &["#"],
    line_opener: r"^#:?",
    block_delimiters: None,
    block_comments_nest: false,
    doc: None,
    directives: DIRECTIVES,
    positional_rules: &[PositionalRuleId::PepEncodingCookie],
    declaration_kinds: DECLARATION_KINDS,
    container_kinds: CONTAINER_KINDS,
    closing_tokens: &[],
    name_fields: &["name"],
    exported_rule: None,
    anchor_wrappers: &[AnchorWrapper::Field {
        kind: "decorated_definition",
        field: "definition",
    }],
    slot_rejects: &[],
    referenced_kinds: &[],
};
