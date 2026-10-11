//! Adapter types: the declarative per-language vocabulary the uniform
//! engine (scanner, classifier, chunker, aligner, verifier) is driven by,
//! plus the compiled-pattern cache.
//!
//! Every regex-bearing field of an adapter (`Pattern::Re`/`Pattern::RawRe`
//! rows, `line_opener`, `slot_rejects`) is compiled once per process per
//! adapter into a [`OnceLock`] slot; a compile failure surfaces as
//! [`Error::Invariant`] and a unit test compiles every adapter's patterns,
//! so it can never ship.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use crate::errors::Error;
use crate::lang::adapters::global::GLOBAL_DIRECTIVES;
use crate::lang::ids::LanguageId;

/// Block-comment delimiters. `None` (on the adapter) for languages without
/// block comments (Python).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockDelimiters {
    /// The opening delimiter (e.g. `/*`).
    pub open: &'static str,
    /// The closing delimiter (e.g. `*/`).
    pub close: &'static str,
}

/// How a language detects doc comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocDetect {
    /// Block text starts `/**` with a non-`/` fourth char (`/**/` is not
    /// doc); with `triple_slash_lines`, every line comment starting `///`
    /// is also doc (Java's JEP 467 Markdown doc comments — javac's
    /// tokenizer makes any `//` followed by a third `/` a doc line, so
    /// `////…` lines are doc too).
    JsdocPrefix {
        /// Whether `///` line comments also count as doc.
        triple_slash_lines: bool,
    },
    /// Node structure: an `outer_doc_comment_marker` or
    /// `inner_doc_comment_marker` child (Rust; never a text prefix, since
    /// `////` is not a doc comment in Rust).
    RustMarkers,
}

/// What the engine does with a detected doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocPolicy {
    /// All doc comments are evaluated as prose (TypeScript/TSX).
    Prose,
    /// Doc comments on exported symbols are `doc-protected`; internals are
    /// prose (JavaScript).
    ExportProtected,
    /// Every doc comment is `doc-protected` (Java, Rust).
    AlwaysProtected,
}

/// Per-language doc-comment detection and policy, plus the tags that make a
/// doc comment functional wherever they appear in its text.
#[derive(Debug)]
pub struct DocConfig {
    /// Detection rule.
    pub detect: DocDetect,
    /// Engine policy for detected doc comments.
    pub policy: DocPolicy,
    /// Doc text containing any of these tags is functional (data, not
    /// classifier code).
    pub functional_tags: &'static [&'static str],
}

/// The slot kinds a [`SlotReject`] pattern applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// A `//`-style (or `#`-style) comment.
    Line,
    /// A `/* … */`-style comment.
    Block,
    /// A doc comment in a block (or doc line) shape.
    Doc,
}

/// Text that must never be *written* into a comment slot of the given
/// kinds, with the reason it is rejected (surfaced in diagnostics).
#[derive(Debug)]
pub struct SlotReject {
    /// The slot kinds this pattern guards.
    pub slots: &'static [SlotKind],
    /// Regex source matched against the (normalized) candidate text.
    pub pattern: &'static str,
    /// Why written text matching this pattern is refused.
    pub reason: &'static str,
}

/// A functional-comment pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Case-sensitive literal, matched as a substring of the comment's raw
    /// text.
    Lit(&'static str),
    /// Regex on `comment_body` (the delimiter-stripped body), unanchored
    /// search; whole-body matches anchor themselves with `^…$`.
    Re(&'static str),
    /// Regex on the RAW comment text (delimiters and every byte included),
    /// for byte-level hazards that body stripping would hide (`lone-cr`,
    /// `line-splice`, `unicode-escape`) and for directives whose meaning
    /// depends on the comment's opener, which body stripping also hides
    /// (ESLint's block-only `/* global */`, Flow's `/*:` comment types,
    /// compiletest's `//@`).
    RawRe(&'static str),
}

/// How a directive row contributes its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSpec {
    /// A fixed tag string (e.g. `"directive:ts-expect-error"`).
    Fixed(&'static str),
    /// `prefix` plus the matched sub-directive word (e.g. prefix
    /// `"directive:go-"` + `go:build` → `directive:go-build`).
    Family {
        /// Tag prefix; the matched word is appended.
        prefix: &'static str,
    },
}

/// What a matching directive applies to (serialized lowercase in
/// inventory snapshots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The next line/node after the comment.
    Next,
    /// The whole file.
    File,
    /// The code that shares the comment's line.
    Inline,
}

/// Where in the file a directive row's match must lie for the row to count.
///
/// Most directives are honored anywhere; editor modelines are honored only
/// near the file's edges, so their rows carry the window as data and the
/// classifier evaluates it on the physical line each match sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Anywhere in the file.
    Anywhere,
    /// Within the first or the last `n` lines of the file (vim modelines:
    /// the default `'modelines'` value is 5).
    FileEdges(usize),
    /// On line 1, or on line 2 when line 1 is a shebang (emacs `-*-` file
    /// variables).
    FirstLineOrAfterShebang,
}

