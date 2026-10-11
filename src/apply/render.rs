//! Replacement rendering: `KEEP_GENERATED` / `HYBRID` / `NEW` text rendered
//! into the target slot's style.
//!
//! [`render_comment`] is pure: everything it needs about the target lives
//! in a [`Slot`]. [`slot_for`] derives a slot from the source text, the
//! record and the adapter; [`slot_from_record`] derives one from the record
//! alone (for statistics computed without the source file).
//!
//! - **Openers** are reused verbatim: a line slot keeps its `line_opener`
//!   match (`//`, `///`, `//!`, `////`, `#`, `#:`), a block slot keeps
//!   `/*!`, `/**` (only when doc-like) or `/*`. NEW records use the
//!   language's plain line prefix, or the block opener when the file's
//!   prevailing NEW style is block.
//! - **Line terminator**: the one ending the slot's own line; else the
//!   file's first line terminator; else `\n`. Lines are `\n`-terminated (a
//!   preceding `\r` belongs to the terminator, so CRLF stays CRLF).
//! - **Layouts**: line slots render `{opener} {line}` per line at the
//!   slot's indent (an empty line renders as the bare opener); block/doc
//!   slots render `{opener} {text} {close}` for one line, else an opener
//!   line, ` * ` decorated body lines and a `*/` line. Trailing and inline
//!   slots receive single-line text (line breaks become single spaces); an
//!   inline block slot whose original span held a line break is followed
//!   by one EOL, so the gap keeps its line break.

use crate::artifact::types::CommentRecord;
use crate::comments::hygiene::is_line_terminator;
use crate::comments::sanitize::opener_len;
use crate::comments::scan::line_start_of;
use crate::comments::types::{CommentKind, Position};
use crate::errors::Error;
use crate::lang::adapter::{LanguageAdapter, SlotKind};

/// Everything [`render_comment`] needs to know about the slot a comment is
/// written into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The slot's lexical kind. Decided by the comment's *shape*: a
    /// line-shaped doc comment (Rust/Java `///`) is a [`SlotKind::Line`]
    /// slot, a block-shaped doc comment a [`SlotKind::Doc`] slot.
    pub kind: SlotKind,
    /// The leading horizontal whitespace of the slot's (first) line;
    /// continuation lines of a multi-line rendering start with it.
    pub indent: String,
    /// The line terminator joining rendered lines (`"\n"` or `"\r\n"`).
    pub eol: &'static str,
    /// Code follows the slot on its last line: single-line text only.
    pub inline: bool,
    /// Code precedes the slot on its first line (the scanner's
    /// `position == trailing`): single-line text only.
    pub trailing: bool,
    /// The original span contained a line terminator.
    pub span_had_line_break: bool,
    /// Whitespace emitted before the opener. Empty for scanned slots (the
    /// original spacing between code and opener lies outside the replaced
    /// range and survives verbatim); a single space for a trailing NEW
    /// insertion that does not already follow whitespace.
    pub trailing_gap: &'static str,
    /// The opener, captured verbatim (`//`, `///`, `#:`, `/**`, `/*` …).
    pub opener: String,
    /// The block closer for block/doc slots (`*/`); empty for line slots.
    pub block_close: &'static str,
}

impl Slot {
    /// The language's default slot of `kind`, as the sanitizer renders
    /// candidate text: the plain line prefix (`//` / `#`) for line slots,
    /// the block opener (`/*`) for block slots and the doc opener (`/**`)
    /// for doc slots; no indent, `\n` EOL, own-line. `None` when the
    /// language has no comment of that kind (block/doc in Python).
    pub fn canonical(kind: SlotKind, adapter: &LanguageAdapter) -> Option<Slot> {
        let (opener, block_close) = match kind {
            SlotKind::Line => (adapter.line_prefixes.first()?.to_string(), ""),
            SlotKind::Block => {
                let delimiters = adapter.block_delimiters?;
                (delimiters.open.to_string(), delimiters.close)
            }
            SlotKind::Doc => {
                let delimiters = adapter.block_delimiters?;
                (format!("{}*", delimiters.open), delimiters.close)
            }
        };
        Some(Slot {
            kind,
            indent: String::new(),
            eol: "\n",
            inline: false,
            trailing: false,
            span_had_line_break: false,
            trailing_gap: "",
            opener,
            block_close,
        })
    }
}

