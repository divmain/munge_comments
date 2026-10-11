//! Comment-text sanitization: the single implementations of
//! [`comment_body`], [`match_key`], [`normalize_comment_text`],
//! [`is_content_empty`] and [`validate_final_comment`].
//!
//! Every consumer that needs delimiter-free comment text — directive
//! matching, heuristic similarity, the content-empty rule, `FakeLlm`
//! scenario keys, the ground-truth manifest, edit surgery — imports these;
//! no consumer strips delimiters its own way. [`validate_final_comment`]
//! is the single written-text validator, shared by the LLM layer, the
//! heuristics, `FakeLlm` and apply.

use unicode_segmentation::UnicodeSegmentation;

use super::classify::matches_functional_pattern;
use super::redact::has_placeholder_collision;
use crate::apply::render::{Slot, render_comment};
use crate::lang::adapter::{LanguageAdapter, SlotKind, compiled};

/// The characters other than `\n` that terminate a line somewhere a comment
/// may end up: a lone CR (every tier-1 compiler's line terminator), and the
/// Unicode line/paragraph separators (JavaScript line terminators).
const OTHER_LINE_TERMINATORS: [char; 3] = ['\r', '\u{2028}', '\u{2029}'];

/// A comment body together with the physical source line each body line
/// came from.
///
/// [`comment_body`] normalizes line terminators, so a lone `\r` (or U+2028 /
/// U+2029) inside a comment starts a new body line without starting a new
/// physical line (physical lines end at `\n`, as the scanner counts them).
/// Rules evaluated per source line — the modeline windows — map body
/// offsets back through [`CommentBody::raw_line_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentBody {
    /// The body text (exactly what [`comment_body`] returns).
    pub text: String,
    /// `(body offset where a body line starts, 0-based physical line of the
    /// raw comment it came from)`, ascending.
    line_starts: Vec<(usize, usize)>,
}

impl CommentBody {
    /// The 0-based physical line, relative to the raw comment's first line,
    /// holding the body byte at `offset`.
    pub fn raw_line_at(&self, offset: usize) -> usize {
        let index = self
            .line_starts
            .partition_point(|&(start, _)| start <= offset);
        index
            .checked_sub(1)
            .and_then(|index| self.line_starts.get(index))
            .map_or(0, |&(_, line)| line)
    }
}

/// How a raw comment is delimited, which decides how its lines are
/// stripped.
enum Shape<'a> {
    /// Starts with one of the adapter's line prefixes.
    Line,
    /// Starts with the block opener; the payload between the delimiters
    /// (the doc/bang marker included).
    Block(&'a str),
    /// No comment delimiters: a delimiter-free regeneration.
    Plain,
}

fn shape<'a>(raw: &'a str, adapter: &LanguageAdapter) -> Shape<'a> {
    if adapter
        .line_prefixes
        .iter()
        .any(|prefix| raw.starts_with(prefix))
    {
        return Shape::Line;
    }
    if let Some(delimiters) = adapter.block_delimiters
        && let Some(after_open) = raw.strip_prefix(delimiters.open)
    {
        let payload = after_open
            .strip_suffix(delimiters.close)
            .unwrap_or(after_open);
        return Shape::Block(payload);
    }
    Shape::Plain
}

/// The length of the line opener at the start of `line` (`//`, `///`,
/// `//!`, `#`, `#:` …), per the adapter's `line_opener`.
///
/// The compiled opener comes from the per-adapter pattern cache; its
/// compilation cannot fail for the pinned openers (a unit test compiles
/// every adapter). Should it ever fail, the bare line prefix is used, which
/// differs only for the doc/bang opener variants.
pub(crate) fn opener_len(line: &str, adapter: &LanguageAdapter) -> Option<usize> {
    match compiled(adapter) {
        Ok(patterns) => patterns
            .line_opener
            .find(line)
            .filter(|matched| matched.start() == 0)
            .map(|matched| matched.end()),
        Err(_) => adapter
            .line_prefixes
            .iter()
            .find(|prefix| line.starts_with(**prefix))
            .map(|prefix| prefix.len()),
    }
}

/// Strips one leading space or tab.
fn strip_one_blank(text: &str) -> &str {
    text.strip_prefix([' ', '\t']).unwrap_or(text)
}

/// Strips leading whitespace, one leading `*`, and one following blank —
/// block-comment continuation decoration.
fn strip_decoration(line: &str) -> &str {
    let trimmed = line.trim_start();
    match trimmed.strip_prefix('*') {
        Some(rest) => strip_one_blank(rest),
        None => trimmed,
    }
}

