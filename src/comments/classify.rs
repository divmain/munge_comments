//! The comment classifier: a pure function over a whole file's scanned
//! comments and source text, producing each comment's classification,
//! tags, effective directive scope and line window as plain data — the
//! adjacency pass and every later stage never re-match patterns.
//!
//! Per comment, in order:
//!
//! 1. **Directive rows** — the adapter's table and [`GLOBAL_DIRECTIVES`](crate::lang::adapters::GLOBAL_DIRECTIVES),
//!    matched together. `Lit` rows are case-sensitive substrings of the raw
//!    text, `Re` rows unanchored regexes on [`comment_body`], `RawRe` rows
//!    unanchored regexes on the raw text. Every matching row contributes
//!    its tag; the effective scope is the most specific (`Next` > `File` >
//!    `Inline`) after `scope_when_trailing`; a row's [`Placement`] is
//!    checked against the physical line each match sits on (modelines).
//! 2. **Tag flags** (`TODO`, `FIXME`, …): word-boundary, case-sensitive.
//! 3. **Doc policies**: load-bearing doc tags (`doc.functional_tags`) make a
//!    doc comment functional; otherwise the policy decides between
//!    `doc-protected` and prose (`ExportProtected` consumes the scanner's
//!    `anchor_exported`, computed by the adapter's `exported_rule`).
//! 4. The **JSX-container** tag.
//!
//! Then the whole-file **positional rules** run over the classified list:
//! the adapter's `positional_rules` (PEP 263 cookie, cgo preamble, Go
//! example output, EOF source maps) and the global license header run.
//!
//! [`comment_body`]: super::sanitize::comment_body

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::errors::Error;
use crate::lang::adapter::{
    CompiledPattern, CompiledPatterns, CompiledRow, DocPolicy, LanguageAdapter, Placement,
    PositionalRuleId, Scope, TagSpec, compiled,
};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::lang::parse;
use crate::lang::types::{PlainNode, first_child_with_field};

use super::sanitize::{CommentBody, comment_body_with_lines};
use super::types::{Anchor, Classification, CommentKind, DeclInfo, Position, ScannedComment};

/// The tag vocabulary other code and tests depend on, sorted: the
/// positional/category tags, the record tags, the tag-flag tags, and the
/// `directive:` tags the classifier derives — for doc `functional_tags`
/// and for every `Fixed` directive-row tag. (`Family` rows derive their
/// tags from the matched sub-directive word, so those are open-ended.)
pub const TAGS: &[&str] = &[
    "cgo-preamble",
    "directive-adjacent",
    "directive:alpha",
    "directive:arg",
    "directive:argument",
    "directive:augments",
    "directive:beta",
    "directive:biome-ignore",
    "directive:biome-ignore-all",
    "directive:biome-ignore-end",
    "directive:biome-ignore-start",
    "directive:build",
    "directive:c8-ignore",
    "directive:c8-ignore-next",
    "directive:callback",
    "directive:checkstyle-off",
    "directive:checkstyle-on",
    "directive:clang-format-off",
    "directive:clang-format-on",
    "directive:class",
    "directive:codeql",
    "directive:compiletest-annotation",
    "directive:constructor",
    "directive:coverity",
    "directive:cppcheck-suppress",
    "directive:deno-lint-ignore",
    "directive:deno-lint-ignore-file",
    "directive:deprecated",
    "directive:dprint-ignore",
    "directive:dprint-ignore-file",
    "directive:endregion",
    "directive:enum",
    "directive:eslint-config",
    "directive:eslint-disable",
    "directive:eslint-disable-next-line",
    "directive:eslint-enable",
    "directive:eslint-env",
    "directive:experimental",
    "directive:export",
    "directive:exported",
    "directive:extends",
    "directive:fall-through",
    "directive:fallthrough",
    "directive:flake8-noqa",
    "directive:flow",
    "directive:flow-comment-type",
    "directive:flow-expected-error",
    "directive:flow-fix-me",
    "directive:flow-ignore",
    "directive:flow-issue",
    "directive:formatter-off",
    "directive:formatter-on",
    "directive:gitleaks-allow",
    "directive:global",
    "directive:go-generated",
    "directive:implements",
    "directive:import",
    "directive:internal",
    "directive:istanbul-ignore-else",
    "directive:istanbul-ignore-file",
    "directive:istanbul-ignore-if",
    "directive:istanbul-ignore-next",
    "directive:legal-comment",
    "directive:lgtm",
    "directive:line",
    "directive:lint",
    "directive:lint-file-ignore",
    "directive:lint-ignore",
    "directive:no-side-effects",
    "directive:node-coverage-disable",
    "directive:node-coverage-enable",
    "directive:node-coverage-ignore-next",
    "directive:noinspection",
    "directive:nolint",
    "directive:nolintbegin",
    "directive:nolintend",
    "directive:nolintnextline",
    "directive:non-nls",
    "directive:nopmd",
    "directive:noqa",
    "directive:nosec",
    "directive:nosemgrep",
    "directive:nosonar",
    "directive:overload",
    "directive:override",
    "directive:oxlint-disable",
    "directive:oxlint-disable-next-line",
    "directive:oxlint-enable",
    "directive:param",
    "directive:pragma-allowlist-nextline-secret",
    "directive:pragma-allowlist-secret",
    "directive:preserve",
    "directive:prettier-ignore",
    "directive:private",
    "directive:prop",
    "directive:property",
    "directive:protected",
    "directive:public",
    "directive:pylint-disable-next",
    "directive:readonly",
    "directive:region",
    "directive:return",
    "directive:returns",
    "directive:ruff-noqa",
    "directive:safety",
    "directive:satisfies",
    "directive:spotless-off",
    "directive:spotless-on",
    "directive:template",
    "directive:this",
    "directive:ts-check",
    "directive:ts-expect-error",
    "directive:ts-ignore",
    "directive:ts-nocheck",
    "directive:tslint-disable",
    "directive:tslint-disable-next-line",
    "directive:tslint-enable",
    "directive:type",
    "directive:typedef",
    "directive:v8-ignore",
    "directive:v8-ignore-next",
    "directive:vite-ignore",
    "encoding-cookie",
    "example-output",
    "fixme",
    "flagged-for-review",
    "hack",
    "jsx-expression",
    "jsx-pragma",
    "license",
    "line-splice",
    "lone-cr",
    "modeline",
    "note",
    "offline",
    "pure-annotation",
    "sanitize-failed",
    "source-map",
    "test-environment",
    "todo",
    "unicode-escape",
    "verify-rail",
    "warning",
    "xxx",
];

/// Tag flags, matched word-boundary and case-sensitively on the uppercase
/// token (`NOTE` matches, `Notes` does not); stored lowercase.
static TAG_FLAGS: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"\b(TODO|FIXME|HACK|WARNING|XXX|NOTE)\b"));

/// The Go example-output marker, on a comment body (go/doc's
/// `outputPrefix`).
static GO_EXAMPLE_OUTPUT: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(unordered )?output:"));

/// A source-map magic comment, on a comment body: V8/browsers read
/// `//# sourceMappingURL=` / `//@ sourceURL=` (and the `/*# … */` form).
static SOURCE_MAP: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"^[#@][ \t]*source(?:Mapping)?URL="));

fn static_regex(
    cell: &'static LazyLock<Result<Regex, regex::Error>>,
) -> Result<&'static Regex, Error> {
    LazyLock::force(cell)
        .as_ref()
        .map_err(|error| Error::Invariant(format!("classifier regex failed to compile: {error}")))
}

/// Classification context: the whole file's source text and identity.
#[derive(Debug, Clone, Copy)]
pub struct ClassifyCtx<'a> {
    /// The file's full source text.
    pub text: &'a str,
    /// The file name (name-dependent rules: Go's `_test.go`).
    pub file_name: &'a str,
    /// The file's language.
    pub language: LanguageId,
    /// Whether the file starts with a shebang line (excluded from records
    /// and not a code token; it lets an emacs `-*-` line 2 count).
    pub has_shebang: bool,
}

