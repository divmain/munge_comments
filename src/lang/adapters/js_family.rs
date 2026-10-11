//! Shared data for the JavaScript family (javascript, typescript, tsx).
//!
//! The three grammars agree on comment syntax (`comment` nodes, `//` line
//! prefixes, `/* … */` blocks, `^//[/!]*` openers) and on the node kinds the
//! family's rules name. The family's directive rows live here once:
//! [`JS_DIRECTIVES`] for javascript, and [`TS_DIRECTIVES`] (the same rows
//! plus TypeScript's `stripInternal` row) for typescript and tsx. Nothing in
//! this module is ever anything but data.

use crate::lang::adapter::{BlockDelimiters, DirectiveRow, Scope};
use crate::lang::adapters::rows::{family, lit, raw, raw_family, re};

use Scope::{File, Inline, Next};

/// Comment node kinds shared by the JavaScript family.
pub const COMMENT_NODE_KINDS: &[&str] = &["comment"];

/// Line-comment prefixes shared by the JavaScript family.
pub const LINE_PREFIXES: &[&str] = &["//"];

/// The line-opener regex source shared by the JavaScript family.
pub const LINE_OPENER: &str = r"^//[/!]*";

/// Block-comment delimiters shared by the JavaScript family.
pub const BLOCK_DELIMITERS: BlockDelimiters = BlockDelimiters {
    open: "/*",
    close: "*/",
};

/// Closing tokens shared by the JavaScript family.
pub const CLOSING_TOKENS: &[&str] = &["}", ")", "]"];

/// Node kinds named by JavaScript-family rule code: JSX expression
/// containers, export wrappers, the shebang node, and the CommonJS export
/// shapes the `JsModule` exported rule inspects.
pub const REFERENCED_KINDS: &[&str] = &[
    "jsx_expression",
    "export_statement",
    "hash_bang_line",
    "assignment_expression",
    "member_expression",
    "expression_statement",
    "object",
    "shorthand_property_identifier",
];

/// JSDoc tags TypeScript honors in `.js` files under `checkJs` /
/// `// @ts-check`: a JavaScript doc comment carrying any of them is
/// load-bearing (functional). `@prop {` and `@argument {` are TypeScript's
/// aliases of `@property {` and `@param {`.
pub const CHECKJS_DOC_TAGS: &[&str] = &[
    "@param {",
    "@param [",
    "@typedef",
    "@type",
    "@callback",
    "@returns {",
    "@return {",
    "@extends",
    "@template",
    "@enum",
    "@this",
    "@implements",
    "@augments",
    "@property {",
    "@prop {",
    "@arg {",
    "@argument {",
    "@satisfies",
    "@overload",
    "@import",
    "@private",
    "@protected",
    "@public",
    "@readonly",
    "@override",
    "@constructor",
    "@class",
    "@deprecated",
];

/// JSDoc tags that stay load-bearing in `.ts`/`.tsx`, where TypeScript
/// ignores JSDoc types: `@internal` (`stripInternal`), `@deprecated`
/// (deprecation diagnostics), and the TSDoc/API-Extractor release tags.
pub const TS_DOC_TAGS: &[&str] = &[
    "@internal",
    "@deprecated",
    "@alpha",
    "@beta",
    "@public",
    "@experimental",
];

/// The line window of a `c8`/`v8`/`node:coverage` `ignore next [N]`
/// directive, from the comment body: `N`, or 1 when absent (or zero, or
/// unparsable — the directive still covers the next line).
pub fn ignore_next_window(body: &str) -> usize {
    body.split_once("ignore next")
        .and_then(|(_, rest)| {
            let rest = rest.trim_start();
            let digits = rest
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..digits].parse::<usize>().ok()
        })
        .filter(|window| *window > 0)
        .unwrap_or(1)
}