/// The prose body of a raw comment, with the physical line of every body
/// line (see [`CommentBody`]).
pub fn comment_body_with_lines(raw: &str, adapter: &LanguageAdapter) -> CommentBody {
    let shape = shape(raw, adapter);
    let source = match shape {
        Shape::Line => raw,
        Shape::Block(payload) => payload,
        Shape::Plain => {
            // Delimiter-free text passes through verbatim, so its line map is just the `\n` positions.
            let mut line_starts = vec![(0, 0)];
            line_starts.extend(
                raw.match_indices('\n')
                    .enumerate()
                    .map(|(line, (at, _))| (at + 1, line + 1)),
            );
            return CommentBody {
                text: raw.to_string(),
                line_starts,
            };
        }
    };

    let mut text = String::new();
    let mut line_starts = Vec::new();
    let mut first = true;
    for (physical, line) in source.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for segment in line.split(OTHER_LINE_TERMINATORS) {
            let stripped = match shape {
                Shape::Line => {
                    let trimmed = segment.trim_start();
                    match opener_len(trimmed, adapter) {
                        Some(length) => strip_one_blank(&trimmed[length..]),
                        // A continuation after a lone CR: no opener.
                        None => trimmed,
                    }
                }
                Shape::Block(_) if first => {
                    // The doc (`/**`) or bang (`/*!`) marker belongs to the
                    // opener.
                    segment
                        .strip_prefix(['*', '!'])
                        .unwrap_or(segment)
                        .trim_start()
                }
                Shape::Block(_) => strip_decoration(segment),
                Shape::Plain => segment,
            };
            if !first {
                text.push('\n');
            }
            first = false;
            line_starts.push((text.len(), physical));
            text.push_str(stripped.trim_end());
        }
    }
    CommentBody { text, line_starts }
}

/// The prose body of a raw comment: the opener/closer stripped from every
/// line, leading `*` decoration stripped from block continuation lines,
/// line terminators (`\r\n`, `\r`, U+2028, U+2029) normalized to `\n`, and
/// trailing whitespace trimmed per line. Line structure is preserved. A
/// delimiter-free regeneration passes through unchanged.
pub fn comment_body(raw: &str, adapter: &LanguageAdapter) -> String {
    comment_body_with_lines(raw, adapter).text
}