/// One classified comment: the scanned record's fields plus classification,
/// tags (sorted, deduplicated), the effective directive scope and the
/// matched line window, as plain data.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedComment {
    /// Syntactic kind (`line`/`block`/`doc`).
    pub kind: CommentKind,
    /// Start byte offset.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
    /// 1-based line of `start`.
    pub line: u32,
    /// 1-based column of `start`, in Unicode scalar values.
    pub column: u32,
    /// Raw comment text including delimiters.
    pub text: String,
    /// Leading / trailing / dangling.
    pub position: Position,
    /// The code this comment annotates.
    pub anchor: Anchor,
    /// Whether the anchor declaration is exported (JavaScript).
    pub anchor_exported: bool,
    /// Whether this comment is the sole content of a JSX `{/* … */}`.
    pub jsx_container_sole: bool,
    /// End of the last code token before this comment.
    pub prev_code_end: Option<usize>,
    /// Start of the first code token after this comment (`text.len()` when
    /// none).
    pub next_code_start: usize,
    /// The root's named child strictly containing this comment, if any.
    pub enclosing_top_level: Option<DeclInfo>,
    /// `functional` / `prose` / `doc-protected`.
    pub classification: Classification,
    /// Every tag this comment earned.
    pub tags: BTreeSet<String>,
    /// The most specific scope among matched directive rows
    /// (`Next` > `File` > `Inline`), after `scope_when_trailing`.
    pub directive_scope: Option<Scope>,
    /// The largest line window among matched line-window rows.
    pub directive_line_window: Option<usize>,
}

impl ClassifiedComment {
    /// An unclassified (prose, untagged) record for a scanned comment.
    fn from_scanned(comment: ScannedComment) -> Self {
        Self {
            kind: comment.kind,
            start: comment.start,
            end: comment.end,
            line: comment.line,
            column: comment.column,
            text: comment.text,
            position: comment.position,
            anchor: comment.anchor,
            anchor_exported: comment.anchor_exported,
            jsx_container_sole: comment.jsx_container_sole,
            prev_code_end: comment.prev_code_end,
            next_code_start: comment.next_code_start,
            enclosing_top_level: comment.enclosing_top_level,
            classification: Classification::Prose,
            tags: BTreeSet::new(),
            directive_scope: None,
            directive_line_window: None,
        }
    }

    /// Marks the comment functional with `tag` (functional dominates every
    /// other classification).
    fn mark_functional(&mut self, tag: &str) {
        self.classification = Classification::Functional;
        self.tags.insert(tag.to_string());
    }

    /// The 1-based line holding the comment's last byte.
    fn end_line(&self) -> usize {
        self.line as usize + self.text.matches('\n').count()
    }
}

/// Classifies a whole file's comments (in ascending offset order, as the
/// scanner produces them) under the adapter `ctx.language` maps to.
pub fn classify_all(
    comments: Vec<ScannedComment>,
    ctx: ClassifyCtx<'_>,
) -> Result<Vec<ClassifiedComment>, Error> {
    let adapter = adapter_for(ctx.language);
    classify_with(comments, ctx, adapter, compiled(adapter)?)
}

/// The classifier over an explicit adapter and its compiled patterns — the
/// seam for adapters that are not the registered one for their language
/// (test-only directive tables, compiled with
/// [`compile_patterns`](crate::lang::adapter::compile_patterns)).
pub fn classify_with(
    comments: Vec<ScannedComment>,
    ctx: ClassifyCtx<'_>,
    adapter: &LanguageAdapter,
    patterns: &CompiledPatterns,
) -> Result<Vec<ClassifiedComment>, Error> {
    if adapter.id != ctx.language {
        return Err(Error::Invariant(format!(
            "classifier called with the {} adapter for a {} file",
            adapter.id.as_str(),
            ctx.language.as_str(),
        )));
    }
    let tag_flags = static_regex(&TAG_FLAGS)?;
    let file = FileLines::new(ctx.text, ctx.has_shebang);

    let mut classified = Vec::with_capacity(comments.len());
    let mut bodies = Vec::with_capacity(comments.len());
    for comment in comments {
        let body = comment_body_with_lines(&comment.text, adapter);
        let mut record = ClassifiedComment::from_scanned(comment);
        let mut functional = false;
        let mut doc_protected = false;

        // 1. Directive rows (the adapter's and the global ones, together).
        let mut scopes = Vec::new();
        for row in &patterns.rows {
            if !row_matches(row, &record.text, &body, Some((&file, record.line))) {
                continue;
            }
            functional = true;
            record.tags.insert(row_tag(row, &record.text, &body));
            scopes.push(effective_scope(row, record.position));
            if let Some(window) = row.row.line_window {
                let window = window(&body.text);
                record.directive_line_window = Some(
                    record
                        .directive_line_window
                        .map_or(window, |largest| largest.max(window)),
                );
            }
        }
        record.directive_scope = most_specific(&scopes);

        // 2. Tag flags.
        for flag in tag_flags.captures_iter(&body.text) {
            if let Some(word) = flag.get(1) {
                record.tags.insert(word.as_str().to_ascii_lowercase());
            }
        }

        // 3. Doc policies.
        if record.kind == CommentKind::Doc
            && let Some(doc) = &adapter.doc
        {
            for tag in &patterns.doc_functional_tags {
                if tag.regex.is_match(&body.text) {
                    functional = true;
                    if let Some(derived) = derive_directive_tag(tag.source) {
                        record.tags.insert(derived);
                    }
                }
            }
            doc_protected = match doc.policy {
                DocPolicy::AlwaysProtected => true,
                DocPolicy::ExportProtected => record.anchor_exported,
                DocPolicy::Prose => false,
            };
        }

        // 4. A JSX container's sole comment keeps its classification but is
        // non-editable (decision row 1b).
        if record.jsx_container_sole {
            record.tags.insert("jsx-expression".to_string());
        }

        record.classification = if functional {
            Classification::Functional
        } else if doc_protected {
            Classification::DocProtected
        } else {
            Classification::Prose
        };
        classified.push(record);
        bodies.push(body.text);
    }

    for rule in adapter.positional_rules {
        match rule {
            PositionalRuleId::PepEncodingCookie => mark_encoding_cookie(&mut classified, ctx.text),
            PositionalRuleId::SourceMapEof => {
                mark_source_map(&mut classified, &bodies, ctx.text)?;
            }
            PositionalRuleId::GoExampleOutput => {
                mark_go_example_output(&mut classified, &bodies, ctx.file_name)?;
            }
            PositionalRuleId::CgoPreamble => {
                mark_cgo_preamble(&mut classified, ctx.text, adapter.id)?;
            }
        }
    }
    mark_license_run(&mut classified, ctx.text);
    Ok(classified)
}

/// Whether `raw` matches any functional pattern of the position-free
/// subset: the adapter's directive rows, [`GLOBAL_DIRECTIVES`](crate::lang::adapters::GLOBAL_DIRECTIVES) (placement
/// windows ignored) and the adapter's doc `functional_tags` (checked
/// whatever the comment's kind).
///
/// Exported for the sanitizer: written text that matches would itself
/// classify as functional, so it is refused. The pinned patterns always
/// compile (a unit test compiles every adapter's); should compilation ever
/// fail, this answers `true` — fail closed — so nothing is written.
pub fn matches_functional_pattern(raw: &str, adapter: &LanguageAdapter) -> bool {
    let Ok(patterns) = compiled(adapter) else {
        return true;
    };
    let body = comment_body_with_lines(raw, adapter);
    patterns
        .rows
        .iter()
        .any(|row| row_matches(row, raw, &body, None))
        || patterns
            .doc_functional_tags
            .iter()
            .any(|tag| tag.regex.is_match(&body.text))
}

/// The encoding named by a counting PEP 263 cookie (python): a comment
/// alone on its line, on line 1 — or on line 2 when line 1 is blank or
/// comment-only (a shebang line counts) — matching
/// `coding[:=][ \t]*[-\w.]+`, evaluated on the source text.
pub fn declared_encoding(text: &str) -> Option<String> {
    pep263_cookie(text).map(|cookie| cookie.encoding.to_string())
}

/// Whether a declared encoding is a UTF-8 alias (`utf-8`, `utf8`, `utf_8`,
/// `utf-8-sig`), case-insensitively.
pub fn is_utf8_alias(name: &str) -> bool {
    ["utf-8", "utf8", "utf_8", "utf-8-sig"]
        .iter()
        .any(|alias| name.eq_ignore_ascii_case(alias))
}

/// The `directive:` tag the derivation rule assigns a literal pattern: the
/// pattern stripped of comment sigils, `@` and `#`, kebab-cased
/// (`@ts-expect-error` → `directive:ts-expect-error`, `//nolint` →
/// `directive:nolint`, `$FlowFixMe` → `directive:flow-fix-me`). `None` when
/// the pattern has no alphanumeric content (such a row picks a `Fixed`
/// tag of its own).
pub fn derive_directive_tag(pattern: &str) -> Option<String> {
    let name = kebab(pattern);
    (!name.is_empty()).then(|| format!("directive:{name}"))
}