/// The JavaScript family's directive rows (javascript; typescript and tsx
/// extend them with [`INTERNAL_ROW`]).
///
/// Rows whose meaning depends on the comment's opener (ESLint's block-only
/// `/* global */`, `/* exported */` and inline rule config, Flow comment
/// types, `/*!` legal comments, triple-slash directives) are raw-text
/// regexes. A next-scoped directive that is a prefix of a file-scoped
/// sibling (`deno-lint-ignore` / `deno-lint-ignore-file`) is a regex ending
/// at a non-word, non-`-` character, so the sibling keeps its own scope.
pub const JS_DIRECTIVES: &[DirectiveRow] = &[
    // TypeScript directives (honored in `.js` under checkJs as well).
    lit("@ts-ignore", "ts-directive", "directive:ts-ignore", Next),
    lit(
        "@ts-expect-error",
        "ts-directive",
        "directive:ts-expect-error",
        Next,
    ),
    lit("@ts-nocheck", "ts-directive", "directive:ts-nocheck", File),
    lit("@ts-check", "ts-directive", "directive:ts-check", File),
    raw_family(
        r"^///\s*<(?P<word>reference|amd-module|amd-dependency)\b",
        "ts-directive",
        "directive:",
        File,
    ),
    // ESLint directives; block-only forms are raw-text regexes.
    lit("eslint-disable", "linter", "directive:eslint-disable", File),
    lit(
        "eslint-disable-next-line",
        "linter",
        "directive:eslint-disable-next-line",
        Next,
    ),
    lit("eslint-enable", "linter", "directive:eslint-enable", File),
    lit("eslint-env", "linter", "directive:eslint-env", File),
    raw(
        r"^/\*\s*globals?(?:\s|\*/)",
        "linter",
        "directive:global",
        File,
    ),
    raw(
        r"^/\*\s*exported(?:\s|\*/)",
        "linter",
        "directive:exported",
        File,
    ),
    raw(
        r"^/\*\s*eslint\s",
        "linter",
        "directive:eslint-config",
        File,
    ),
    lit("tslint:disable", "linter", "directive:tslint-disable", File),
    lit(
        "tslint:disable-next-line",
        "linter",
        "directive:tslint-disable-next-line",
        Next,
    ),
    lit("tslint:enable", "linter", "directive:tslint-enable", File),
    lit("oxlint-disable", "linter", "directive:oxlint-disable", File),
    lit(
        "oxlint-disable-next-line",
        "linter",
        "directive:oxlint-disable-next-line",
        Next,
    ),
    lit("oxlint-enable", "linter", "directive:oxlint-enable", File),
    re(
        r"deno-lint-ignore(?:[^-\w]|$)",
        "linter",
        "directive:deno-lint-ignore",
        Next,
    ),
    lit(
        "deno-lint-ignore-file",
        "linter",
        "directive:deno-lint-ignore-file",
        File,
    ),
    re(
        r"biome-ignore(?:[^-\w]|$)",
        "linter",
        "directive:biome-ignore",
        Next,
    ),
    lit(
        "biome-ignore-all",
        "linter",
        "directive:biome-ignore-all",
        File,
    ),
    lit(
        "biome-ignore-start",
        "linter",
        "directive:biome-ignore-start",
        File,
    ),
    lit(
        "biome-ignore-end",
        "linter",
        "directive:biome-ignore-end",
        File,
    ),
    lit(
        "prettier-ignore",
        "formatter",
        "directive:prettier-ignore",
        Next,
    ),
    family(
        r"(?:^|\s)@(?P<word>format|prettier|noformat)\b",
        "formatter",
        "directive:",
        File,
    ),
    re(
        r"dprint-ignore(?:[^-\w]|$)",
        "formatter",
        "directive:dprint-ignore",
        Next,
    ),
    lit(
        "dprint-ignore-file",
        "formatter",
        "directive:dprint-ignore-file",
        File,
    ),
    lit(
        "istanbul ignore next",
        "coverage",
        "directive:istanbul-ignore-next",
        Next,
    ),
    lit(
        "istanbul ignore if",
        "coverage",
        "directive:istanbul-ignore-if",
        Next,
    ),
    lit(
        "istanbul ignore else",
        "coverage",
        "directive:istanbul-ignore-else",
        Next,
    ),
    lit(
        "istanbul ignore file",
        "coverage",
        "directive:istanbul-ignore-file",
        File,
    ),
    lit("c8 ignore", "coverage", "directive:c8-ignore", File),
    lit(
        "c8 ignore next",
        "coverage",
        "directive:c8-ignore-next",
        Next,
    )
    .with_line_window(ignore_next_window),
    lit("v8 ignore", "coverage", "directive:v8-ignore", File),
    lit(
        "v8 ignore next",
        "coverage",
        "directive:v8-ignore-next",
        Next,
    )
    .with_line_window(ignore_next_window),
    lit(
        "node:coverage ignore next",
        "coverage",
        "directive:node-coverage-ignore-next",
        Next,
    )
    .with_line_window(ignore_next_window),
    lit(
        "node:coverage disable",
        "coverage",
        "directive:node-coverage-disable",
        File,
    ),
    lit(
        "node:coverage enable",
        "coverage",
        "directive:node-coverage-enable",
        File,
    ),
    // Bundlers and minifiers. Webpack's own magic-comment rule:
    // `webpack[A-Z]…:` after a non-word character.
    family(
        r"(?:^|\W)webpack(?P<word>[A-Z][A-Za-z]*):",
        "bundler",
        "directive:webpack-",
        Inline,
    ),
    lit("@vite-ignore", "bundler", "directive:vite-ignore", Inline),
    lit("@__PURE__", "bundler", "pure-annotation", Next).when_trailing(Inline),
    lit("#__PURE__", "bundler", "pure-annotation", Next).when_trailing(Inline),
    lit(
        "@__NO_SIDE_EFFECTS__",
        "bundler",
        "directive:no-side-effects",
        Next,
    )
    .when_trailing(Inline),
    lit(
        "#__NO_SIDE_EFFECTS__",
        "bundler",
        "directive:no-side-effects",
        Next,
    )
    .when_trailing(Inline),
    // Legal comments minifiers preserve.
    lit("@license", "legal", "license", File),
    lit("@preserve", "legal", "directive:preserve", File),
    raw(r"^/\*!", "legal", "directive:legal-comment", File),
    lit("#region", "editor", "directive:region", File),
    lit("#endregion", "editor", "directive:endregion", File),
    re(
        r"(?:^|\s)@jsx(?:Frag|ImportSource|Runtime)?\b",
        "jsx-pragma",
        "jsx-pragma",
        File,
    ),
    re(r"(?:^|\s)@flow\b", "flow", "directive:flow", File),
    // Flow suppressions (the same next-line hazard as `@ts-expect-error`)
    // and Flow comment types (`/*: T */`, `/*:: … */`), which Flow reads as
    // type syntax.
    lit("$FlowFixMe", "flow", "directive:flow-fix-me", Next),
    lit(
        "$FlowExpectedError",
        "flow",
        "directive:flow-expected-error",
        Next,
    ),
    lit("$FlowIssue", "flow", "directive:flow-issue", Next),
    lit("$FlowIgnore", "flow", "directive:flow-ignore", Next),
    raw(r"^/\*:", "flow", "directive:flow-comment-type", Inline),
    lit(
        "@vitest-environment",
        "test-environment",
        "test-environment",
        File,
    ),
    lit(
        "@jest-environment",
        "test-environment",
        "test-environment",
        File,
    ),
];

/// TypeScript's `stripInternal` row (typescript/tsx only): `@internal` in
/// any comment — TypeScript's `isInternalDeclaration` checks every leading
/// comment range for the substring, not just JSDoc.
pub const INTERNAL_ROW: DirectiveRow = lit("@internal", "ts-directive", "directive:internal", Next);

/// The typescript/tsx directive rows: [`JS_DIRECTIVES`] plus
/// [`INTERNAL_ROW`].
pub const TS_DIRECTIVES: &[DirectiveRow] =
    &with_row::<{ JS_DIRECTIVES.len() + 1 }>(JS_DIRECTIVES, INTERNAL_ROW);

/// `rows` followed by `extra`, as an array of exactly `N = rows.len() + 1`
/// rows (checked at compile time).
const fn with_row<const N: usize>(rows: &[DirectiveRow], extra: DirectiveRow) -> [DirectiveRow; N] {
    assert!(N == rows.len() + 1, "with_row: N must be rows.len() + 1");
    let mut out = [extra; N];
    let mut index = 0;
    while index < rows.len() {
        out[index] = rows[index];
        index += 1;
    }
    out
}