/// The comparison key of a raw comment: its body, whitespace-collapsed and
/// trimmed. Similarity, the content-empty rule, `FakeLlm` scenarios and the
/// ground-truth manifest all use this.
pub fn match_key(raw: &str, adapter: &LanguageAdapter) -> String {
    comment_body(raw, adapter)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The content-empty rule (decision-matrix row 3, and the sanitizer's
/// first content check): the body of `raw` holds zero word tokens —
/// `unicode_words()` segments, UAX #29 word boundaries. `//`, `/* */`,
/// `# -----` are content-empty.
pub fn is_content_empty(raw: &str, adapter: &LanguageAdapter) -> bool {
    comment_body(raw, adapter).unicode_words().next().is_none()
}

/// Why [`validate_final_comment`] refused text that would be written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidFinalComment {
    /// A character no slot may hold, in any language: a C0 control other
    /// than tab and line feed, U+007F, U+FEFF, or a bidi control.
    #[error("contains the forbidden character U+{:04X}", u32::from(*.0))]
    ForbiddenCharacter(char),
    /// Redaction placeholder text (`⟦C<digits>⟧`) leaked into the text.
    #[error("contains redaction placeholder text")]
    PlaceholderLeak,
    /// Zero word tokens: it would render as a bare opener.
    #[error("has no word content")]
    ContentEmpty,
    /// The language has no comment of the slot's kind.
    #[error("the language has no {0:?} comments")]
    UnsupportedSlot(SlotKind),
    /// The block closer, which cannot be escaped, bound for a block/doc
    /// slot.
    #[error("contains the block-comment closer {0:?}")]
    BlockClose(&'static str),
    /// An adapter `slot_rejects` pattern matched.
    #[error("{0}")]
    SlotReject(&'static str),
    /// The rendered text would itself classify as functional.
    #[error("would itself be a functional comment")]
    Functional,
    /// The adapter's patterns failed to compile (never for the pinned
    /// tables); validation fails closed.
    #[error("the language's patterns are unavailable")]
    PatternsUnavailable,
}

/// Whether no comment slot may hold `c` (see
/// [`InvalidFinalComment::ForbiddenCharacter`]): NUL is a compile error in
/// Go and CPython, a non-leading BOM is rejected by both, rustc denies
/// bidi controls in comments, and the verifier sees none of it.
fn is_forbidden(c: char) -> bool {
    (c < '\u{20}' && c != '\t' && c != '\n')
        || c == '\u{7F}'
        || c == '\u{FEFF}'
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// Validates text that would be *written* into a slot of `slot_kind` (a
/// judge `finalComment` for `KEEP_GENERATED`/`HYBRID`, a NEW record's
/// text), after [`normalize_comment_text`]. Never edits the text; rejects,
/// in order:
///
/// 1. forbidden characters (see [`InvalidFinalComment::ForbiddenCharacter`]);
/// 2. placeholder leakage (`⟦C<digits>⟧`);
/// 3. content-empty text ([`is_content_empty`]);
/// 4. a block/doc slot in a language without block comments, or the
///    block closer in a block/doc slot;
/// 5. any adapter `slot_rejects` pattern for `slot_kind`;
/// 6. text that would itself classify as functional:
///    [`matches_functional_pattern`] on the text rendered into the
///    language's default slot of `slot_kind` (`//`/`#`, `/*`, `/**`) —
///    both multi-line and collapsed to one line, as an inline or trailing
///    slot renders it — so the result never depends on a particular
///    slot's captured opener or layout. Doc `functional_tags` are checked
///    in every slot kind.
pub fn validate_final_comment(
    text: &str,
    slot_kind: SlotKind,
    adapter: &LanguageAdapter,
) -> Result<(), InvalidFinalComment> {
    if let Some(c) = text.chars().find(|&c| is_forbidden(c)) {
        return Err(InvalidFinalComment::ForbiddenCharacter(c));
    }
    if has_placeholder_collision(text) {
        return Err(InvalidFinalComment::PlaceholderLeak);
    }
    if is_content_empty(text, adapter) {
        return Err(InvalidFinalComment::ContentEmpty);
    }
    let slot = Slot::canonical(slot_kind, adapter)
        .ok_or(InvalidFinalComment::UnsupportedSlot(slot_kind))?;
    if slot_kind != SlotKind::Line && text.contains(slot.block_close) {
        return Err(InvalidFinalComment::BlockClose(slot.block_close));
    }
    let patterns = compiled(adapter).map_err(|_| InvalidFinalComment::PatternsUnavailable)?;
    if let Some(reject) = patterns
        .slot_rejects
        .iter()
        .find(|reject| reject.kind == slot_kind && reject.regex.is_match(text))
    {
        return Err(InvalidFinalComment::SlotReject(reject.reason));
    }
    let collapsed = Slot {
        trailing: true,
        ..slot.clone()
    };
    if matches_functional_pattern(&render_comment(text, &slot), adapter)
        || matches_functional_pattern(&render_comment(text, &collapsed), adapter)
    {
        return Err(InvalidFinalComment::Functional);
    }
    Ok(())
}

/// Benign normalization of (model-returned or hand-edited) comment text;
/// never eats content:
///
/// - all line terminators (`\r\n`, `\r`, U+2028, U+2029) become `\n`;
/// - a line opener (`//`, `///`, `#`, `#:` …, per the adapter's
///   `line_opener`) plus one optional blank is removed only when every
///   non-blank line starts with one *as a delimiter* — followed by a blank
///   or nothing — so `#1234: x` survives intact;
/// - a block opener/closer pair (with a doc/bang marker standing alone) is
///   removed only when the text both starts with the opener and ends with
///   the closer; lines left blank by that removal are dropped;
/// - `*` continuation decoration is removed only when every non-blank
///   continuation line carries it *as decoration* — followed by a blank or
///   nothing — so `*args are forwarded` survives intact.
pub fn normalize_comment_text(text: &str, adapter: &LanguageAdapter) -> String {
    let mut normalized = text.replace("\r\n", "\n");
    for terminator in OTHER_LINE_TERMINATORS {
        normalized = normalized.replace(terminator, "\n");
    }
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    strip_wrapped_line_openers(&mut lines, adapter);
    let opener_line_dropped = strip_wrapped_block_delimiters(&mut lines, adapter);
    // Once the opener's own line is gone, every remaining line is a
    // continuation line.
    let continuation_from = usize::from(!opener_line_dropped);
    strip_wrapped_decoration(&mut lines, continuation_from);
    lines.join("\n")
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// Whether `rest` (the text after a delimiter) shows the delimiter stands
/// alone: followed by a blank or nothing.
fn delimiter_stands_alone(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with([' ', '\t'])
}

fn strip_wrapped_line_openers(lines: &mut [String], adapter: &LanguageAdapter) {
    let openers: Vec<Option<usize>> = lines
        .iter()
        .map(|line| {
            opener_len(line, adapter).filter(|&length| delimiter_stands_alone(&line[length..]))
        })
        .collect();
    let wrapped = lines
        .iter()
        .zip(&openers)
        .all(|(line, opener)| is_blank(line) || opener.is_some())
        && openers.iter().any(Option::is_some);
    if !wrapped {
        return;
    }
    for (line, opener) in lines.iter_mut().zip(openers) {
        if let Some(length) = opener {
            *line = strip_one_blank(&line[length..]).to_string();
        }
    }
}

/// Strips a wrapping block opener/closer pair; returns whether the
/// opener's own line was dropped (it held nothing but the opener).
fn strip_wrapped_block_delimiters(lines: &mut Vec<String>, adapter: &LanguageAdapter) -> bool {
    let Some(delimiters) = adapter.block_delimiters else {
        return false;
    };
    let joined = lines.join("\n");
    if joined.len() < delimiters.open.len() + delimiters.close.len()
        || !joined.starts_with(delimiters.open)
        || !joined.ends_with(delimiters.close)
    {
        return false;
    }
    let mut inner = &joined[delimiters.open.len()..joined.len() - delimiters.close.len()];
    // A doc (`/**`) or bang (`/*!`) marker is part of the opener when it
    // stands alone; `/**args*/`-style glued text keeps its character.
    if let Some(rest) = inner.strip_prefix(['*', '!'])
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        inner = rest;
    }
    let inner = inner.strip_prefix([' ', '\t']).unwrap_or(inner);
    let inner = inner.strip_suffix([' ', '\t']).unwrap_or(inner);
    let mut stripped: Vec<String> = inner.split('\n').map(str::to_string).collect();
    // The opener's and closer's own lines, when they held nothing else; reported back so the caller knows whether the first line still holds opener text.
    let opener_line_dropped =
        stripped.len() > 1 && stripped.first().is_some_and(|line| is_blank(line));
    if opener_line_dropped {
        stripped.remove(0);
    }
    if stripped.len() > 1 && stripped.last().is_some_and(|line| is_blank(line)) {
        stripped.pop();
    }
    *lines = stripped;
    opener_line_dropped
}

/// Strips `*` decoration from `lines[continuation_from..]` when every
/// non-blank one of them carries it as decoration.
fn strip_wrapped_decoration(lines: &mut [String], continuation_from: usize) {
    let Some(continuation) = lines.get_mut(continuation_from..) else {
        return;
    };
    let decorated = |line: &String| {
        line.trim_start()
            .strip_prefix('*')
            .is_some_and(delimiter_stands_alone)
    };
    let wrapped = continuation
        .iter()
        .all(|line| is_blank(line) || decorated(line))
        && continuation.iter().any(decorated);
    if !wrapped {
        return;
    }
    for line in continuation {
        *line = strip_decoration(line).to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::adapters::adapter_for;
    use crate::lang::ids::LanguageId;

    fn js() -> &'static LanguageAdapter {
        adapter_for(LanguageId::Javascript)
    }
    fn py() -> &'static LanguageAdapter {
        adapter_for(LanguageId::Python)
    }
    fn rust() -> &'static LanguageAdapter {
        adapter_for(LanguageId::Rust)
    }

    #[test]
    fn comment_body_strips_line_openers_per_language() {
        assert_eq!(comment_body("// hello", js()), "hello");
        assert_eq!(comment_body("//hello", js()), "hello");
        assert_eq!(comment_body("/// doc note", js()), "doc note");
        assert_eq!(comment_body("//! inner", rust()), "inner");
        assert_eq!(comment_body("//// four", rust()), "four");
        assert_eq!(comment_body("# py comment", py()), "py comment");
        assert_eq!(comment_body("#: sphinx", py()), "sphinx");
        assert_eq!(
            comment_body("//go:build linux", adapter_for(LanguageId::Go)),
            "go:build linux"
        );
        // Only one blank after the opener is delimiter; the rest is content.
        assert_eq!(comment_body("//    indented", js()), "   indented");
        // A bare opener leaves an empty body.
        assert_eq!(comment_body("//", js()), "");
        assert_eq!(comment_body("#", py()), "");
    }

    #[test]
    fn comment_body_strips_every_line_of_a_grouped_run() {
        // A grouped record's text spans from the first opener to the last
        // comment's end, so continuation lines carry the shared indent.
        assert_eq!(comment_body("// a\n    // b\n    //", js()), "a\nb\n");
        assert_eq!(comment_body("# a\r\n# b", py()), "a\nb");
    }

    #[test]
    fn comment_body_strips_block_delimiters_and_decoration() {
        assert_eq!(comment_body("/* note */", js()), "note");
        assert_eq!(comment_body("/** doc */", js()), "doc");
        assert_eq!(comment_body("/*! legal */", js()), "legal");
        assert_eq!(comment_body("/**/", js()), "");
        assert_eq!(comment_body("/* */", js()), "");
        assert_eq!(
            comment_body("/**\n * first\n * second\n */", js()),
            "\nfirst\nsecond\n",
        );
        // Interior `*` is content, and so is a first-line `*` after a plain
        // opener's blank.
        assert_eq!(comment_body("/* a * b */", js()), "a * b");
        assert_eq!(comment_body("/* *args */", js()), "*args");
        // Rust's nested block comments keep their inner delimiters.
        assert_eq!(
            comment_body("/* outer /* inner */ still */", rust()),
            "outer /* inner */ still",
        );
        // Python has no block comments: such text is not delimited.
        assert_eq!(comment_body("/* x */", py()), "/* x */");
    }

    #[test]
    fn comment_body_normalizes_terminators_and_trims_trailing_blanks() {
        // A lone CR becomes a line break; the continuation has no opener.
        assert_eq!(comment_body("# a\rx = 1", py()), "a\nx = 1");
        assert_eq!(comment_body("// a\u{2028}b", js()), "a\nb");
        assert_eq!(comment_body("// trailing   ", js()), "trailing");
        assert_eq!(comment_body("/* x \r\n * y  \r\n */", js()), "x\ny\n");
    }

    #[test]
    fn body_lines_map_back_to_physical_lines() {
        let body = comment_body_with_lines("/* a\n * b\r * c\n * d */", js());
        assert_eq!(body.text, "a\nb\nc\nd");
        let line_of = |needle: &str| body.raw_line_at(body.text.find(needle).expect("present"));
        assert_eq!(line_of("a"), 0);
        assert_eq!(line_of("b"), 1);
        // The lone CR split a body line, not a physical line.
        assert_eq!(line_of("c"), 1);
        assert_eq!(line_of("d"), 2);

        let plain = comment_body_with_lines("x\ny", js());
        assert_eq!(plain.text, "x\ny");
        assert_eq!(plain.raw_line_at(2), 1);
    }

    #[test]
    fn a_delimiter_free_regeneration_passes_through_unchanged() {
        assert_eq!(comment_body("just prose", js()), "just prose");
        assert_eq!(comment_body("two\nlines  ", py()), "two\nlines  ");
    }

    #[test]
    fn match_key_collapses_whitespace() {
        assert_eq!(match_key("//  spaced   out ", js()), "spaced out");
        assert_eq!(match_key("/* one\n * two\n */", js()), "one two");
        assert_eq!(match_key("# x", py()), "x");
        assert_eq!(match_key("/* */", js()), "");
    }

    #[test]
    fn normalization_normalizes_line_terminators() {
        assert_eq!(
            normalize_comment_text("a\r\nb\rc\nd\u{2028}e\u{2029}f", js()),
            "a\nb\nc\nd\ne\nf",
        );
    }

    #[test]
    fn normalization_strips_wrapped_line_openers_only_when_unambiguous() {
        assert_eq!(
            normalize_comment_text("// first\n// second", js()),
            "first\nsecond"
        );
        assert_eq!(normalize_comment_text("/// doc line", js()), "doc line");
        assert_eq!(normalize_comment_text("// a\n\n// b", js()), "a\n\nb");
        // Only some lines carry it: kept.
        assert_eq!(
            normalize_comment_text("first\n// second", js()),
            "first\n// second"
        );
        // `#` glued to content is content: `#1234: x` survives intact.
        assert_eq!(
            normalize_comment_text("#1234: workaround", py()),
            "#1234: workaround"
        );
        assert_eq!(normalize_comment_text("#coding note", py()), "#coding note");
        assert_eq!(
            normalize_comment_text("//nolint please", js()),
            "//nolint please"
        );
        // A bare opener line among prefixed lines: stripped.
        assert_eq!(normalize_comment_text("#\n# note", py()), "\nnote");
        assert_eq!(normalize_comment_text("#: sphinx", py()), "sphinx");
    }

    #[test]
    fn normalization_strips_block_delimiters_only_as_a_pair() {
        assert_eq!(normalize_comment_text("/* some note */", js()), "some note");
        assert_eq!(normalize_comment_text("/** doc note */", js()), "doc note");
        assert_eq!(
            normalize_comment_text("/**\n * first\n * second\n */", js()),
            "first\nsecond",
        );
        // Opener without closer, or closer without opener: kept.
        assert_eq!(normalize_comment_text("/* some note", js()), "/* some note");
        assert_eq!(normalize_comment_text("some note */", js()), "some note */");
        // Overlapping delimiters are not a pair.
        assert_eq!(normalize_comment_text("/*/", js()), "/*/");
        // A glued `*` after the opener is content.
        assert_eq!(normalize_comment_text("/**args*/", js()), "*args");
        // Python has no block delimiters: never stripped.
        assert_eq!(normalize_comment_text("/* x */", py()), "/* x */");
    }

    #[test]
    fn normalization_strips_decoration_only_when_unambiguous() {
        assert_eq!(
            normalize_comment_text("first\n * second\n * third", js()),
            "first\nsecond\nthird",
        );
        // `*` glued to content is content: `*args are forwarded` survives.
        assert_eq!(
            normalize_comment_text("forward them:\n*args are forwarded", js()),
            "forward them:\n*args are forwarded",
        );
        assert_eq!(
            normalize_comment_text("*args are forwarded", js()),
            "*args are forwarded"
        );
        // Only some continuation lines decorated: kept.
        assert_eq!(normalize_comment_text("a\n * b\nc", js()), "a\n * b\nc");
    }

    #[test]
    fn normalization_is_idempotent_on_plain_prose() {
        for text in ["plain prose", "two\nlines", "#1234: x", "*args", "a * b"] {
            for id in LanguageId::ALL {
                let adapter = adapter_for(id);
                let once = normalize_comment_text(text, adapter);
                assert_eq!(once, text, "{}: {text:?}", id.as_str());
            }
        }
    }

    const SLOTS: [SlotKind; 3] = [SlotKind::Line, SlotKind::Block, SlotKind::Doc];

    fn adapter(id: LanguageId) -> &'static LanguageAdapter {
        adapter_for(id)
    }

    /// Normalizes, then validates — the order every caller uses.
    fn check(text: &str, slot: SlotKind, id: LanguageId) -> Result<(), InvalidFinalComment> {
        validate_final_comment(
            &normalize_comment_text(text, adapter(id)),
            slot,
            adapter(id),
        )
    }

    /// The slot kinds `id` has.
    fn slots_of(id: LanguageId) -> &'static [SlotKind] {
        if adapter(id).block_delimiters.is_some() {
            &SLOTS
        } else {
            &SLOTS[..1]
        }
    }

    #[test]
    fn content_empty_text_is_rejected() {
        for id in LanguageId::ALL {
            for &slot in slots_of(id) {
                for text in ["", "   ", " \n\t\n", "-----", "*"] {
                    assert_eq!(
                        check(text, slot, id),
                        Err(InvalidFinalComment::ContentEmpty),
                        "{} {slot:?} {text:?}",
                        id.as_str()
                    );
                }
                assert_eq!(check("a", slot, id), Ok(()), "{} {slot:?}", id.as_str());
                assert_eq!(check("42", slot, id), Ok(()), "{} {slot:?}", id.as_str());
            }
        }
        assert!(is_content_empty("//", js()));
        assert!(is_content_empty("/* */", js()));
        assert!(is_content_empty("# -----", py()));
        assert!(!is_content_empty("# x", py()));
        assert!(!is_content_empty("// 数", js()));
    }

    #[test]
    fn forbidden_characters_are_rejected_in_every_slot() {
        for id in LanguageId::ALL {
            for &slot in slots_of(id) {
                for c in [
                    '\0', '\u{1}', '\u{B}', '\u{C}', '\r', '\u{1B}', '\u{7F}', '\u{FEFF}',
                    '\u{202A}', '\u{202E}', '\u{2066}', '\u{2069}',
                ] {
                    let text = format!("note {c} here");
                    assert_eq!(
                        validate_final_comment(&text, slot, adapter(id)),
                        Err(InvalidFinalComment::ForbiddenCharacter(c)),
                        "{} {slot:?} U+{:04X}",
                        id.as_str(),
                        u32::from(c)
                    );
                }
                // Tab, line feed and ordinary non-ASCII text are fine.
                assert_eq!(
                    validate_final_comment("a\tb\nc é → 数", slot, adapter(id)),
                    Ok(()),
                    "{} {slot:?}",
                    id.as_str()
                );
            }
        }
    }

    #[test]
    fn placeholder_leakage_is_rejected() {
        for id in LanguageId::ALL {
            for &slot in slots_of(id) {
                assert_eq!(
                    check("see ⟦C12⟧ above", slot, id),
                    Err(InvalidFinalComment::PlaceholderLeak),
                );
                assert_eq!(check("brackets ⟦like this⟧ and ⟦C⟧", slot, id), Ok(()));
            }
        }
    }

    #[test]
    fn the_block_closer_is_rejected_in_block_and_doc_slots_only() {
        for id in LanguageId::ALL {
            if adapter(id).block_delimiters.is_none() {
                continue;
            }
            for slot in [SlotKind::Block, SlotKind::Doc] {
                assert_eq!(
                    check("matches src/**/*.ts files", slot, id),
                    Err(InvalidFinalComment::BlockClose("*/")),
                    "{} {slot:?}",
                    id.as_str()
                );
                assert_eq!(
                    check("ends early */ x", slot, id),
                    Err(InvalidFinalComment::BlockClose("*/"))
                );
            }
            assert_eq!(
                check("matches src/**/*.ts files", SlotKind::Line, id),
                Ok(()),
                "{}",
                id.as_str()
            );
        }
    }

    #[test]
    fn a_language_without_block_comments_has_no_block_slots() {
        for slot in [SlotKind::Block, SlotKind::Doc] {
            assert_eq!(
                check("prose", slot, LanguageId::Python),
                Err(InvalidFinalComment::UnsupportedSlot(slot)),
            );
        }
        assert_eq!(check("prose", SlotKind::Line, LanguageId::Python), Ok(()));
    }

    #[test]
    fn c_line_slots_reject_line_splices() {
        for id in [LanguageId::C, LanguageId::Cpp] {
            for text in [
                "continues \\",
                "continues \\  ",
                "continues \\\t",
                "first \\\nsecond",
                "trigraph ??/",
                "trigraph ??/ ",
            ] {
                assert!(
                    matches!(
                        check(text, SlotKind::Line, id),
                        Err(InvalidFinalComment::SlotReject(_))
                    ),
                    "{} {text:?}",
                    id.as_str()
                );
            }
            // A backslash elsewhere, or in a block slot, splices nothing.
            assert_eq!(check("a \\ b", SlotKind::Line, id), Ok(()));
            assert_eq!(check("C:\\dir\\file.txt", SlotKind::Line, id), Ok(()));
            assert_eq!(check("continues \\", SlotKind::Block, id), Ok(()));
        }
        // Other languages have no line-splice rule.
        assert_eq!(
            check("continues \\", SlotKind::Line, LanguageId::Go),
            Ok(())
        );
    }

    #[test]
    fn nested_block_openers_are_rejected_in_c_and_rust_block_slots() {
        for id in [LanguageId::C, LanguageId::Cpp, LanguageId::Rust] {
            for slot in [SlotKind::Block, SlotKind::Doc] {
                assert!(
                    matches!(
                        check("see /* this", slot, id),
                        Err(InvalidFinalComment::SlotReject(_))
                    ),
                    "{} {slot:?}",
                    id.as_str()
                );
            }
            assert_eq!(check("see /* this", SlotKind::Line, id), Ok(()));
        }
        // Block-open text is harmless in JS/TS/Java/Go block slots.
        for id in [
            LanguageId::Javascript,
            LanguageId::Typescript,
            LanguageId::Tsx,
            LanguageId::Java,
            LanguageId::Go,
        ] {
            for slot in [SlotKind::Block, SlotKind::Doc] {
                assert_eq!(
                    check("see /* this", slot, id),
                    Ok(()),
                    "{} {slot:?}",
                    id.as_str()
                );
            }
        }
    }

    #[test]
    fn java_unicode_escapes_are_rejected_in_every_slot() {
        for slot in SLOTS {
            for text in [
                "ends here \\u000a code();",
                "closes \\u002a/",
                "many \\uuuu000A",
            ] {
                assert!(
                    matches!(
                        check(text, slot, LanguageId::Java),
                        Err(InvalidFinalComment::SlotReject(_))
                    ),
                    "{slot:?} {text:?}"
                );
            }
            assert_eq!(
                check("a \\n escape and \\u00", slot, LanguageId::Java),
                Ok(())
            );
            // Only javac decodes them.
            assert_eq!(
                check("ends here \\u000a", slot, LanguageId::Javascript),
                Ok(())
            );
        }
    }

    #[test]
    fn text_that_would_be_functional_is_rejected() {
        use LanguageId::*;
        let functional = [
            (Typescript, SlotKind::Line, "// @ts-ignore the types lie"),
            (Typescript, SlotKind::Line, "@ts-expect-error"),
            (Javascript, SlotKind::Doc, "The name.\n@type {string}"),
            (Javascript, SlotKind::Block, "@__PURE__"),
            (
                Javascript,
                SlotKind::Line,
                "eslint-disable-next-line no-console",
            ),
            (Typescript, SlotKind::Line, "Hidden.\n@internal"),
            (Python, SlotKind::Line, "type: ignore"),
            (Python, SlotKind::Line, "noqa: E501"),
            (Go, SlotKind::Line, "//go:noinline"),
            (Go, SlotKind::Line, "Deprecated: use g."),
            (Cpp, SlotKind::Line, "NOLINT"),
            (Rust, SlotKind::Line, "SAFETY: the pointer is valid"),
        ];
        for (id, slot, text) in functional {
            assert_eq!(
                check(text, slot, id),
                Err(InvalidFinalComment::Functional),
                "{} {slot:?} {text:?}",
                id.as_str()
            );
        }
        // Doc functional tags are rejected in every slot kind.
        for slot in SLOTS {
            assert_eq!(
                check("Old API.\n@deprecated use g()", slot, Java),
                Err(InvalidFinalComment::Functional),
                "{slot:?}"
            );
        }
        // Acceptance counterparts: the same subjects as plain prose.
        let prose = [
            (Typescript, SlotKind::Line, "the types are wrong here"),
            (Javascript, SlotKind::Doc, "The display name, a string."),
            (Javascript, SlotKind::Block, "pure function"),
            (Python, SlotKind::Line, "the type is ignored by callers"),
            (Go, SlotKind::Line, "go:noinline is not needed here"),
            (Go, SlotKind::Line, "This is deprecated upstream."),
            (Rust, SlotKind::Line, "the pointer is valid"),
            (Java, SlotKind::Doc, "Old API; prefer g()."),
        ];
        for (id, slot, text) in prose {
            assert_eq!(
                check(text, slot, id),
                Ok(()),
                "{} {slot:?} {text:?}",
                id.as_str()
            );
        }
    }

    #[test]
    fn the_functional_check_also_sees_the_collapsed_one_line_rendering() {
        // Multi-line, Go's whole-comment generated marker cannot match; an
        // inline or trailing slot would collapse it into one that does.
        assert_eq!(
            check(
                "Code generated by stringer\nDO NOT EDIT.",
                SlotKind::Line,
                LanguageId::Go
            ),
            Err(InvalidFinalComment::Functional),
        );
        assert_eq!(
            check(
                "Code generated by stringer.\nEdit freely.",
                SlotKind::Line,
                LanguageId::Go
            ),
            Ok(()),
        );
    }

    #[test]
    fn a_hand_edited_line_separator_never_terminates_a_line_slot() {
        let js = js();
        let rendered = render_comment(
            &normalize_comment_text("first\u{2028}x = 1;\u{2029}third", js),
            &Slot::canonical(SlotKind::Line, js).expect("line slot"),
        );
        assert_eq!(rendered, "// first\n// x = 1;\n// third");
        assert!(!rendered.contains(['\u{2028}', '\u{2029}', '\r']));
        assert_eq!(
            check(
                "first\u{2028}x = 1;",
                SlotKind::Line,
                LanguageId::Javascript
            ),
            Ok(())
        );
    }

    #[test]
    fn every_adapters_comment_body_handles_its_comment_shapes() {
        for id in LanguageId::ALL {
            let adapter = adapter_for(id);
            let raw = if id == LanguageId::Python {
                "# note"
            } else {
                "// note"
            };
            assert_eq!(comment_body(raw, adapter), "note", "{}", id.as_str());
            if adapter.block_delimiters.is_some() {
                assert_eq!(
                    comment_body("/* note */", adapter),
                    "note",
                    "{}",
                    id.as_str()
                );
            }
        }
    }
}