/// Kebab-cases a name: runs of non-alphanumerics become single `-`,
/// camel-case boundaries (`fixMe`, `XMLHttp`) split, everything lowercase,
/// no leading or trailing `-`.
fn kebab(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let mut out = String::new();
    let mut pending_dash = false;
    for (index, &character) in characters.iter().enumerate() {
        if !character.is_alphanumeric() {
            pending_dash = !out.is_empty();
            continue;
        }
        let previous = index.checked_sub(1).map(|at| characters[at]);
        let next = characters.get(index + 1).copied();
        let camel_boundary = character.is_uppercase()
            && previous.is_some_and(|previous| {
                previous.is_lowercase()
                    || previous.is_numeric()
                    || (previous.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        if (pending_dash || camel_boundary) && !out.is_empty() {
            out.push('-');
        }
        pending_dash = false;
        out.extend(character.to_lowercase());
    }
    out
}

/// The file's line facts the placement windows need.
struct FileLines {
    /// Number of lines (a final line terminator does not start a line).
    total: usize,
    /// Whether line 1 is a shebang.
    has_shebang: bool,
}

impl FileLines {
    fn new(text: &str, has_shebang: bool) -> Self {
        let breaks = text.matches('\n').count();
        let total = if text.is_empty() || text.ends_with('\n') {
            breaks
        } else {
            breaks + 1
        };
        Self { total, has_shebang }
    }

    /// Whether 1-based `line` satisfies `placement`.
    fn admits(&self, placement: Placement, line: usize) -> bool {
        match placement {
            Placement::Anywhere => true,
            Placement::FileEdges(lines) => line <= lines || line + lines > self.total,
            Placement::FirstLineOrAfterShebang => line == 1 || (line == 2 && self.has_shebang),
        }
    }
}

/// Whether a compiled row matches a comment. With `placement` (the file's
/// lines and the comment's 1-based first line), a row whose [`Placement`]
/// is not `Anywhere` matches only when one of its matches lies on an
/// admitted physical line.
fn row_matches(
    row: &CompiledRow,
    raw: &str,
    body: &CommentBody,
    placement: Option<(&FileLines, u32)>,
) -> bool {
    let window = match placement {
        Some((file, first_line)) if row.row.placement != Placement::Anywhere => {
            Some((file, first_line as usize))
        }
        _ => None,
    };
    let Some((file, first_line)) = window else {
        return match &row.pattern {
            CompiledPattern::Literal { needle } => raw.contains(needle),
            CompiledPattern::Body { regex } => regex.is_match(&body.text),
            CompiledPattern::Raw { regex } => regex.is_match(raw),
        };
    };
    let line_in_raw = |offset: usize| raw[..offset].matches('\n').count();
    let mut lines: Box<dyn Iterator<Item = usize>> = match &row.pattern {
        CompiledPattern::Literal { needle } => Box::new(
            raw.match_indices(needle)
                .map(move |(at, _)| line_in_raw(at)),
        ),
        CompiledPattern::Body { regex } => Box::new(
            regex
                .find_iter(&body.text)
                .map(|found| body.raw_line_at(last_char_offset(&found))),
        ),
        CompiledPattern::Raw { regex } => Box::new(
            regex
                .find_iter(raw)
                .map(move |found| line_in_raw(last_char_offset(&found))),
        ),
    };
    lines.any(|relative| file.admits(row.row.placement, first_line + relative))
}

/// The offset of a match's last character: the line a match "sits on" is
/// the one its directive text ends on (a vim match may begin with the
/// whitespace that precedes it, including a line break).
fn last_char_offset(found: &regex::Match<'_>) -> usize {
    found
        .as_str()
        .char_indices()
        .last()
        .map_or(found.start(), |(at, _)| found.start() + at)
}

/// The tag a matched row contributes: `Fixed` verbatim; `Family` appends
/// the kebab-cased sub-directive word (the `word` capture group of a
/// regex row; the word directly following a literal), or is the bare
/// prefix when there is no word.
fn row_tag(row: &CompiledRow, raw: &str, body: &CommentBody) -> String {
    match row.row.tag {
        TagSpec::Fixed(tag) => tag.to_string(),
        TagSpec::Family { prefix } => {
            let word = match &row.pattern {
                CompiledPattern::Literal { needle } => raw.find(needle).map(|at| {
                    let after = raw[at + needle.len()..].trim_start_matches([' ', '\t']);
                    let length = after
                        .find(|character: char| {
                            !(character.is_alphanumeric() || character == '_' || character == '-')
                        })
                        .unwrap_or(after.len());
                    &after[..length]
                }),
                CompiledPattern::Body { regex } => regex
                    .captures(&body.text)
                    .and_then(|captures| captures.name("word"))
                    .map(|word| word.as_str()),
                CompiledPattern::Raw { regex } => regex
                    .captures(raw)
                    .and_then(|captures| captures.name("word"))
                    .map(|word| word.as_str()),
            };
            match word.map(kebab).filter(|word| !word.is_empty()) {
                Some(word) => format!("{prefix}{word}"),
                None => prefix.trim_end_matches('-').to_string(),
            }
        }
    }
}

/// A matched row's scope for a comment at `position`: `scope_when_trailing`
/// replaces the row's scope when code precedes the comment on its line.
fn effective_scope(row: &CompiledRow, position: Position) -> Scope {
    match (position, row.row.scope_when_trailing) {
        (Position::Trailing, Some(trailing)) => trailing,
        _ => row.row.scope,
    }
}

/// The most specific scope among matches: `Next` > `File` > `Inline`.
fn most_specific(scopes: &[Scope]) -> Option<Scope> {
    scopes.iter().copied().max_by_key(|scope| match scope {
        Scope::Next => 2,
        Scope::File => 1,
        Scope::Inline => 0,
    })
}

/// A counting PEP 263 cookie: the byte offset of its `#` and the encoding
/// it names.
struct Pep263Cookie<'a> {
    hash_offset: usize,
    encoding: &'a str,
}

/// CPython's `check_coding_spec` over the source's first two lines: the
/// comment must be the first non-blank text on its line; line 2 counts
/// only when line 1 is blank or comment-only. A leading BOM is not part of
/// line 1's text.
fn pep263_cookie(text: &str) -> Option<Pep263Cookie<'_>> {
    let skip = if text.starts_with('\u{FEFF}') {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    };
    let mut offset = skip;
    let mut lines = text[skip..].split('\n').map(|line| {
        let start = offset;
        offset += line.len() + 1;
        (start, line.strip_suffix('\r').unwrap_or(line))
    });
    let (start, line1) = lines.next()?;
    if let Some(cookie) = coding_spec(line1, start) {
        return Some(cookie);
    }
    let first_non_blank = line1.trim_start_matches([' ', '\t', '\x0c']);
    if !(first_non_blank.is_empty() || first_non_blank.starts_with('#')) {
        return None;
    }
    let (start, line2) = lines.next()?;
    coding_spec(line2, start)
}

/// The coding spec of one source line starting at `line_start`: a
/// comment-only line (`[ \t\f]*#`) containing `coding` + `:`/`=` + blanks +
/// a non-empty `[-\w.]+` name (ASCII), first such occurrence — tokenize's
/// `^[ \t\f]*#.*?coding[:=][ \t]*([-\w.]+)`.
fn coding_spec(line: &str, line_start: usize) -> Option<Pep263Cookie<'_>> {
    let indent = line.len() - line.trim_start_matches([' ', '\t', '\x0c']).len();
    let comment = line[indent..].strip_prefix('#')?;
    let mut search = 0;
    while let Some(found) = comment[search..].find("coding") {
        let after = search + found + "coding".len();
        search = after;
        let Some(rest) = comment[after..].strip_prefix([':', '=']) else {
            continue;
        };
        let rest = rest.trim_start_matches([' ', '\t']);
        let length = rest
            .find(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
            })
            .unwrap_or(rest.len());
        if length > 0 {
            return Some(Pep263Cookie {
                hash_offset: line_start + indent,
                encoding: &rest[..length],
            });
        }
    }
    None
}

/// `PepEncodingCookie`: the comment holding a counting cookie.
fn mark_encoding_cookie(classified: &mut [ClassifiedComment], text: &str) {
    let Some(cookie) = pep263_cookie(text) else {
        return;
    };
    for comment in classified {
        if comment.start == cookie.hash_offset {
            comment.mark_functional("encoding-cookie");
        }
    }
}