/// A slot's kind, opener and closer.
struct Shape {
    kind: SlotKind,
    opener: String,
    block_close: &'static str,
}

/// The shape of a scanned comment, from its raw text: a line comment keeps
/// its `line_opener` match; a block comment keeps `/*!`, `/**` when
/// doc-like, else `/*`. `None` when `raw` is not a comment of the language.
///
/// "Doc-like" is the record's own doc classification where the language
/// detects doc comments (so Rust's `/***`, which rustc does not treat as
/// doc, stays `/*`), and the JSDoc text rule (`/**` followed by a non-`/`)
/// where it does not (a C/C++ Doxygen `/** */` stays `/** */`).
fn scanned_shape(raw: &str, kind: CommentKind, adapter: &LanguageAdapter) -> Option<Shape> {
    if let Some(prefix) = adapter
        .line_prefixes
        .iter()
        .find(|prefix| raw.starts_with(**prefix))
    {
        let length = opener_len(raw, adapter).unwrap_or(prefix.len());
        return Some(Shape {
            kind: SlotKind::Line,
            opener: raw[..length].to_string(),
            block_close: "",
        });
    }
    let delimiters = adapter.block_delimiters?;
    let rest = raw.strip_prefix(delimiters.open)?;
    let doc_like = match adapter.doc {
        Some(_) => kind == CommentKind::Doc,
        None => rest
            .strip_prefix('*')
            .is_some_and(|after| !after.starts_with('/')),
    };
    let marker = if rest.starts_with('!') {
        "!"
    } else if doc_like && rest.starts_with('*') {
        "*"
    } else {
        ""
    };
    Some(Shape {
        kind: if kind == CommentKind::Doc {
            SlotKind::Doc
        } else {
            SlotKind::Block
        },
        opener: format!("{}{marker}", delimiters.open),
        block_close: delimiters.close,
    })
}

/// The kind of slot a scanned comment's replacement text is written into:
/// the shape [`slot_for`] derives from its raw text (a Rust `///` doc
/// comment is a line slot, a JSDoc `/** */` a doc slot). Plan time
/// validates a judge's `finalComment` against exactly this kind, so it
/// agrees with apply. `None` when `raw` is not a comment of the language.
pub fn scanned_slot_kind(
    raw: &str,
    kind: CommentKind,
    adapter: &LanguageAdapter,
) -> Option<SlotKind> {
    scanned_shape(raw, kind, adapter).map(|shape| shape.kind)
}

/// The shape of a NEW record: the plain line prefix for `line`, the block
/// opener for `block`. `None` for `doc` (NEW records are never doc) or a
/// kind the language lacks.
fn new_shape(kind: CommentKind, adapter: &LanguageAdapter) -> Option<Shape> {
    let slot_kind = match kind {
        CommentKind::Line => SlotKind::Line,
        CommentKind::Block => SlotKind::Block,
        CommentKind::Doc => return None,
    };
    Slot::canonical(slot_kind, adapter).map(|slot| Shape {
        kind: slot.kind,
        opener: slot.opener,
        block_close: slot.block_close,
    })
}