/// Positional rules: functional-by-position classification that no pattern
/// table can express. Which rules fire is per-adapter data
/// (`positional_rules`); their implementations live in the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionalRuleId {
    /// A PEP 263 encoding cookie on line 1/2 (Python).
    PepEncodingCookie,
    /// The doc-comment block preceding `import "C"` (both import shapes) is
    /// compiled C code (Go).
    CgoPreamble,
    /// `// Output:` and everything after it inside an `Example…` test
    /// function (Go).
    GoExampleOutput,
    /// A trailing `sourceMappingURL=` comment at end of file
    /// (JavaScript/TypeScript/TSX).
    SourceMapEof,
}

/// One functional-comment pattern row.
#[derive(Debug, Clone, Copy)]
pub struct DirectiveRow {
    /// The pattern to match.
    pub pattern: Pattern,
    /// Category label (e.g. `"ts-directive"`, `"linter"`, `"coverage"`).
    pub category: &'static str,
    /// The tag this row contributes on match. A `Family` tag on a `Re` or
    /// `RawRe` row takes its sub-directive word from the pattern's named
    /// capture group `word` (validated at compile time); on a `Lit` row,
    /// from the word directly following the literal.
    pub tag: TagSpec,
    /// What the directive applies to.
    pub scope: Scope,
    /// A different scope when code precedes the comment on its line (e.g.
    /// `//nolint`: `Next` on its own line, `Inline` when trailing).
    pub scope_when_trailing: Option<Scope>,
    /// For line-window directives (`c8 ignore next N`): extracts N from the
    /// comment's body (`comment_body`).
    pub line_window: Option<fn(&str) -> usize>,
    /// Where in the file a match must lie for the row to count
    /// (`Anywhere` for every row except the modelines).
    pub placement: Placement,
}

impl DirectiveRow {
    /// A row honored anywhere in the file, with no trailing scope and no
    /// line window — the shape of nearly every table row.
    pub const fn new(pattern: Pattern, category: &'static str, tag: TagSpec, scope: Scope) -> Self {
        Self {
            pattern,
            category,
            tag,
            scope,
            scope_when_trailing: None,
            line_window: None,
            placement: Placement::Anywhere,
        }
    }

    /// The same row with a different scope when code precedes the comment
    /// on its line.
    pub const fn when_trailing(self, scope: Scope) -> Self {
        Self {
            scope_when_trailing: Some(scope),
            ..self
        }
    }

    /// The same row with a line-window extractor.
    pub const fn with_line_window(self, window: fn(&str) -> usize) -> Self {
        Self {
            line_window: Some(window),
            ..self
        }
    }
}

/// How an `ExportProtected` doc policy decides exported-ness. Variants are
/// added only when a policy needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportedRule {
    /// JavaScript: the anchor declaration is exported when it has an
    /// `export_statement` ancestor, or (in `.js`/`.cjs`) is a CommonJS
    /// export of the same file, or is itself the `module.exports` /
    /// `exports.<name>` assignment.
    JsModule,
}

/// Grammar shapes that wrap an anchor declaration, with how to unwrap them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorWrapper {
    /// If the chosen node is `kind`, the anchor becomes its child under
    /// `field` (e.g. `export_statement` → `declaration`, Python's
    /// `decorated_definition` → `definition`).
    Field {
        /// The wrapper node kind.
        kind: &'static str,
        /// The field holding the wrapped declaration.
        field: &'static str,
    },
    /// If the chosen node is `kind`, the anchor becomes the next named
    /// sibling that is neither a `PrefixSibling` kind nor a comment (Rust's
    /// `attribute_item` run).
    PrefixSibling {
        /// The wrapper node kind.
        kind: &'static str,
    },
}

