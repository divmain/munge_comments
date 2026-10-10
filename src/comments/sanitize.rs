//! Comment-text sanitization primitives: the single implementations of
//! [`comment_body`], [`match_key`] and [`normalize_comment_text`].
//!
//! Every consumer that needs delimiter-free comment text — directive
//! matching, heuristic similarity, the content-empty rule, `FakeLlm`
//! scenario keys, the ground-truth manifest, edit surgery — imports these;
//! no consumer strips delimiters its own way. `validate_final_comment`
//! (written-text validation) needs slot rendering and lands with the
//! sanitizer-validation ticket.

use crate::lang::adapter::{LanguageAdapter, compiled};

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
fn opener_len(line: &str, adapter: &LanguageAdapter) -> Option<usize> {
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
            // Passes through unchanged.
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
    // The opener's and closer's own lines, when they held nothing else.
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