/// The slot `record` occupies in `text` (the file's current, original
/// text), per the module rules.
///
/// # Errors
///
/// [`Error::Artifact`] when the record does not fit the text: its range is
/// out of bounds or off a `char` boundary, a scanned record's range does
/// not hold a comment of the language, or a NEW record's kind is `doc` or
/// one the language lacks.
pub fn slot_for(
    text: &str,
    record: &CommentRecord,
    adapter: &LanguageAdapter,
) -> Result<Slot, Error> {
    let (start, end) = (record.start, record.end);
    if start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(Error::Artifact(format!(
            "{}: range [{start}, {end}) does not fit the {}-byte source text",
            record.id,
            text.len()
        )));
    }
    let new_origin = record.is_new_origin();
    let raw = &text[start..end];
    let shape = if new_origin {
        new_shape(record.kind, adapter).ok_or_else(|| {
            Error::Artifact(format!(
                "{}: a NEW {} comment cannot be written in {}",
                record.id,
                kind_name(record.kind),
                adapter.id.as_str()
            ))
        })?
    } else {
        scanned_shape(raw, record.kind, adapter).ok_or_else(|| {
            Error::Artifact(format!(
                "{}: [{start}, {end}) does not hold a {} comment",
                record.id,
                adapter.id.as_str()
            ))
        })?
    };

    let line_start = line_start_of(text, start);
    let indent: String = text[line_start..]
        .chars()
        .take_while(|&c| c.is_whitespace() && !is_line_terminator(c))
        .collect();
    let trailing = record.position == Position::Trailing;
    // NEW insertions occupy a line of their own (leading) or end their
    // line (trailing); nothing of theirs is followed by code.
    let inline = !new_origin && code_follows_on_line(text, end);
    let span_had_line_break = raw.chars().any(is_line_terminator);
    let follows_whitespace = text[line_start..start]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace);
    let trailing_gap = if new_origin && trailing && !follows_whitespace {
        " "
    } else {
        ""
    };
    Ok(Slot {
        kind: shape.kind,
        indent,
        eol: slot_eol(text, start),
        inline,
        trailing,
        span_had_line_break,
        trailing_gap,
        opener: shape.opener,
        block_close: shape.block_close,
    })
}

/// The slot of `record` derived without source text (for statistics):
/// indent `""`, EOL `"\n"`, `trailing` = `position == trailing`, never
/// inline, no span line break; opener and closer from the record's
/// `original` text (the language default for NEW records). A record
/// whose shape cannot be derived (only possible in a hand-edited artifact)
/// falls back to the language's default slot of its kind, else to a line
/// slot.
pub fn slot_from_record(record: &CommentRecord, adapter: &LanguageAdapter) -> Slot {
    let shape = match &record.original {
        Some(original) => scanned_shape(original, record.kind, adapter),
        None => new_shape(record.kind, adapter),
    };
    let trailing = record.position == Position::Trailing;
    let fallback = || {
        let kind = match record.kind {
            CommentKind::Line => SlotKind::Line,
            CommentKind::Block => SlotKind::Block,
            CommentKind::Doc => SlotKind::Doc,
        };
        Slot::canonical(kind, adapter)
            .or_else(|| Slot::canonical(SlotKind::Line, adapter))
            .map(|slot| Shape {
                kind: slot.kind,
                opener: slot.opener,
                block_close: slot.block_close,
            })
    };
    let shape = shape.or_else(fallback).unwrap_or(Shape {
        kind: SlotKind::Line,
        opener: String::new(),
        block_close: "",
    });
    Slot {
        kind: shape.kind,
        indent: String::new(),
        eol: "\n",
        inline: false,
        trailing,
        span_had_line_break: false,
        trailing_gap: if record.is_new_origin() && trailing {
            " "
        } else {
            ""
        },
        opener: shape.opener,
        block_close: shape.block_close,
    }
}

fn kind_name(kind: CommentKind) -> &'static str {
    match kind {
        CommentKind::Line => "line",
        CommentKind::Block => "block",
        CommentKind::Doc => "doc",
    }
}

/// Whether non-whitespace text follows `offset` before the next `\n`.
fn code_follows_on_line(text: &str, offset: usize) -> bool {
    text[offset..]
        .split('\n')
        .next()
        .is_some_and(|rest| rest.chars().any(|c| !c.is_whitespace()))
}

/// The slot's EOL: the terminator ending the line holding `start`; else the
/// file's first terminator; else `\n`.
fn slot_eol(text: &str, start: usize) -> &'static str {
    let crlf_or_lf = |newline: usize| {
        if newline > 0 && text.as_bytes()[newline - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    };
    match text[start..].find('\n') {
        Some(at) => crlf_or_lf(start + at),
        None => text.find('\n').map_or("\n", crlf_or_lf),
    }
}

/// `text` split into lines at every line terminator (`\r\n`, `\n`, `\r`,
/// U+2028, U+2029) — the same set normalization folds, so even unnormalized
/// text can never end a line comment early.
fn text_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some((at, terminator)) = rest.char_indices().find(|&(_, c)| is_line_terminator(c)) {
        lines.push(&rest[..at]);
        let mut next = at + terminator.len_utf8();
        if terminator == '\r' && rest[next..].starts_with('\n') {
            next += 1;
        }
        rest = &rest[next..];
    }
    lines.push(rest);
    lines
}