/// One language's entire declarative configuration. Data only: no parser
/// imports, no engine logic.
#[derive(Debug)]
pub struct LanguageAdapter {
    /// The language this adapter configures.
    pub id: LanguageId,
    /// Node types that are comments, per grammar (`comment` for most;
    /// `line_comment` + `block_comment` for Rust and Java).
    pub comment_node_kinds: &'static [&'static str],
    /// A comment whose text starts with one of these is kind `line`
    /// (`["//"]`, or `["#"]` for Python).
    pub line_prefixes: &'static [&'static str],
    /// Regex source; the opener `slot_for` captures verbatim for line slots
    /// (c-family `^//[/!]*`, Python `^#:?`).
    pub line_opener: &'static str,
    /// Block-comment delimiters, when the language has block comments.
    pub block_delimiters: Option<BlockDelimiters>,
    /// Whether block comments nest (Rust: true).
    pub block_comments_nest: bool,
    /// Doc-comment detection + policy, when the language has doc comments.
    pub doc: Option<DocConfig>,
    /// The language's functional-comment pattern rows (filled by the
    /// directive-tables ticket).
    pub directives: &'static [DirectiveRow],
    /// Which positional rules fire for this language (filled by the
    /// classifier ticket).
    pub positional_rules: &'static [PositionalRuleId],
    /// Grammar node types that are anchorable declarations; also the
    /// prompt's `nodeKind` vocabulary.
    pub declaration_kinds: &'static [&'static str],
    /// Block-like node types never used as anchors.
    pub container_kinds: &'static [&'static str],
    /// Anonymous token types that make a preceding comment dangling
    /// (`["}", ")", "]"]`; Python has none.
    pub closing_tokens: &'static [&'static str],
    /// Field names tried for anchor naming.
    pub name_fields: &'static [&'static str],
    /// How exported-ness is decided, where policy is `ExportProtected`
    /// (JavaScript only in v1).
    pub exported_rule: Option<ExportedRule>,
    /// Grammar shapes that wrap an anchor declaration, and how to unwrap
    /// them.
    pub anchor_wrappers: &'static [AnchorWrapper],
    /// Text that must never be written into a slot of the given kinds.
    pub slot_rejects: &'static [SlotReject],
    /// Every other node kind that rule code names for this language,
    /// validated at grammar initialization so an omission fails loudly.
    pub referenced_kinds: &'static [&'static str],
}

/// One compiled pattern, in the form its consumer matches it in.
#[derive(Debug)]
pub enum CompiledPattern {
    /// Substring match against the raw comment text.
    Literal {
        /// The needle.
        needle: &'static str,
    },
    /// Unanchored regex against `comment_body`.
    Body {
        /// The compiled regex.
        regex: Regex,
    },
    /// Unanchored regex against the raw comment text.
    Raw {
        /// The compiled regex.
        regex: Regex,
    },
}

/// A directive row with its pattern compiled.
#[derive(Debug)]
pub struct CompiledRow {
    /// The originating row.
    pub row: &'static DirectiveRow,
    /// The row's pattern, in matchable form.
    pub pattern: CompiledPattern,
}

/// A slot reject with its pattern compiled for one slot kind.
#[derive(Debug)]
pub struct CompiledSlotReject {
    /// The slot kind this reject guards.
    pub kind: SlotKind,
    /// The compiled reject pattern.
    pub regex: Regex,
    /// Why text matching it is refused.
    pub reason: &'static str,
}

/// One doc `functional_tags` entry in matchable form.
///
/// A tag matches doc text when it occurs with any run of whitespace
/// (including none) where the tag spells a single space — TypeScript reads
/// `@param{T}` like `@param {T}` — and, for a tag ending in a word
/// character, not as the prefix of a longer tag (`@class` does not match
/// `@classdesc`, `@private` does not match `@privateRemarks`).
#[derive(Debug)]
pub struct CompiledDocTag {
    /// The tag as written in the adapter data (e.g. `"@param {"`).
    pub source: &'static str,
    /// The compiled matcher.
    pub regex: Regex,
}