/// `SourceMapEof`: a `sourceMappingURL=`/`sourceURL=` magic comment that is
/// the file's trailing comment (no code after it).
fn mark_source_map(
    classified: &mut [ClassifiedComment],
    bodies: &[String],
    text: &str,
) -> Result<(), Error> {
    let source_map = static_regex(&SOURCE_MAP)?;
    for (comment, body) in classified.iter_mut().zip(bodies) {
        if comment.next_code_start == text.len() && source_map.is_match(body) {
            comment.mark_functional("source-map");
        }
    }
    Ok(())
}

/// go test's example-function name rule (`isTest(name, "Example")`): the
/// name is `Example`, or continues with a non-lowercase character.
fn is_example_name(name: &str) -> bool {
    name.strip_prefix("Example")
        .is_some_and(|rest| rest.chars().next().is_none_or(|next| !next.is_lowercase()))
}

/// `GoExampleOutput`: in a `_test.go` file, inside a top-level
/// `func Example…`, the comment matching `(?i)^\s*(unordered )?output:` and
/// every comment after it to the end of that function.
fn mark_go_example_output(
    classified: &mut [ClassifiedComment],
    bodies: &[String],
    file_name: &str,
) -> Result<(), Error> {
    if !file_name.ends_with("_test.go") {
        return Ok(());
    }
    let output = static_regex(&GO_EXAMPLE_OUTPUT)?;
    // The example function whose output block has been entered (comments
    // arrive in offset order and functions do not overlap).
    let mut entered: Option<usize> = None;
    for (comment, body) in classified.iter_mut().zip(bodies) {
        let Some(function) = comment.enclosing_top_level.as_ref().filter(|declaration| {
            declaration.node_kind == "function_declaration"
                && declaration.name.as_deref().is_some_and(is_example_name)
        }) else {
            continue;
        };
        if entered == Some(function.start) || output.is_match(body) {
            entered = Some(function.start);
            comment.mark_functional("example-output");
        }
    }
    Ok(())
}

/// `CgoPreamble`: the doc-comment block of an `import "C"` spec is compiled
/// C code. cmd/cgo reads the spec's own doc comment (an `import_spec`
/// inside a grouped import) and falls back to the declaration's doc
/// comment when the declaration has a single spec — so a block preceding
/// either a `"C"` spec of an `import_spec_list` or a single-spec
/// `import_declaration` whose path is `"C"` is the preamble.
///
/// The import shapes come from the file's parse tree (parsed only when the
/// source contains a `"C"` literal at all).
fn mark_cgo_preamble(
    classified: &mut [ClassifiedComment],
    text: &str,
    language: LanguageId,
) -> Result<(), Error> {
    if classified.is_empty() || !text.contains("\"C\"") {
        return Ok(());
    }
    let tree = parse(text, language)?;
    for target in cgo_doc_targets(&tree.root, text) {
        mark_doc_block(classified, text, target, "cgo-preamble");
    }
    Ok(())
}

/// The start offsets whose doc comment cgo reads as the preamble.
fn cgo_doc_targets(root: &PlainNode, text: &str) -> Vec<usize> {
    let is_c = |spec: &PlainNode| {
        first_child_with_field(spec, "path")
            .is_some_and(|path| &text[path.start..path.end] == "\"C\"")
    };
    let mut targets = Vec::new();
    for declaration in root
        .children
        .iter()
        .filter(|node| node.kind == "import_declaration")
    {
        let mut specs = Vec::new();
        for child in &declaration.children {
            match child.kind {
                "import_spec" => specs.push(child),
                "import_spec_list" => {
                    for spec in child
                        .children
                        .iter()
                        .filter(|node| node.kind == "import_spec")
                    {
                        // A grouped spec's own doc comment.
                        if is_c(spec) {
                            targets.push(spec.start);
                        }
                        specs.push(spec);
                    }
                }
                _ => {}
            }
        }
        // The declaration's doc comment, for a single-spec `import "C"`.
        if let [spec] = specs.as_slice()
            && is_c(spec)
        {
            targets.push(declaration.start);
        }
    }
    targets
}

/// Marks the Go doc-comment block of the code token at `target`: the
/// comment group (consecutive comments, each starting no more than one line
/// after the previous one ends, none sharing a line with preceding code)
/// whose last comment ends on the line immediately before the token.
fn mark_doc_block(classified: &mut [ClassifiedComment], text: &str, target: usize, tag: &str) {
    let Some(last) = classified
        .iter()
        .rposition(|comment| comment.next_code_start == target && comment.end <= target)
    else {
        return;
    };
    let target_line = 1 + text[..target].matches('\n').count();
    let lead = &classified[last];
    if lead.position == Position::Trailing || lead.end_line() + 1 != target_line {
        return;
    }
    let mut first = last;
    while let Some(previous) = first.checked_sub(1) {
        let candidate = &classified[previous];
        if candidate.next_code_start != target
            || candidate.position == Position::Trailing
            || candidate.end_line() + 1 < classified[first].line as usize
        {
            break;
        }
        first = previous;
    }
    for comment in &mut classified[first..=last] {
        comment.mark_functional(tag);
    }
}

/// The license header run: the maximal run of comments starting at the
/// file's first comment (shebang excluded) with no code token before it
/// (`prev_code_end == None`) and only whitespace between members. The
/// whole run is functional (tag `license`) when any member's text contains
/// `copyright`, `license` or `spdx`, case-insensitively — so keyword-free
/// continuation lines are covered.
fn mark_license_run(classified: &mut [ClassifiedComment], text: &str) {
    if classified
        .first()
        .is_none_or(|first| first.prev_code_end.is_some())
    {
        return;
    }
    let mut length = 1;
    while let Some(next) = classified.get(length) {
        let gap = &text[classified[length - 1].end..next.start];
        if !gap.chars().all(char::is_whitespace) {
            break;
        }
        length += 1;
    }
    let run = &mut classified[..length];
    let mentions_license = run.iter().any(|comment| {
        let lower = comment.text.to_lowercase();
        ["copyright", "license", "spdx"]
            .iter()
            .any(|keyword| lower.contains(keyword))
    });
    if mentions_license {
        for comment in run {
            comment.mark_functional("license");
        }
    }
}