/// `{opener}` plus ` {line}` unless the line is empty.
fn push_opened(out: &mut String, opener: &str, line: &str) {
    out.push_str(opener);
    if !line.is_empty() {
        out.push(' ');
        out.push_str(line);
    }
}

/// Renders `final_comment` into `slot`, byte-exactly per the module
/// rules. The result replaces the slot's original range (or, for NEW, is
/// the inserted comment: a leading insertion adds the slot's indent before
/// and its EOL after).
pub fn render_comment(final_comment: &str, slot: &Slot) -> String {
    let lines = text_lines(final_comment);
    let single_line = slot.inline || slot.trailing;
    let mut out = String::from(slot.trailing_gap);
    let one_line_block = |out: &mut String, text: &str| {
        out.push_str(&slot.opener);
        out.push(' ');
        out.push_str(text);
        out.push(' ');
        out.push_str(slot.block_close);
    };
    match slot.kind {
        SlotKind::Line if single_line => push_opened(&mut out, &slot.opener, &lines.join(" ")),
        SlotKind::Line => {
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    out.push_str(slot.eol);
                    out.push_str(&slot.indent);
                }
                push_opened(&mut out, &slot.opener, line);
            }
        }
        SlotKind::Block | SlotKind::Doc if single_line => {
            one_line_block(&mut out, &lines.join(" "));
            if slot.inline && slot.span_had_line_break {
                out.push_str(slot.eol);
            }
        }
        SlotKind::Block | SlotKind::Doc => match lines.as_slice() {
            [line] => one_line_block(&mut out, line),
            _ => {
                out.push_str(&slot.opener);
                for line in &lines {
                    out.push_str(slot.eol);
                    out.push_str(&slot.indent);
                    push_opened(&mut out, " *", line);
                }
                out.push_str(slot.eol);
                out.push_str(&slot.indent);
                out.push(' ');
                out.push_str(slot.block_close);
            }
        },
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::types::{DecidedBy, Decision};
    use crate::comments::inventory::inventory_file;
    use crate::comments::types::{Anchor, Classification, FileRef};
    use crate::lang::adapters::adapter_for;
    use crate::lang::ids::LanguageId;

    /// A file name for the languages these tests exercise. Deliberately
    /// not exhaustive over `LanguageId`: adding a language must not
    /// require touching the engine, its tests included.
    fn file_name(language: LanguageId) -> &'static str {
        match language {
            LanguageId::Javascript => "a.js",
            LanguageId::Typescript => "a.ts",
            LanguageId::Python => "a.py",
            LanguageId::Java => "A.java",
            LanguageId::C => "a.c",
            LanguageId::Cpp => "a.cpp",
            LanguageId::Rust => "a.rs",
            other => panic!("no test file name for {other:?}"),
        }
    }

    fn record(
        id: &str,
        kind: CommentKind,
        (start, end): (usize, usize),
        position: Position,
        original: Option<String>,
    ) -> CommentRecord {
        CommentRecord {
            id: id.to_owned(),
            kind,
            start,
            end,
            line: 1,
            column: 1,
            position,
            classification: Classification::Prose,
            tags: Vec::new(),
            original,
            anchor: Anchor {
                node_kind: "program".to_owned(),
                name: None,
                snippet: String::new(),
            },
            regenerated: None,
            would_you_comment_here: None,
            difficulty: None,
            difficulty_rationale: None,
            missing_context: None,
            decision: Decision::KeepGenerated,
            final_comment: None,
            confidence: 1.0,
            decision_rationale: None,
            decided_by: DecidedBy::Human,
        }
    }

    /// The scanned record whose text starts with `needle`.
    fn scanned(text: &str, language: LanguageId, needle: &str) -> CommentRecord {
        let inventory = inventory_file(
            text,
            FileRef {
                language,
                file_name: file_name(language).to_owned(),
            },
        )
        .expect("inventory");
        let comment = inventory
            .comments
            .iter()
            .find(|comment| comment.text.starts_with(needle))
            .unwrap_or_else(|| panic!("no comment starting {needle:?} in {text:?}"));
        record(
            &comment.id,
            comment.kind,
            (comment.start, comment.end),
            comment.position,
            Some(comment.text.clone()),
        )
    }

    /// `text` with the comment starting `needle` replaced by `final_comment`.
    fn rewrite(text: &str, language: LanguageId, needle: &str, final_comment: &str) -> String {
        let record = scanned(text, language, needle);
        let slot = slot_for(text, &record, adapter_for(language)).expect("slot");
        let mut out = text.to_owned();
        out.replace_range(
            record.start..record.end,
            &render_comment(final_comment, &slot),
        );
        out
    }

    #[test]
    fn line_slots_render_one_opener_per_line_at_the_indent() {
        let text = "function f() {\n  // old\n  g();\n}\n";
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "// old", "first\n\nthird"),
            "function f() {\n  // first\n  //\n  // third\n  g();\n}\n",
        );
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "// old", "only"),
            "function f() {\n  // only\n  g();\n}\n",
        );
    }

    #[test]
    fn block_slots_render_one_line_and_decorated_multi_line_forms() {
        let text = "/* old */\nint x;\n";
        assert_eq!(
            rewrite(text, LanguageId::C, "/* old", "new text"),
            "/* new text */\nint x;\n",
        );
        let text = "function f() {\n    /**\n     * old\n     */\n    function g() {}\n}\n";
        assert_eq!(
            rewrite(
                text,
                LanguageId::Javascript,
                "/**",
                "line one\n\nline three"
            ),
            "function f() {\n    /**\n     * line one\n     *\n     * line three\n     */\n    function g() {}\n}\n",
        );
        // A multi-line original receiving one-line text.
        let text = "/*\n * old\n * text\n */\nint x;\n";
        assert_eq!(
            rewrite(text, LanguageId::C, "/*", "short"),
            "/* short */\nint x;\n"
        );
    }

    #[test]
    fn trailing_slots_collapse_multi_line_text_and_keep_their_gap() {
        let text = "g();    // old\nh();\n";
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "// old", "a\nb\n\nc"),
            "g();    // a b  c\nh();\n",
        );
        let text = "int x; /* old */\nint y;\n";
        assert_eq!(
            rewrite(text, LanguageId::C, "/* old", "a\nb"),
            "int x; /* a b */\nint y;\n",
        );
    }

    #[test]
    fn inline_slots_take_one_line_and_keep_an_original_line_break() {
        let text = "function f(x) {\n  return /* a\n b */ x;\n}\n";
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "/* a", "new\ntext"),
            "function f(x) {\n  return /* new text */\n x;\n}\n",
        );
        let text = "const s = a /* old */ + b;\n";
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "/* old", "new\ntext"),
            "const s = a /* new text */ + b;\n",
        );
        // CRLF: the kept break is the slot line's.
        let text = "function f(x) {\r\n  return /* a\r\n b */ x;\r\n}\r\n";
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "/* a", "new"),
            "function f(x) {\r\n  return /* new */\r\n x;\r\n}\r\n",
        );
    }

    #[test]
    fn openers_are_reused_verbatim() {
        let text = "/// old\nint x;\n";
        assert_eq!(
            rewrite(text, LanguageId::Cpp, "///", "new\ntext"),
            "/// new\n/// text\nint x;\n",
        );
        let text = "/** old */\nint x;\n";
        assert_eq!(
            rewrite(text, LanguageId::C, "/**", "new"),
            "/** new */\nint x;\n"
        );
        let text = "#: old\nx = 1\n";
        assert_eq!(
            rewrite(text, LanguageId::Python, "#:", "new\ntext"),
            "#: new\n#: text\nx = 1\n",
        );
        let text = "//! old\nfn f() {}\n";
        assert_eq!(
            rewrite(text, LanguageId::Rust, "//!", "new"),
            "//! new\nfn f() {}\n"
        );
        let text = "//// old\nfn f() {}\n";
        assert_eq!(
            rewrite(text, LanguageId::Rust, "////", "new"),
            "//// new\nfn f() {}\n"
        );
    }

    #[test]
    fn block_openers_keep_their_doc_ness() {
        // rustc does not treat `/***` as doc: it must not become `/**`.
        let text = "/*** old */\nfn f() {}\n";
        assert_eq!(
            rewrite(text, LanguageId::Rust, "/***", "new"),
            "/* new */\nfn f() {}\n"
        );
        let text = "/** old */\nfn f() {}\n";
        assert_eq!(
            rewrite(text, LanguageId::Rust, "/**", "new"),
            "/** new */\nfn f() {}\n"
        );
        // `/**/` is not doc-like.
        let text = "int x; /**/\nint y;\n";
        let record = scanned(text, LanguageId::C, "/**/");
        let slot = slot_for(text, &record, adapter_for(LanguageId::C)).expect("slot");
        assert_eq!((slot.kind, slot.opener.as_str()), (SlotKind::Block, "/*"));
        // Java's `///` doc lines are line-shaped: a line slot.
        let text = "class A {\n    /// old\n    void f() {}\n}\n";
        let record = scanned(text, LanguageId::Java, "///");
        let slot = slot_for(text, &record, adapter_for(LanguageId::Java)).expect("slot");
        assert_eq!(record.kind, CommentKind::Doc);
        assert_eq!((slot.kind, slot.opener.as_str()), (SlotKind::Line, "///"));
        let text = "class A {\n    /** old */\n    void f() {}\n}\n";
        let record = scanned(text, LanguageId::Java, "/**");
        let slot = slot_for(text, &record, adapter_for(LanguageId::Java)).expect("slot");
        assert_eq!((slot.kind, slot.opener.as_str()), (SlotKind::Doc, "/**"));
    }

    #[test]
    fn rendering_uses_the_slot_lines_eol() {
        // Mixed EOLs: each slot takes its own line's terminator.
        let text = "// a\r\nx();\n// b\ny();\r\n";
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "// a", "1\n2"),
            "// 1\r\n// 2\r\nx();\n// b\ny();\r\n",
        );
        assert_eq!(
            rewrite(text, LanguageId::Javascript, "// b", "1\n2"),
            "// a\r\nx();\n// 1\n// 2\ny();\r\n",
        );
        // CRLF stays CRLF, block layout included.
        let text = "x = 1\r\n# old\r\ny = 2\r\n";
        assert_eq!(
            rewrite(text, LanguageId::Python, "# old", "1\n2"),
            "x = 1\r\n# 1\r\n# 2\r\ny = 2\r\n",
        );
        let text = "/* old */\r\nint x;\r\n";
        assert_eq!(
            rewrite(text, LanguageId::C, "/*", "1\n\n2"),
            "/*\r\n * 1\r\n *\r\n * 2\r\n */\r\nint x;\r\n",
        );
        // An unterminated last line takes the file's first terminator.
        let text = "x = 1\r\ny = 2\n# old";
        assert_eq!(
            rewrite(text, LanguageId::Python, "# old", "1\n2"),
            "x = 1\r\ny = 2\n# 1\r\n# 2",
        );
        // No terminator anywhere: `\n`.
        assert_eq!(
            rewrite("# old", LanguageId::Python, "# old", "1\n2"),
            "# 1\n# 2"
        );
    }

    #[test]
    fn every_line_terminator_starts_a_new_opened_line() {
        let text = "// old\nx();\n";
        let record = scanned(text, LanguageId::Javascript, "// old");
        let slot = slot_for(text, &record, adapter_for(LanguageId::Javascript)).expect("slot");
        // Even unnormalized text: U+2028, U+2029, a lone CR, CRLF.
        assert_eq!(
            render_comment("a\u{2028}b\u{2029}c\rd\r\ne", &slot),
            "// a\n// b\n// c\n// d\n// e",
        );
    }

    #[test]
    fn a_leading_bom_is_not_indent() {
        let text = "\u{FEFF}// old\nx();\n";
        assert_eq!(
            rewrite(text, LanguageId::Typescript, "// old", "1\n2"),
            "\u{FEFF}// 1\n// 2\nx();\n",
        );
    }

    #[test]
    fn new_records_use_the_language_default_openers() {
        let text = "  x();\ny();\n";
        let js = adapter_for(LanguageId::Javascript);
        let leading = record("N1", CommentKind::Line, (0, 0), Position::Leading, None);
        let slot = slot_for(text, &leading, js).expect("slot");
        assert_eq!(slot.indent, "  ");
        assert!(!slot.inline && !slot.trailing);
        assert_eq!(render_comment("new\ntext", &slot), "// new\n  // text");

        let block = record("N1", CommentKind::Block, (0, 0), Position::Leading, None);
        let slot = slot_for(text, &block, js).expect("slot");
        assert_eq!(render_comment("new", &slot), "/* new */");

        // A trailing NEW: single line, one space of gap.
        let trailing = record("N2", CommentKind::Line, (6, 6), Position::Trailing, None);
        let slot = slot_for(text, &trailing, js).expect("slot");
        assert_eq!(render_comment("a\nb", &slot), " // a b");
        // An insertion point already after whitespace needs no gap.
        let text = "x();  \n";
        let trailing = record("N2", CommentKind::Line, (6, 6), Position::Trailing, None);
        let slot = slot_for(text, &trailing, js).expect("slot");
        assert_eq!(render_comment("a", &slot), "// a");

        let py = adapter_for(LanguageId::Python);
        let leading = record("N1", CommentKind::Line, (0, 0), Position::Leading, None);
        let slot = slot_for("x = 1\n", &leading, py).expect("slot");
        assert_eq!(render_comment("new", &slot), "# new");
    }

    #[test]
    fn slot_for_rejects_records_that_do_not_fit_the_text() {
        let js = adapter_for(LanguageId::Javascript);
        let text = "// é\nx();\n";
        let out_of_bounds = record("C1", CommentKind::Line, (0, 99), Position::Leading, None);
        assert!(matches!(
            slot_for(text, &out_of_bounds, js),
            Err(Error::Artifact(_))
        ));
        let off_boundary = record(
            "C1",
            CommentKind::Line,
            (0, 4),
            Position::Leading,
            Some("x".into()),
        );
        assert!(matches!(
            slot_for(text, &off_boundary, js),
            Err(Error::Artifact(_))
        ));
        let not_a_comment = record(
            "C1",
            CommentKind::Line,
            (6, 10),
            Position::Leading,
            Some("x();".into()),
        );
        assert!(matches!(
            slot_for(text, &not_a_comment, js),
            Err(Error::Artifact(_))
        ));
        let new_doc = record("N1", CommentKind::Doc, (0, 0), Position::Leading, None);
        assert!(matches!(
            slot_for(text, &new_doc, js),
            Err(Error::Artifact(_))
        ));
        let py_block = record("N1", CommentKind::Block, (0, 0), Position::Leading, None);
        assert!(matches!(
            slot_for("x = 1\n", &py_block, adapter_for(LanguageId::Python)),
            Err(Error::Artifact(_))
        ));
    }

    #[test]
    fn slot_from_record_needs_no_source_text() {
        let js = adapter_for(LanguageId::Javascript);
        let doc = record(
            "C1",
            CommentKind::Doc,
            (10, 22),
            Position::Trailing,
            Some("/** old */".into()),
        );
        let slot = slot_from_record(&doc, js);
        assert_eq!(
            slot,
            Slot {
                kind: SlotKind::Doc,
                indent: String::new(),
                eol: "\n",
                inline: false,
                trailing: true,
                span_had_line_break: false,
                trailing_gap: "",
                opener: "/**".into(),
                block_close: "*/",
            }
        );
        let new = record("N1", CommentKind::Block, (0, 0), Position::Leading, None);
        let slot = slot_from_record(&new, js);
        assert_eq!((slot.kind, slot.opener.as_str()), (SlotKind::Block, "/*"));
        assert_eq!(render_comment("a\nb", &slot), "/*\n * a\n * b\n */");
        // A shape that cannot be derived falls back to the default slot.
        let new_doc = record("N1", CommentKind::Doc, (0, 0), Position::Leading, None);
        assert_eq!(slot_from_record(&new_doc, js).opener, "/**");
        let py_block = record("N1", CommentKind::Block, (0, 0), Position::Leading, None);
        assert_eq!(
            slot_from_record(&py_block, adapter_for(LanguageId::Python)).opener,
            "#"
        );
    }
}