/// The compiled form of one adapter's regex-bearing data: its directive
/// rows together with [`GLOBAL_DIRECTIVES`], the `line_opener`, the doc
/// `functional_tags`, and the `slot_rejects` per slot kind.
#[derive(Debug)]
pub struct CompiledPatterns {
    /// The adapter's directive rows followed by the global rows.
    pub rows: Vec<CompiledRow>,
    /// The adapter's `line_opener`.
    pub line_opener: Regex,
    /// The adapter's doc `functional_tags` (empty without a doc config).
    pub doc_functional_tags: Vec<CompiledDocTag>,
    /// The adapter's `slot_rejects`, expanded per slot kind.
    pub slot_rejects: Vec<CompiledSlotReject>,
}

/// Per-adapter compiled-pattern slots, indexed by
/// [`LanguageId::index`](crate::lang::ids::LanguageId::index).
static COMPILED: [OnceLock<Result<CompiledPatterns, String>>; 9] = [
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
];

/// The adapter's compiled patterns, initializing them on first use. A
/// pattern that fails to compile is an internal invariant: the pinned
/// patterns are RE2-compatible and a unit test compiles all of them, so a
/// failure here means someone shipped a bad pattern.
///
/// The cache is keyed by the adapter's language id, so it serves the
/// registered adapters (`adapters::adapter_for`); an ad-hoc adapter (a
/// test-only table) is compiled with [`compile_patterns`] instead.
pub fn compiled(adapter: &LanguageAdapter) -> Result<&'static CompiledPatterns, Error> {
    let slot = &COMPILED[adapter.id.index()];
    let result = slot.get_or_init(|| compile(adapter));
    result
        .as_ref()
        .map_err(|message| compile_error(adapter, message))
}

/// Compiles an adapter's patterns without caching them — the seam for
/// adapters that are not the registered one for their language id.
pub fn compile_patterns(adapter: &LanguageAdapter) -> Result<CompiledPatterns, Error> {
    compile(adapter).map_err(|message| compile_error(adapter, &message))
}

fn compile_error(adapter: &LanguageAdapter, message: &str) -> Error {
    Error::Invariant(format!(
        "pattern compilation failed for the {} adapter: {message}",
        adapter.id.as_str(),
    ))
}

/// The regex source a doc functional tag compiles to (see
/// [`CompiledDocTag`]).
fn doc_tag_source(tag: &str) -> String {
    let mut source = tag
        .split(' ')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(r"\s*");
    if tag
        .chars()
        .last()
        .is_some_and(|last| last.is_alphanumeric() || last == '_')
    {
        source.push_str(r"\b");
    }
    source
}