/// The table-driven directive-row suite: one case per row per language.
#[cfg(test)]
#[path = "classify_directive_tests.rs"]
mod directive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::scan::scan_file;
    use crate::comments::types::FileRef;
    use crate::lang::adapter::{DirectiveRow, Pattern, compile_patterns};
    use crate::lang::adapters::ALL_ADAPTERS;
    use crate::lang::adapters::GLOBAL_DIRECTIVES;

    fn classify(text: &str, language: LanguageId, file_name: &str) -> Vec<ClassifiedComment> {
        let scan = scan_file(
            text,
            FileRef {
                language,
                file_name: file_name.to_string(),
            },
        )
        .expect("test snippets scan");
        classify_all(
            scan.comments,
            ClassifyCtx {
                text,
                file_name,
                language,
                has_shebang: scan.has_shebang,
            },
        )
        .expect("test snippets classify")
    }

    fn find<'a>(comments: &'a [ClassifiedComment], needle: &str) -> &'a ClassifiedComment {
        comments
            .iter()
            .find(|comment| comment.text.contains(needle))
            .unwrap_or_else(|| panic!("no comment contains {needle:?}"))
    }

    fn class_of(comments: &[ClassifiedComment], needle: &str) -> Classification {
        find(comments, needle).classification
    }

    fn has_tag(comments: &[ClassifiedComment], needle: &str, tag: &str) -> bool {
        find(comments, needle).tags.contains(tag)
    }

    use Classification::{DocProtected, Functional, Prose};

    // ---- The match engine, against a test-only adapter table. -------------

    fn window_after_next(body: &str) -> usize {
        body.split_whitespace()
            .skip_while(|word| *word != "next")
            .nth(1)
            .and_then(|count| count.parse().ok())
            .unwrap_or(1)
    }

    const fn row(pattern: Pattern, tag: TagSpec, scope: Scope) -> DirectiveRow {
        DirectiveRow {
            pattern,
            category: "test",
            tag,
            scope,
            scope_when_trailing: None,
            line_window: None,
            placement: Placement::Anywhere,
        }
    }

    static TEST_ROWS: &[DirectiveRow] = &[
        row(
            Pattern::Lit("@lit-directive"),
            TagSpec::Fixed("directive:lit-directive"),
            Scope::Inline,
        ),
        row(
            Pattern::Re(r"(?i)^shout:"),
            TagSpec::Fixed("directive:shout"),
            Scope::Inline,
        ),
        // Byte-level rows on the raw text, plus body twins that must miss.
        row(
            Pattern::RawRe(r"\r[^\n]"),
            TagSpec::Fixed("lone-cr"),
            Scope::Inline,
        ),
        row(
            Pattern::Re(r"\r"),
            TagSpec::Fixed("directive:body-cr"),
            Scope::Inline,
        ),
        row(
            Pattern::RawRe(r"(?m)[ \t]+$"),
            TagSpec::Fixed("directive:raw-trailing-blank"),
            Scope::Inline,
        ),
        row(
            Pattern::Re(r"(?m)[ \t]+$"),
            TagSpec::Fixed("directive:body-trailing-blank"),
            Scope::Inline,
        ),
        DirectiveRow {
            scope_when_trailing: Some(Scope::Inline),
            ..row(
                Pattern::Lit("skip-next"),
                TagSpec::Fixed("directive:skip-next"),
                Scope::Next,
            )
        },
        row(
            Pattern::Lit("file-off"),
            TagSpec::Fixed("directive:file-off"),
            Scope::File,
        ),
        DirectiveRow {
            line_window: Some(window_after_next),
            ..row(
                Pattern::Re(r"^cover ignore next\b"),
                TagSpec::Fixed("directive:cover-ignore-next"),
                Scope::Next,
            )
        },
        row(
            Pattern::Lit("//go:"),
            TagSpec::Family {
                prefix: "directive:go-",
            },
            Scope::File,
        ),
        row(
            Pattern::Re(r"^tool:\s*(?P<word>[\w-]+)"),
            TagSpec::Family {
                prefix: "directive:tool-",
            },
            Scope::Inline,
        ),
    ];

    /// Scans `text` as JavaScript and classifies it under the JavaScript
    /// adapter with `TEST_ROWS` as its directive table.
    fn classify_test_table(text: &str) -> Vec<ClassifiedComment> {
        let adapter = LanguageAdapter {
            directives: TEST_ROWS,
            ..crate::lang::adapters::javascript::ADAPTER
        };
        let patterns = compile_patterns(&adapter).expect("test rows compile");
        let scan = scan_file(
            text,
            FileRef {
                language: LanguageId::Javascript,
                file_name: "t.js".to_string(),
            },
        )
        .expect("test snippets scan");
        classify_with(
            scan.comments,
            ClassifyCtx {
                text,
                file_name: "t.js",
                language: LanguageId::Javascript,
                has_shebang: scan.has_shebang,
            },
            &adapter,
            &patterns,
        )
        .expect("test snippets classify")
    }

    fn tags(comment: &ClassifiedComment) -> Vec<&str> {
        comment.tags.iter().map(String::as_str).collect()
    }

    #[test]
    fn literal_rows_match_raw_substrings_case_sensitively() {
        let comments = classify_test_table(
            "// see @lit-directive here\nx();\n// @LIT-DIRECTIVE\ny();\n/* a @lit-directive */\nz();\n",
        );
        assert_eq!(class_of(&comments, "see @lit"), Functional);
        assert_eq!(
            tags(find(&comments, "see @lit")),
            ["directive:lit-directive"]
        );
        assert_eq!(class_of(&comments, "@LIT"), Prose);
        assert_eq!(class_of(&comments, "a @lit"), Functional);
    }

    #[test]
    fn regex_rows_match_the_body_and_inline_case_flags_apply() {
        let comments =
            classify_test_table("// shout: a\nx();\n// SHOUT: b\ny();\n// say shout: c\nz();\n");
        // `^` anchors at the body's start, after the opener is stripped.
        assert_eq!(class_of(&comments, "shout: a"), Functional);
        // `(?i)` makes the row case-insensitive.
        assert_eq!(class_of(&comments, "SHOUT"), Functional);
        assert_eq!(class_of(&comments, "say shout"), Prose);
    }

    #[test]
    fn raw_regex_rows_see_bytes_the_body_hides() {
        let comments =
            classify_test_table("/* a\rb */\nx();\n// trailing   \ny();\n// clean\nz();\n");
        let lone_cr = find(&comments, "a\rb");
        assert!(lone_cr.tags.contains("lone-cr"));
        assert!(
            !lone_cr.tags.contains("directive:body-cr"),
            "comment_body normalizes the lone CR away",
        );
        let trailing = find(&comments, "trailing");
        assert!(trailing.tags.contains("directive:raw-trailing-blank"));
        assert!(
            !trailing.tags.contains("directive:body-trailing-blank"),
            "comment_body trims trailing blanks",
        );
        assert_eq!(class_of(&comments, "clean"), Prose);
    }

    #[test]
    fn multiple_matches_contribute_every_tag_and_the_most_specific_scope() {
        let comments = classify_test_table(
            "// file-off skip-next @lit-directive\nx();\n// file-off @lit-directive\ny();\n// @lit-directive\nz();\n",
        );
        let all = find(&comments, "file-off skip-next");
        assert_eq!(
            tags(all),
            [
                "directive:file-off",
                "directive:lit-directive",
                "directive:skip-next"
            ],
        );
        assert_eq!(all.directive_scope, Some(Scope::Next));
        assert_eq!(
            find(&comments, "file-off @lit").directive_scope,
            Some(Scope::File),
        );
        // An inline-only match.
        assert_eq!(comments[2].text, "// @lit-directive");
        assert_eq!(comments[2].directive_scope, Some(Scope::Inline));
    }

    #[test]
    fn unmatched_comments_have_no_scope() {
        let comments = classify_test_table("// nothing here\nz();\n");
        assert_eq!(comments[0].directive_scope, None);
        assert_eq!(comments[0].directive_line_window, None);
        assert_eq!(comments[0].classification, Prose);
    }

    #[test]
    fn scope_when_trailing_applies_only_after_code() {
        let comments = classify_test_table("// skip-next\nx(); // skip-next\n");
        assert_eq!(comments[0].position, Position::Leading);
        assert_eq!(comments[0].directive_scope, Some(Scope::Next));
        assert_eq!(comments[1].position, Position::Trailing);
        assert_eq!(comments[1].directive_scope, Some(Scope::Inline));
        // A trailing match still combines with other rows' scopes.
        let comments = classify_test_table("x(); // skip-next file-off\n");
        assert_eq!(comments[0].directive_scope, Some(Scope::File));
    }

    #[test]
    fn line_windows_are_evaluated_per_matched_row() {
        let comments = classify_test_table(
            "// cover ignore next 3\na();\n// cover ignore next\nb();\n// skip-next\nc();\n",
        );
        assert_eq!(comments[0].directive_line_window, Some(3));
        // No count: the row's extractor defaults to 1.
        assert_eq!(comments[1].text, "// cover ignore next");
        assert_eq!(comments[1].directive_line_window, Some(1));
        // A next-scoped row without a line window.
        assert_eq!(comments[2].directive_line_window, None);
        assert_eq!(comments[2].directive_scope, Some(Scope::Next));
    }

    #[test]
    fn family_rows_append_the_matched_word() {
        let comments = classify_test_table(
            "//go:build linux\na();\n//go:noinline\nb();\n// tool: skip_file\nc();\n// tool: keepAlive\nd();\n",
        );
        assert_eq!(tags(find(&comments, "go:build")), ["directive:go-build"]);
        assert_eq!(
            tags(find(&comments, "go:noinline")),
            ["directive:go-noinline"]
        );
        assert_eq!(
            tags(find(&comments, "skip_file")),
            ["directive:tool-skip-file"]
        );
        assert_eq!(
            tags(find(&comments, "keepAlive")),
            ["directive:tool-keep-alive"]
        );
        // No word after the literal: the bare family prefix.
        let comments = classify_test_table("//go:\na();\n");
        assert_eq!(tags(&comments[0]), ["directive:go"]);
    }

    #[test]
    fn global_rows_are_matched_alongside_the_adapter_table() {
        let comments =
            classify_test_table("x();\n// SPDX-License-Identifier: MIT @lit-directive\ny();\n");
        assert_eq!(tags(&comments[0]), ["directive:lit-directive", "license"],);
        assert_eq!(comments[0].directive_scope, Some(Scope::File));
    }

    // ---- Tag flags. ---------------------------------------------------------

    #[test]
    fn tag_flags_are_word_bounded_and_case_sensitive() {
        let comments = classify(
            "// TODO: x\na();\n// FIXME and HACK\nb();\n// Notes on NOTEWORTHY things\nc();\n// NOTE: y\nd();\n// XXX WARNING\ne();\n// todo lowercase\nf();\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(tags(find(&comments, "TODO")), ["todo"]);
        assert_eq!(class_of(&comments, "TODO"), Prose);
        assert_eq!(tags(find(&comments, "FIXME")), ["fixme", "hack"]);
        assert!(tags(find(&comments, "Notes")).is_empty());
        assert_eq!(tags(find(&comments, "NOTE: y")), ["note"]);
        assert_eq!(tags(find(&comments, "XXX")), ["warning", "xxx"]);
        assert!(tags(find(&comments, "todo lowercase")).is_empty());
    }

    // ---- Doc policies. ------------------------------------------------------

    #[test]
    fn javascript_doc_policy_and_load_bearing_tags() {
        let text = "/** @param [x] optional */\nfunction a(x) {}\n\
                    /** @private */\nfunction b() {}\n\
                    /** @deprecated use c2 */\nfunction c() {}\n\
                    /** @prop {string} name */\nfunction d() {}\n\
                    /** @argument {number} n */\nfunction e(n) {}\n\
                    /** Adds things. */\nfunction f() {}\n\
                    /** Exported adder. */\nexport function g() {}\n\
                    /** @classdesc not load-bearing */\nfunction h() {}\n";
        let comments = classify(text, LanguageId::Javascript, "a.js");
        assert_eq!(class_of(&comments, "@param [x]"), Functional);
        assert!(has_tag(&comments, "@param [x]", "directive:param"));
        assert_eq!(class_of(&comments, "@private"), Functional);
        assert!(has_tag(&comments, "@private", "directive:private"));
        assert_eq!(class_of(&comments, "@deprecated"), Functional);
        assert_eq!(class_of(&comments, "@prop {"), Functional);
        assert!(has_tag(&comments, "@prop {", "directive:prop"));
        assert_eq!(class_of(&comments, "@argument {"), Functional);
        // Internal JSDoc is prose; exported JSDoc is doc-protected.
        assert_eq!(class_of(&comments, "Adds things"), Prose);
        assert_eq!(class_of(&comments, "Exported adder"), DocProtected);
        assert_eq!(class_of(&comments, "@classdesc"), Prose);
    }

    #[test]
    fn javascript_load_bearing_tags_need_a_doc_comment() {
        let comments = classify(
            "// @param {string} x\nfunction a(x) {}\n/* @type {number} */\nlet n = 1;\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "@param"), Prose);
        assert_eq!(class_of(&comments, "@type"), Prose);
    }

    #[test]
    fn typescript_doc_policy_is_prose_except_load_bearing_tags() {
        let text = "/** @deprecated use g */\nfunction f() {}\n\
                    /** @beta */\nfunction g() {}\n\
                    /** @internal */\nfunction h() {}\n\
                    /** @param {string} x */\nfunction i(x: string) {}\n\
                    /** Exported prose. */\nexport function j() {}\n";
        for (language, name) in [(LanguageId::Typescript, "a.ts"), (LanguageId::Tsx, "a.tsx")] {
            let comments = classify(text, language, name);
            assert_eq!(class_of(&comments, "@deprecated"), Functional);
            assert!(has_tag(&comments, "@deprecated", "directive:deprecated"));
            assert_eq!(class_of(&comments, "@beta"), Functional);
            assert_eq!(class_of(&comments, "@internal"), Functional);
            assert_eq!(class_of(&comments, "@param {string}"), Prose);
            assert_eq!(class_of(&comments, "Exported prose"), Prose);
        }
        // The same `@param {string}` JSDoc in a .js file is load-bearing.
        let comments = classify(
            "/** @param {string} x */\nfunction i(x) {}\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "@param {string}"), Functional);
    }

    #[test]
    fn rust_doc_comments_are_protected_by_node_structure() {
        let comments = classify(
            "/// outer doc\nfn f() {}\n//// four slashes\nfn g() {}\n/** block doc */\nfn h() {}\n",
            LanguageId::Rust,
            "a.rs",
        );
        assert_eq!(class_of(&comments, "outer doc"), DocProtected);
        assert_eq!(class_of(&comments, "four slashes"), Prose);
        assert_eq!(class_of(&comments, "block doc"), DocProtected);
        let comments = classify("//! inner doc\nfn f() {}\n", LanguageId::Rust, "lib.rs");
        assert_eq!(class_of(&comments, "inner doc"), DocProtected);
    }

    #[test]
    fn java_docs_are_protected_and_deprecated_javadoc_is_functional() {
        let comments = classify(
            "class A {\n  /** Javadoc. */\n  void a() {}\n  /// Markdown doc.\n  void b() {}\n  \
             /** @deprecated use X */\n  void c() {}\n  // plain\n  void d() {}\n}\n",
            LanguageId::Java,
            "A.java",
        );
        assert_eq!(class_of(&comments, "Javadoc."), DocProtected);
        assert_eq!(class_of(&comments, "Markdown doc."), DocProtected);
        assert_eq!(class_of(&comments, "@deprecated"), Functional);
        assert!(has_tag(&comments, "@deprecated", "directive:deprecated"));
        assert_eq!(class_of(&comments, "plain"), Prose);
    }

    #[test]
    fn languages_without_doc_policies_treat_doc_shapes_as_prose() {
        let comments = classify(
            "/** doxygen */\nint f(void);\n/// tripled\nint g(void);\n",
            LanguageId::Cpp,
            "a.h",
        );
        assert_eq!(class_of(&comments, "doxygen"), Prose);
        assert_eq!(class_of(&comments, "tripled"), Prose);
        let comments = classify(
            "package p\n\n// F does things.\nfunc F() {}\n",
            LanguageId::Go,
            "a.go",
        );
        assert_eq!(class_of(&comments, "F does"), Prose);
    }

    #[test]
    fn jsx_container_sole_comments_stay_prose_with_their_tag() {
        let comments = classify(
            "const a = <div>{/* sole */}</div>;\n",
            LanguageId::Tsx,
            "a.tsx",
        );
        assert_eq!(class_of(&comments, "sole"), Prose);
        assert!(has_tag(&comments, "sole", "jsx-expression"));
    }

    // ---- matches_functional_pattern / declared_encoding. -------------------

    #[test]
    fn functional_pattern_check_is_position_free() {
        let js = adapter_for(LanguageId::Javascript);
        let ts = adapter_for(LanguageId::Typescript);
        // Modeline rows match regardless of placement windows.
        assert!(matches_functional_pattern("// vim: set ts=4:", js));
        assert!(matches_functional_pattern("/* -*- mode: js -*- */", js));
        assert!(matches_functional_pattern(
            "// SPDX-License-Identifier: MIT",
            js
        ));
        // Doc functional tags, in any slot kind.
        assert!(matches_functional_pattern("/** @type {X} */", js));
        assert!(matches_functional_pattern("// @type {X}", js));
        assert!(!matches_functional_pattern("/** @type {X} */", ts));
        assert!(matches_functional_pattern("/** @internal */", ts));
        assert!(!matches_functional_pattern(
            "// plain prose about types",
            js
        ));
        assert!(!matches_functional_pattern("// see the regex: below", js));
    }

    #[test]
    fn declared_encoding_reads_counting_cookies_only() {
        assert_eq!(
            declared_encoding("# -*- coding: latin-1 -*-\nx = 1\n").as_deref(),
            Some("latin-1"),
        );
        let utf8 = declared_encoding("# coding=utf-8\n").expect("a cookie");
        assert!(is_utf8_alias(&utf8));
        assert_eq!(declared_encoding("#\n#\n# coding: latin-1\n"), None);
        assert_eq!(
            declared_encoding("#!/usr/bin/env python\n# vim: set fileencoding=cp1252 :\n")
                .as_deref(),
            Some("cp1252"),
        );
        assert_eq!(declared_encoding("x = 1  # coding: latin-1\n"), None);
        assert_eq!(declared_encoding("x = 1\n# coding: latin-1\n"), None);
        assert_eq!(
            declared_encoding("\u{FEFF}# coding: utf-8\n").as_deref(),
            Some("utf-8"),
        );
        // `coding:` with no name does not stop the search.
        assert_eq!(
            declared_encoding("# coding: , coding=ascii\n").as_deref(),
            Some("ascii"),
        );
        assert_eq!(declared_encoding(""), None);
    }

    #[test]
    fn utf8_aliases_are_case_insensitive() {
        for alias in ["utf-8", "UTF-8", "utf8", "Utf_8", "utf-8-SIG"] {
            assert!(is_utf8_alias(alias), "{alias}");
        }
        for other in ["latin-1", "utf-16", "utf", "cp1252", "ascii"] {
            assert!(!is_utf8_alias(other), "{other}");
        }
    }

    // ---- Positional rules. --------------------------------------------------

    #[test]
    fn license_header_runs_skip_the_shebang_in_python_js_and_rust() {
        let comments = classify(
            "#!/usr/bin/env python3\n# Copyright 2026 Example Corp.\n# SPDX-License-Identifier: MIT\n\
             # All rights reserved, see the file for details.\n\nimport os\n# later prose\n",
            LanguageId::Python,
            "a.py",
        );
        for needle in ["Copyright", "SPDX", "All rights reserved"] {
            assert_eq!(class_of(&comments, needle), Functional, "{needle}");
            assert!(has_tag(&comments, needle, "license"), "{needle}");
        }
        assert_eq!(class_of(&comments, "later prose"), Prose);

        let comments = classify(
            "#!/usr/bin/env node\n/*\n * Licensed under the MIT license.\n */\n// continuation\nx();\n// prose\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "Licensed"), Functional);
        assert_eq!(class_of(&comments, "continuation"), Functional);
        assert_eq!(class_of(&comments, "// prose"), Prose);

        let comments = classify(
            "#!/usr/bin/env rust-script\n// Copyright (c) the authors\n// keyword-free line\nfn main() {}\n",
            LanguageId::Rust,
            "a.rs",
        );
        assert_eq!(class_of(&comments, "Copyright"), Functional);
        assert_eq!(class_of(&comments, "keyword-free"), Functional);
    }

    #[test]
    fn license_runs_need_a_keyword_and_no_preceding_code() {
        let comments = classify(
            "// just a header\n// more\nx();\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "just a header"), Prose);
        let comments = classify("x();\n// Copyright 2026\n", LanguageId::Javascript, "a.js");
        assert_eq!(class_of(&comments, "Copyright"), Prose);
        // Code interrupts the run: the comment after it is not a member.
        let comments = classify(
            "// Copyright 2026\nx();\n// after code\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "Copyright"), Functional);
        assert_eq!(class_of(&comments, "after code"), Prose);
    }

    #[test]
    fn spdx_lines_outside_the_header_run_are_license_functional() {
        let comments = classify(
            "package p\n\nfunc f() {\n\t// SPDX-License-Identifier: Apache-2.0\n}\n",
            LanguageId::Go,
            "a.go",
        );
        assert_eq!(class_of(&comments, "SPDX"), Functional);
        assert!(has_tag(&comments, "SPDX", "license"));
    }

    #[test]
    fn pep263_cookies_count_only_on_lines_one_and_two_alone_on_their_line() {
        let cookie = |text: &str| classify(text, LanguageId::Python, "a.py");

        let comments = cookie("# -*- coding: latin-1 -*-\nx = 1\n");
        assert_eq!(class_of(&comments, "coding"), Functional);
        assert!(has_tag(&comments, "coding", "encoding-cookie"));
        // Line 3: prose.
        let comments = cookie("# a\n# b\n# coding: latin-1\nx = 1\n");
        assert_eq!(class_of(&comments, "coding"), Prose);
        // Line 2 with code on line 1: prose.
        let comments = cookie("x = 1\n# coding: latin-1\n");
        assert_eq!(class_of(&comments, "coding"), Prose);
        // Line 2 after a shebang line 1: functional.
        let comments = cookie("#!/usr/bin/env python3\n# coding: latin-1\nx = 1\n");
        assert_eq!(class_of(&comments, "coding"), Functional);
        assert!(has_tag(&comments, "coding", "encoding-cookie"));
        // Line 2 after a blank line 1: functional.
        let comments = cookie("\n# coding=utf-8\nx = 1\n");
        assert_eq!(class_of(&comments, "coding"), Functional);
        // Trailing code on the cookie's line: prose.
        let comments = cookie("x = 1  # coding: latin-1\n");
        assert_eq!(class_of(&comments, "coding"), Prose);
    }

    #[test]
    fn cgo_preambles_are_the_doc_block_of_import_c() {
        let go = |text: &str| classify(text, LanguageId::Go, "a.go");

        let comments =
            go("package p\n\n// #include <stdio.h>\n// #include <stdlib.h>\nimport \"C\"\n");
        assert_eq!(class_of(&comments, "stdio"), Functional);
        assert_eq!(class_of(&comments, "stdlib"), Functional);
        assert!(has_tag(&comments, "stdio", "cgo-preamble"));

        let comments = go("package p\n\n/*\n#include <stdio.h>\n*/\nimport \"C\"\n");
        assert_eq!(class_of(&comments, "stdio"), Functional);

        // A blank line separates the block from `import "C"`: not its doc.
        let comments = go("package p\n\n// #include <x.h>\n\nimport \"C\"\n");
        assert_eq!(class_of(&comments, "x.h"), Prose);
        // A blank line inside the block ends the group at the blank.
        let comments = go("package p\n\n// unrelated\n\n// #include <x.h>\nimport \"C\"\n");
        assert_eq!(class_of(&comments, "unrelated"), Prose);
        assert_eq!(class_of(&comments, "x.h"), Functional);
        // Not adjacent to `import "C"` at all.
        let comments = go("package p\n\n// #include <x.h>\nimport \"fmt\"\n");
        assert_eq!(class_of(&comments, "x.h"), Prose);
    }

    #[test]
    fn cgo_preambles_inside_grouped_imports() {
        let go = |text: &str| classify(text, LanguageId::Go, "a.go");

        let comments = go(
            "package p\n\nimport (\n\t// fmt doc\n\t\"fmt\" // trailing\n\t// #include <x.h>\n\t\"C\"\n)\n",
        );
        assert_eq!(class_of(&comments, "x.h"), Functional);
        assert!(has_tag(&comments, "x.h", "cgo-preamble"));
        assert_eq!(class_of(&comments, "fmt doc"), Prose);
        assert_eq!(class_of(&comments, "trailing"), Prose);

        // A grouped declaration with a single "C" spec: its own doc comment
        // counts too.
        let comments = go("package p\n\n// #include <y.h>\nimport (\n\t\"C\"\n)\n");
        assert_eq!(class_of(&comments, "y.h"), Functional);
        // With several specs, the declaration's doc comment is not read.
        let comments = go("package p\n\n// #include <y.h>\nimport (\n\t\"C\"\n\t\"fmt\"\n)\n");
        assert_eq!(class_of(&comments, "y.h"), Prose);
    }

    #[test]
    fn source_map_comments_count_only_at_eof() {
        for (language, name) in [
            (LanguageId::Javascript, "a.js"),
            (LanguageId::Typescript, "a.ts"),
            (LanguageId::Tsx, "a.tsx"),
        ] {
            let comments = classify("x();\n//# sourceMappingURL=a.js.map\n", language, name);
            assert_eq!(class_of(&comments, "sourceMappingURL"), Functional);
            assert!(has_tag(&comments, "sourceMappingURL", "source-map"));
            let comments = classify("x();\n//@ sourceURL=a.js\n", language, name);
            assert_eq!(class_of(&comments, "sourceURL"), Functional);
            let comments = classify("//# sourceMappingURL=a.js.map\nx();\n", language, name);
            assert_eq!(class_of(&comments, "sourceMappingURL"), Prose);
            // Prose merely mentioning the name is not the magic comment.
            let comments = classify("x();\n// the sourceMappingURL= comment\n", language, name);
            assert_eq!(class_of(&comments, "sourceMappingURL"), Prose);
        }
        // Not a positional rule for other languages.
        let comments = classify(
            "fn f() {}\n//# sourceMappingURL=a.map\n",
            LanguageId::Rust,
            "a.rs",
        );
        assert_eq!(class_of(&comments, "sourceMappingURL"), Prose);
    }

    #[test]
    fn vim_modelines_count_in_the_first_and_last_five_lines() {
        let filler: String = (0..20).map(|index| format!("x{index}();\n")).collect();
        // Line 2.
        let text = format!("x();\n// vim: set ts=4 sw=4:\n{filler}");
        let comments = classify(&text, LanguageId::Javascript, "a.js");
        assert_eq!(class_of(&comments, "vim:"), Functional);
        assert!(has_tag(&comments, "vim:", "modeline"));
        // Within the last 5 lines.
        let text = format!("{filler}// vi: noet\nx();\n");
        let comments = classify(&text, LanguageId::Javascript, "a.js");
        assert_eq!(class_of(&comments, "vi:"), Functional);
        // In the middle: prose.
        let text = format!("{filler}// Vim: set ts=2:\n{filler}");
        let comments = classify(&text, LanguageId::Javascript, "a.js");
        assert_eq!(class_of(&comments, "Vim:"), Prose);
        // A block comment whose modeline line sits inside the window while
        // the comment starts outside it.
        let text = format!("{filler}/*\n * vim700: ts=8\n */\n");
        let comments = classify(&text, LanguageId::Javascript, "a.js");
        assert_eq!(class_of(&comments, "vim700:"), Functional);
        // Python and Go too (global rows).
        let comments = classify("x = 1\n# vim: ts=4\n", LanguageId::Python, "a.py");
        assert_eq!(class_of(&comments, "vim:"), Functional);
    }

    #[test]
    fn emacs_modelines_count_on_line_one_or_two_after_a_shebang() {
        let comments = classify(
            "// -*- mode: js -*-\nx();\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert!(has_tag(&comments, "-*-", "modeline"));
        let comments = classify(
            "x();\n\n// -*- mode: js -*-\ny();\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "-*-"), Prose);
        assert!(!has_tag(&comments, "-*-", "modeline"));
        let comments = classify(
            "x();\n// -*- mode: js -*-\n",
            LanguageId::Javascript,
            "a.js",
        );
        assert_eq!(class_of(&comments, "-*-"), Prose);
        let comments = classify(
            "#!/usr/bin/env python3\n# -*- mode: python -*-\nx = 1\n",
            LanguageId::Python,
            "a.py",
        );
        assert!(has_tag(&comments, "-*-", "modeline"));
    }

    #[test]
    fn go_example_output_blocks_are_functional_in_test_files_only() {
        let text = "package p\n\nfunc ExampleFoo() {\n\t// prints one\n\tfmt.Println(1)\n\t// Output:\n\t// 1\n}\n";
        let comments = classify(text, LanguageId::Go, "x_test.go");
        assert_eq!(class_of(&comments, "prints one"), Prose);
        assert_eq!(class_of(&comments, "Output:"), Functional);
        assert!(has_tag(&comments, "Output:", "example-output"));
        assert_eq!(class_of(&comments, "// 1"), Functional);
        assert!(has_tag(&comments, "// 1", "example-output"));

        // Same text in a non-test file: prose.
        let comments = classify(text, LanguageId::Go, "x.go");
        assert_eq!(class_of(&comments, "Output:"), Prose);
        assert_eq!(class_of(&comments, "// 1"), Prose);

        // A non-Example function (and a lowercase continuation): prose.
        for name in ["TestFoo", "Examplefoo", "helper"] {
            let text = format!(
                "package p\n\nfunc {name}() {{\n\tfmt.Println(1)\n\t// Output:\n\t// 1\n}}\n"
            );
            let comments = classify(&text, LanguageId::Go, "x_test.go");
            assert_eq!(class_of(&comments, "Output:"), Prose, "{name}");
        }

        // `Unordered output:` and the bare `Example` name.
        let text = "package p\n\nfunc Example() {\n\tfmt.Println(2)\n\t// Unordered output: 2\n}\n";
        let comments = classify(text, LanguageId::Go, "x_test.go");
        assert_eq!(class_of(&comments, "Unordered output"), Functional);

        // The output block ends with its function.
        let text =
            "package p\n\nfunc Example_a() {\n\t// Output: a\n}\n\n// after\nfunc helper() {}\n";
        let comments = classify(text, LanguageId::Go, "x_test.go");
        assert_eq!(class_of(&comments, "Output: a"), Functional);
        assert_eq!(class_of(&comments, "after"), Prose);
    }

    // ---- Negatives. ---------------------------------------------------------

    #[test]
    fn mentions_are_not_directives() {
        let comments = classify(
            "// see the regex: below\nx();\n// index: 3\ny();\n// Notes about the vim editor\nz();\n",
            LanguageId::Javascript,
            "a.js",
        );
        for needle in ["regex:", "index:", "Notes"] {
            assert_eq!(class_of(&comments, needle), Prose, "{needle}");
            assert!(tags(find(&comments, needle)).is_empty(), "{needle}");
        }
    }

    // ---- Tags and derivation. ----------------------------------------------

    #[test]
    fn tags_are_sorted_unique_and_cover_the_depended_upon_vocabulary() {
        assert!(
            TAGS.windows(2).all(|pair| pair[0] < pair[1]),
            "sorted, unique"
        );
        for tag in [
            "directive-adjacent",
            "jsx-expression",
            "license",
            "encoding-cookie",
            "cgo-preamble",
            "example-output",
            "modeline",
            "source-map",
            "pure-annotation",
            "jsx-pragma",
            "test-environment",
            "unicode-escape",
            "line-splice",
            "lone-cr",
            "flagged-for-review",
            "sanitize-failed",
            "offline",
            "verify-rail",
            "todo",
            "fixme",
            "hack",
            "warning",
            "xxx",
            "note",
        ] {
            assert!(TAGS.contains(&tag), "{tag}");
        }
    }

    #[test]
    fn every_fixed_and_doc_derived_tag_is_in_the_vocabulary() {
        let rows = ALL_ADAPTERS
            .iter()
            .flat_map(|adapter| adapter.directives)
            .chain(GLOBAL_DIRECTIVES);
        for row in rows {
            if let TagSpec::Fixed(tag) = row.tag {
                assert!(TAGS.contains(&tag), "row {:?}: {tag}", row.pattern);
            }
        }
        for adapter in ALL_ADAPTERS {
            for tag in adapter.doc.iter().flat_map(|doc| doc.functional_tags) {
                let derived = derive_directive_tag(tag).expect("doc tags have names");
                assert!(TAGS.contains(&derived.as_str()), "{tag} → {derived}");
            }
        }
    }

    #[test]
    fn literal_tag_derivation_follows_the_rule() {
        let derived = |pattern: &str| derive_directive_tag(pattern);
        assert_eq!(
            derived("@ts-expect-error").as_deref(),
            Some("directive:ts-expect-error")
        );
        assert_eq!(derived("//nolint").as_deref(), Some("directive:nolint"));
        assert_eq!(derived("#region").as_deref(), Some("directive:region"));
        assert_eq!(
            derived("$FlowFixMe").as_deref(),
            Some("directive:flow-fix-me")
        );
        assert_eq!(
            derived("XMLHttpRequest").as_deref(),
            Some("directive:xml-http-request")
        );
        assert_eq!(
            derived("NOLINTNEXTLINE").as_deref(),
            Some("directive:nolintnextline")
        );
        assert_eq!(
            derived("# pylint: disable-next=").as_deref(),
            Some("directive:pylint-disable-next"),
        );
        assert_eq!(
            derived("c8 ignore next").as_deref(),
            Some("directive:c8-ignore-next")
        );
        assert_eq!(derived("@param {").as_deref(), Some("directive:param"));
        assert_eq!(derived("//~"), None);
    }

    #[test]
    fn positional_rules_are_wired_per_language() {
        let rules = |id: LanguageId| adapter_for(id).positional_rules.to_vec();
        assert_eq!(
            rules(LanguageId::Python),
            [PositionalRuleId::PepEncodingCookie]
        );
        assert_eq!(
            rules(LanguageId::Go),
            [
                PositionalRuleId::CgoPreamble,
                PositionalRuleId::GoExampleOutput
            ],
        );
        for id in [
            LanguageId::Javascript,
            LanguageId::Typescript,
            LanguageId::Tsx,
        ] {
            assert_eq!(rules(id), [PositionalRuleId::SourceMapEof]);
        }
        for id in [
            LanguageId::Java,
            LanguageId::C,
            LanguageId::Cpp,
            LanguageId::Rust,
        ] {
            assert!(rules(id).is_empty());
        }
    }

    #[test]
    fn a_mismatched_adapter_is_an_invariant() {
        let adapter = adapter_for(LanguageId::Python);
        let error = classify_with(
            Vec::new(),
            ClassifyCtx {
                text: "",
                file_name: "a.js",
                language: LanguageId::Javascript,
                has_shebang: false,
            },
            adapter,
            compiled(adapter).expect("compiles"),
        )
        .expect_err("language and adapter disagree");
        assert_eq!(error.exit_code(), 7);
    }
}
