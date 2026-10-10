//! Compact constructors for directive-table rows, shared by the adapter
//! data modules. Every row they build is honored anywhere in the file
//! (`Placement::Anywhere`); trailing scopes and line windows are added with
//! [`DirectiveRow::when_trailing`] / [`DirectiveRow::with_line_window`].

use crate::lang::adapter::{DirectiveRow, Pattern, Scope, TagSpec};

/// A literal row (case-sensitive substring of the raw text) with a fixed
/// tag.
pub const fn lit(
    pattern: &'static str,
    category: &'static str,
    tag: &'static str,
    scope: Scope,
) -> DirectiveRow {
    DirectiveRow::new(Pattern::Lit(pattern), category, TagSpec::Fixed(tag), scope)
}

/// A body-regex row with a fixed tag.
pub const fn re(
    pattern: &'static str,
    category: &'static str,
    tag: &'static str,
    scope: Scope,
) -> DirectiveRow {
    DirectiveRow::new(Pattern::Re(pattern), category, TagSpec::Fixed(tag), scope)
}

/// A body-regex row whose tag is `prefix` plus the `word` capture.
pub const fn family(
    pattern: &'static str,
    category: &'static str,
    prefix: &'static str,
    scope: Scope,
) -> DirectiveRow {
    DirectiveRow::new(
        Pattern::Re(pattern),
        category,
        TagSpec::Family { prefix },
        scope,
    )
}

/// A literal row whose tag is `prefix` plus the word following the literal.
pub const fn lit_family(
    pattern: &'static str,
    category: &'static str,
    prefix: &'static str,
    scope: Scope,
) -> DirectiveRow {
    DirectiveRow::new(
        Pattern::Lit(pattern),
        category,
        TagSpec::Family { prefix },
        scope,
    )
}

/// A raw-text-regex row with a fixed tag.
pub const fn raw(
    pattern: &'static str,
    category: &'static str,
    tag: &'static str,
    scope: Scope,
) -> DirectiveRow {
    DirectiveRow::new(
        Pattern::RawRe(pattern),
        category,
        TagSpec::Fixed(tag),
        scope,
    )
}

/// A raw-text-regex row whose tag is `prefix` plus the `word` capture.
pub const fn raw_family(
    pattern: &'static str,
    category: &'static str,
    prefix: &'static str,
    scope: Scope,
) -> DirectiveRow {
    DirectiveRow::new(
        Pattern::RawRe(pattern),
        category,
        TagSpec::Family { prefix },
        scope,
    )
}

/// A comment containing a lone `\r` (a CR not followed by LF): the grammar's
/// comment token swallows it, but the compiler/interpreter treats it as a
/// line terminator, so the "comment" may hide live code (python, java,
/// c/cpp). Bare tag `lone-cr`.
pub const LONE_CR: DirectiveRow = raw(r"\r(?:[^\n]|$)", "byte-hazard", "lone-cr", Scope::Inline);