/// Compiles one adapter's regex-bearing data. A `String` error names the
/// offending pattern.
fn compile(adapter: &LanguageAdapter) -> Result<CompiledPatterns, String> {
    let mut rows = Vec::new();
    let mut compile_row = |row: &'static DirectiveRow| -> Result<(), String> {
        let regex_for = |what: &str, source: &str| -> Result<Regex, String> {
            let regex = Regex::new(source)
                .map_err(|error| format!("{what} pattern {source:?}: {error}"))?;
            if matches!(row.tag, TagSpec::Family { .. })
                && !regex.capture_names().any(|name| name == Some("word"))
            {
                return Err(format!(
                    "{what} pattern {source:?} has a Family tag but no `word` capture group",
                ));
            }
            Ok(regex)
        };
        let pattern = match row.pattern {
            Pattern::Lit(needle) => CompiledPattern::Literal { needle },
            Pattern::Re(source) => CompiledPattern::Body {
                regex: regex_for("body", source)?,
            },
            Pattern::RawRe(source) => CompiledPattern::Raw {
                regex: regex_for("raw", source)?,
            },
        };
        rows.push(CompiledRow { row, pattern });
        Ok(())
    };
    for row in adapter.directives {
        compile_row(row)?;
    }
    for row in GLOBAL_DIRECTIVES {
        compile_row(row)?;
    }
    let line_opener = Regex::new(adapter.line_opener)
        .map_err(|error| format!("line opener {:?}: {error}", adapter.line_opener))?;
    let mut doc_functional_tags = Vec::new();
    if let Some(doc) = &adapter.doc {
        for &tag in doc.functional_tags {
            let source = doc_tag_source(tag);
            let regex = Regex::new(&source)
                .map_err(|error| format!("doc functional tag {tag:?}: {error}"))?;
            doc_functional_tags.push(CompiledDocTag { source: tag, regex });
        }
    }
    let mut slot_rejects = Vec::new();
    for reject in adapter.slot_rejects {
        let regex = Regex::new(reject.pattern)
            .map_err(|error| format!("slot reject {:?}: {error}", reject.pattern))?;
        for kind in reject.slots {
            slot_rejects.push(CompiledSlotReject {
                kind: *kind,
                regex: regex.clone(),
                reason: reject.reason,
            });
        }
    }
    Ok(CompiledPatterns {
        rows,
        line_opener,
        doc_functional_tags,
        slot_rejects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::adapters::ALL_ADAPTERS;

    #[test]
    fn every_adapters_patterns_compile() {
        for adapter in ALL_ADAPTERS {
            let compiled = compiled(adapter).expect("every adapter's patterns compile");
            // Each language's line opener matches a representative opener for its prefix family (`//` vs `#`). Python's `^#:?` also admits Sphinx `#:` lines, so it needs a `#`-style sample rather than the c-family `//`.
            let opener_sample = if adapter.id == LanguageId::Python {
                "#: sphinx-style"
            } else {
                "// note"
            };
            assert!(
                compiled.line_opener.is_match(opener_sample),
                "{}: line opener must match {opener_sample:?}",
                adapter.id.as_str(),
            );
        }
    }

    #[test]
    fn a_bad_pattern_is_a_compile_error_not_a_panic() {
        let bad = LanguageAdapter {
            id: LanguageId::Javascript,
            comment_node_kinds: &[],
            line_prefixes: &[],
            line_opener: "((( not a regex",
            block_delimiters: None,
            block_comments_nest: false,
            doc: None,
            directives: &[],
            positional_rules: &[],
            declaration_kinds: &[],
            container_kinds: &[],
            closing_tokens: &[],
            name_fields: &[],
            exported_rule: None,
            anchor_wrappers: &[],
            slot_rejects: &[],
            referenced_kinds: &[],
        };
        let message = compile(&bad).expect_err("a bad pattern must be an error");
        assert!(
            message.contains("line opener"),
            "names the pattern: {message}"
        );
    }

    #[test]
    fn doc_tags_tolerate_whitespace_and_respect_tag_boundaries() {
        let matches = |tag: &str, text: &str| {
            Regex::new(&doc_tag_source(tag))
                .expect("doc tag sources compile")
                .is_match(text)
        };
        assert!(matches("@param {", "@param {string} x"));
        assert!(matches("@param {", "@param{string} x"));
        assert!(matches("@param {", "@param   {string} x"));
        assert!(!matches("@param {", "@param x"));
        assert!(matches("@class", "@class Foo"));
        assert!(matches("@class", "@class"));
        assert!(!matches("@class", "@classdesc Foo"));
        assert!(!matches("@private", "@privateRemarks notes"));
        assert!(matches("@deprecated", "@deprecated."));
        assert!(!matches("@type", "@typedef {Foo} Bar"));
    }

    #[test]
    fn a_family_regex_row_needs_a_word_group() {
        static ROWS: &[DirectiveRow] = &[DirectiveRow {
            pattern: Pattern::Re(r"^pylint:\s*\w+"),
            category: "linter",
            tag: TagSpec::Family {
                prefix: "directive:pylint-",
            },
            scope: Scope::Inline,
            scope_when_trailing: None,
            line_window: None,
            placement: Placement::Anywhere,
        }];
        let adapter = LanguageAdapter {
            directives: ROWS,
            ..crate::lang::adapters::python::ADAPTER
        };
        let message = compile(&adapter).expect_err("a Family regex needs `word`");
        assert!(message.contains("word"), "{message}");
    }

    #[test]
    fn compiled_slots_stay_in_sync_with_language_ids() {
        assert_eq!(COMPILED.len(), LanguageId::ALL.len());
    }
}
