//! Cross-language directive rows.
//!
//! Rows that apply to every language — modelines, `SPDX-License-Identifier:`,
//! and the cross-language scanner/linter suppressions — live once here and
//! are matched together with each adapter's own `directives` everywhere
//! directive rows are consulted.
//!
//! The modeline and SPDX rows come first; the cross-language
//! scanner/linter suppression rows follow them.

use crate::lang::adapter::{DirectiveRow, Pattern, Placement, Scope, TagSpec};
use crate::lang::adapters::rows::{lit, re};

/// Directive rows that apply to every language, matched alongside every
/// adapter's own rows.
pub static GLOBAL_DIRECTIVES: &[DirectiveRow] = &[
    // Vim modelines, by vim's own rule: `vi:`, `vim:`/`Vim:` (optionally
    // version-qualified, e.g. `vim700:`, `vim>=703:`) or `ex:`, at the start
    // of the text or after whitespace — so `regex:` and `index:` never
    // match. Vim reads them only in the first and last 5 lines.
    DirectiveRow {
        pattern: Pattern::Re(r"(^|\s)(vi|[Vv]im([<=>]?[0-9]+)?|ex):"),
        category: "modeline",
        tag: TagSpec::Fixed("modeline"),
        scope: Scope::File,
        scope_when_trailing: None,
        line_window: None,
        placement: Placement::FileEdges(5),
    },
    // Emacs `-*- … -*-` file variables, honored only on the first line (or
    // the second, after a shebang).
    DirectiveRow {
        pattern: Pattern::Re(r"-\*-.*-\*-"),
        category: "modeline",
        tag: TagSpec::Fixed("modeline"),
        scope: Scope::File,
        scope_when_trailing: None,
        line_window: None,
        placement: Placement::FirstLineOrAfterShebang,
    },
    // REUSE-spec license identifiers, wherever they appear.
    DirectiveRow {
        pattern: Pattern::Lit("SPDX-License-Identifier:"),
        category: "license",
        tag: TagSpec::Fixed("license"),
        scope: Scope::File,
        scope_when_trailing: None,
        line_window: None,
        placement: Placement::Anywhere,
    },
    // Cross-language scanner/linter suppressions.
    // SonarQube.
    lit("NOSONAR", "scanner", "directive:nosonar", Scope::Inline),
    // Semgrep.
    lit("nosemgrep", "scanner", "directive:nosemgrep", Scope::Next).when_trailing(Scope::Inline),
    // LGTM / CodeQL alert suppressions.
    lit("lgtm[", "scanner", "directive:lgtm", Scope::Next).when_trailing(Scope::Inline),
    lit("codeql[", "scanner", "directive:codeql", Scope::Next).when_trailing(Scope::Inline),
    // gosec `#nosec` / bandit `# nosec`: at the start of the comment body
    // (the bandit form, whose `#` is the opener) or as `#nosec` after it
    // (the gosec form), followed by a rule list, a `:` or the end — so prose
    // that merely mentions nosec is not a directive.
    re(
        r"(?:^|#)\s*nosec(?:[\s:]|$)",
        "scanner",
        "directive:nosec",
        Scope::Next,
    )
    .when_trailing(Scope::Inline),
    // gitleaks and detect-secrets.
    lit(
        "gitleaks:allow",
        "scanner",
        "directive:gitleaks-allow",
        Scope::Inline,
    ),
    lit(
        "pragma: allowlist secret",
        "scanner",
        "directive:pragma-allowlist-secret",
        Scope::Inline,
    ),
    lit(
        "pragma: allowlist nextline secret",
        "scanner",
        "directive:pragma-allowlist-nextline-secret",
        Scope::Next,
    ),
];
