//! Unit tests for the inventory pipeline: directive adjacency (next-scoped
//! and line-window), logical grouping and its breakers, group field
//! derivation, gap-free ids, the `editable` flag and style statistics —
//! the adjacency and grouping cases run in every tier-1 language.

use super::*;
use crate::comments::classify::{ClassifyCtx, classify_all};
use crate::comments::scan::scan_file;
use crate::lang::ids::LanguageId;

use Classification::{DocProtected, Functional, Prose};
use LanguageId::{C, Cpp, Go, Java, Javascript, Python, Rust, Tsx, Typescript};

/// One language's snippet scaffolding: a function whose body the cases
/// fill with indented lines.
struct Lang {
    id: LanguageId,
    file_name: &'static str,
    /// The line-comment opener.
    prefix: &'static str,
    /// Everything up to the function body.
    open: &'static str,
    /// Everything after the function body.
    close: &'static str,
    /// The body's indent.
    indent: &'static str,
    /// A simple statement valid in the body.
    stmt: &'static str,
    /// A next-scoped directive on its own line.
    next_directive: &'static str,
}

const LANGS: &[Lang] = &[
    Lang {
        id: Javascript,
        file_name: "a.js",
        prefix: "//",
        open: "function f() {\n",
        close: "}\n",
        indent: "  ",
        stmt: "let x = 1;",
        next_directive: "// eslint-disable-next-line no-console",
    },
    Lang {
        id: Typescript,
        file_name: "a.ts",
        prefix: "//",
        open: "function f(): void {\n",
        close: "}\n",
        indent: "  ",
        stmt: "let x = 1;",
        next_directive: "// @ts-expect-error",
    },
    Lang {
        id: Tsx,
        file_name: "a.tsx",
        prefix: "//",
        open: "function f() {\n",
        close: "}\n",
        indent: "  ",
        stmt: "let x = 1;",
        next_directive: "// eslint-disable-next-line no-console",
    },
    Lang {
        id: Python,
        file_name: "a.py",
        prefix: "#",
        open: "def f():\n",
        close: "",
        indent: "    ",
        stmt: "x = 1",
        next_directive: "# pylint: disable-next=unused-variable",
    },
    Lang {
        id: Go,
        file_name: "a.go",
        prefix: "//",
        open: "package p\n\nfunc f() {\n",
        close: "}\n",
        indent: "\t",
        stmt: "x := 1",
        next_directive: "//nolint:errcheck",
    },
    Lang {
        id: Java,
        file_name: "A.java",
        prefix: "//",
        open: "class A {\n    void f() {\n",
        close: "    }\n}\n",
        indent: "        ",
        stmt: "int x = 1;",
        next_directive: "//noinspection unchecked",
    },
    Lang {
        id: C,
        file_name: "a.c",
        prefix: "//",
        open: "int f(void) {\n",
        close: "}\n",
        indent: "    ",
        stmt: "int x = 1;",
        next_directive: "// NOLINTNEXTLINE(bugprone-branch-clone)",
    },
    Lang {
        id: Cpp,
        file_name: "a.cpp",
        prefix: "//",
        open: "int f() {\n",
        close: "}\n",
        indent: "    ",
        stmt: "int x = 1;",
        next_directive: "// NOLINTNEXTLINE(bugprone-branch-clone)",
    },
    Lang {
        id: Rust,
        file_name: "a.rs",
        prefix: "//",
        open: "fn f() {\n",
        close: "}\n",
        indent: "    ",
        stmt: "let x = 1;",
        next_directive: "// SAFETY: the caller upholds the invariant",
    },
];

impl Lang {
    /// A prose line comment.
    fn prose(&self, words: &str) -> String {
        format!("{} {words}", self.prefix)
    }

    /// The function with `lines` as its body: each non-empty line is
    /// indented by the body indent, an empty line stays blank.
    fn wrap(&self, lines: &[String]) -> String {
        let mut source = self.open.to_string();
        for line in lines {
            if !line.is_empty() {
                source.push_str(self.indent);
                source.push_str(line);
            }
            source.push('\n');
        }
        source.push_str(self.close);
        source
    }

    fn inventory(&self, source: &str) -> Inventory {
        inventory_of(self.id, self.file_name, source)
    }
}

fn inventory_of(language: LanguageId, file_name: &str, source: &str) -> Inventory {
    inventory_file(
        source,
        FileRef {
            language,
            file_name: file_name.to_string(),
        },
    )
    .unwrap_or_else(|error| panic!("{file_name}: inventory failed: {error:?}\n{source}"))
}

/// The record whose text contains `needle`.
fn record<'a>(inventory: &'a Inventory, needle: &str) -> &'a InventoryComment {
    inventory
        .comments
        .iter()
        .find(|comment| comment.text.contains(needle))
        .unwrap_or_else(|| panic!("no record contains {needle:?}: {:#?}", inventory.comments))
}

fn texts(inventory: &Inventory) -> Vec<&str> {
    inventory
        .comments
        .iter()
        .map(|comment| comment.text.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// Directive adjacency.

#[test]
fn next_scoped_directive_protects_following_prose_in_every_language() {
    for lang in LANGS {
        let source = lang.wrap(&[
            lang.next_directive.to_string(),
            lang.prose("alpha prose between directive and code"),
            lang.stmt.to_string(),
        ]);
        let inventory = lang.inventory(&source);
        assert_eq!(inventory.comments.len(), 2, "{}: {source}", lang.file_name);
        let directive = record(&inventory, lang.next_directive);
        assert_eq!(directive.classification, Functional, "{}", lang.file_name);
        assert_eq!(
            directive.directive_scope,
            Some(Scope::Next),
            "{}",
            lang.file_name
        );
        let prose = record(&inventory, "alpha prose");
        assert_eq!(prose.classification, Functional, "{}", lang.file_name);
        assert!(
            prose.tags.contains("directive-adjacent"),
            "{}",
            lang.file_name
        );
        assert!(!prose.editable, "{}", lang.file_name);
    }
}

#[test]
fn directive_directly_followed_by_code_protects_nothing_extra_in_every_language() {
    for lang in LANGS {
        let source = lang.wrap(&[
            lang.next_directive.to_string(),
            lang.stmt.to_string(),
            lang.prose("alpha prose after the code"),
            lang.stmt.to_string(),
        ]);
        let inventory = lang.inventory(&source);
        assert_eq!(inventory.comments.len(), 2, "{}: {source}", lang.file_name);
        let prose = record(&inventory, "alpha prose");
        assert_eq!(prose.classification, Prose, "{}", lang.file_name);
        assert!(
            !prose.tags.contains("directive-adjacent"),
            "{}",
            lang.file_name
        );
        assert!(prose.editable, "{}", lang.file_name);
    }
}

#[test]
fn ts_expect_error_protects_jsdoc_between_it_and_code() {
    for (language, file_name) in [(Typescript, "a.ts"), (Tsx, "a.tsx"), (Javascript, "a.js")] {
        let source = "// @ts-expect-error\n/** prose about the value */\nconst x = missing;\n";
        let inventory = inventory_of(language, file_name, source);
        assert_eq!(inventory.comments.len(), 2, "{file_name}");
        let prose = record(&inventory, "prose about");
        assert_eq!(prose.kind, CommentKind::Doc, "{file_name}");
        assert_eq!(prose.classification, Functional, "{file_name}");
        assert!(prose.tags.contains("directive-adjacent"), "{file_name}");
    }
}

#[test]
fn eslint_disable_next_line_directly_followed_by_code() {
    let source =
        "// eslint-disable-next-line no-console\nconsole.log(1);\n// alpha prose\nlet x = 1;\n";
    let inventory = inventory_of(Javascript, "a.js", source);
    let prose = record(&inventory, "alpha prose");
    assert_eq!(prose.classification, Prose);
    assert_eq!(prose.tags, BTreeSet::new());
}

#[test]
fn nolintnextline_in_cpp_protects_prose_before_its_target() {
    let source = "// NOLINTNEXTLINE(cppcoreguidelines-avoid-magic-numbers)\n/* alpha prose */\n// beta prose\nint x = 42;\n";
    let inventory = inventory_of(Cpp, "a.cpp", source);
    assert_eq!(inventory.comments.len(), 3);
    for needle in ["alpha prose", "beta prose"] {
        let prose = record(&inventory, needle);
        assert_eq!(prose.classification, Functional, "{needle}");
        assert!(prose.tags.contains("directive-adjacent"), "{needle}");
    }
}

#[test]
fn c8_ignore_next_3_window_covers_three_lines() {
    let source = "\
/* c8 ignore next 3 */
a();
// prose on line three
// prose on line four
// prose on line five
b();
";
    let inventory = inventory_of(Javascript, "a.js", source);
    let directive = record(&inventory, "c8 ignore");
    assert_eq!(directive.directive_line_window, Some(3));
    for needle in ["line three", "line four"] {
        let prose = record(&inventory, needle);
        assert_eq!(prose.classification, Functional, "{needle}");
        assert!(prose.tags.contains("directive-adjacent"), "{needle}");
    }
    let outside = record(&inventory, "line five");
    assert_eq!(outside.classification, Prose);
    assert!(outside.editable);
}

#[test]
fn c8_ignore_next_2_window_covers_prose_starting_on_the_next_two_lines_only() {
    // The first code token after the directive precedes every comment
    // below, so only the window protects. Line 2's trailing comment and
    // the block starting on line 3 (it extends past the window: the start
    // line counts) are covered; nothing starting on line 4 or later is.
    let source = "\
/* c8 ignore next 2 */
a(); // trailing prose on line two
b(); /* block prose from line three
spanning line four */ // prose starting on line four
// prose on line five
c();
";
    let inventory = inventory_of(Javascript, "a.js", source);
    assert_eq!(inventory.comments.len(), 5);
    for needle in ["line two", "block prose from line three"] {
        let prose = record(&inventory, needle);
        assert_eq!(prose.classification, Functional, "{needle}");
        assert!(prose.tags.contains("directive-adjacent"), "{needle}");
    }
    for needle in ["starting on line four", "line five"] {
        let prose = record(&inventory, needle);
        assert_eq!(prose.classification, Prose, "{needle}");
        assert!(!prose.tags.contains("directive-adjacent"), "{needle}");
    }
}

#[test]
fn line_window_counts_blank_lines() {
    // Line 2 is blank but still one of the window's two lines.
    let source =
        "/* v8 ignore next 2 */\n\na(); // prose on line three\nb(); // prose on line four\n";
    let inventory = inventory_of(Typescript, "a.ts", source);
    let covered = record(&inventory, "line three");
    assert_eq!(covered.classification, Functional);
    assert!(covered.tags.contains("directive-adjacent"));
    let outside = record(&inventory, "line four");
    assert_eq!(outside.classification, Prose);
}

// ---------------------------------------------------------------------------
// Logical grouping.

#[test]
fn indented_run_inside_a_function_body_groups_in_every_language() {
    for lang in LANGS {
        let alpha = lang.prose("alpha prose paragraph");
        let beta = lang.prose("beta prose paragraph");
        let gamma = lang.prose("gamma prose paragraph");
        let source = lang.wrap(&[
            alpha.clone(),
            beta.clone(),
            gamma.clone(),
            lang.stmt.to_string(),
        ]);
        let inventory = lang.inventory(&source);
        assert_eq!(inventory.comments.len(), 1, "{}: {source}", lang.file_name);
        let group = &inventory.comments[0];
        let indent = lang.indent;
        assert_eq!(
            group.text,
            format!("{alpha}\n{indent}{beta}\n{indent}{gamma}"),
            "{}",
            lang.file_name,
        );
        assert_eq!(group.members.len(), 3, "{}", lang.file_name);
        assert_eq!(&source[group.start..group.end], group.text);
        for (member, text) in group.members.iter().zip([&alpha, &beta, &gamma]) {
            assert_eq!(&source[member.clone()], text, "{}", lang.file_name);
        }
        assert_eq!(group.kind, CommentKind::Line, "{}", lang.file_name);
        assert_eq!(group.classification, Prose, "{}", lang.file_name);
        assert!(group.editable, "{}", lang.file_name);
        assert_eq!(group.id, "C1", "{}", lang.file_name);
        assert_eq!(
            group.column as usize,
            indent.chars().count() + 1,
            "{}",
            lang.file_name,
        );
    }
}

/// Asserts that `lines` (a function body) yields `expected` records, each
/// a single comment.
fn assert_ungrouped(lang: &Lang, lines: &[String], expected: usize, case: &str) {
    let source = lang.wrap(lines);
    let inventory = lang.inventory(&source);
    assert_eq!(
        inventory.comments.len(),
        expected,
        "{} ({case}): {:#?}\n{source}",
        lang.file_name,
        texts(&inventory),
    );
    for comment in &inventory.comments {
        assert_eq!(
            comment.members,
            vec![comment.start..comment.end],
            "{} ({case})",
            lang.file_name
        );
    }
}

#[test]
fn grouping_breakers_in_every_language() {
    for lang in LANGS {
        let alpha = lang.prose("alpha prose paragraph");
        let beta = lang.prose("beta prose paragraph");
        let stmt = lang.stmt.to_string();

        // A functional line in between.
        let directive = format!("{} NOSONAR", lang.prefix);
        assert_ungrouped(
            lang,
            &[alpha.clone(), directive, beta.clone(), stmt.clone()],
            3,
            "directive",
        );

        // An indent change.
        let deeper = format!("  {beta}");
        assert_ungrouped(
            lang,
            &[alpha.clone(), deeper, stmt.clone()],
            2,
            "indent change",
        );

        // A blank line.
        assert_ungrouped(
            lang,
            &[alpha.clone(), String::new(), beta.clone(), stmt.clone()],
            2,
            "blank line",
        );

        // A trailing comment, with the next line's comment aligned under it.
        let trailing = format!("{stmt} {alpha}");
        let aligned = format!("{}{beta}", " ".repeat(stmt.len() + 1));
        assert_ungrouped(lang, &[trailing, aligned, stmt.clone()], 2, "trailing");

        // A leading line followed by a trailing comment on the next line.
        let trailing_beta = format!("{stmt} {beta}");
        assert_ungrouped(lang, &[alpha.clone(), trailing_beta], 2, "trailing next");
    }
}

#[test]
fn a_leading_bom_does_not_break_a_line_one_group() {
    // Line 1 starts after the BOM, so a group starting there has an empty
    // indent like the lines that continue it.
    let source = "\u{FEFF}// alpha prose\n// beta prose\nlet x = 1;\n";
    let inventory = inventory_of(Javascript, "a.js", source);
    assert_eq!(texts(&inventory), ["// alpha prose\n// beta prose"]);
    assert_eq!(inventory.comments[0].members.len(), 2);
}

#[test]
fn a_block_comment_never_groups() {
    let source =
        "function f() {\n  // alpha prose\n  /* beta prose */\n  // gamma prose\n  return 1;\n}\n";
    let inventory = inventory_of(Javascript, "a.js", source);
    assert_eq!(
        texts(&inventory),
        ["// alpha prose", "/* beta prose */", "// gamma prose"]
    );
}

#[test]
fn a_rust_doc_run_never_groups() {
    let source = "/// alpha doc line\n/// beta doc line\nfn f() {}\n";
    let inventory = inventory_of(Rust, "a.rs", source);
    assert_eq!(
        texts(&inventory),
        ["/// alpha doc line", "/// beta doc line"]
    );
    for comment in &inventory.comments {
        assert_eq!(comment.kind, CommentKind::Doc);
        assert_eq!(comment.classification, DocProtected);
        assert!(!comment.editable);
        assert_eq!(comment.members.len(), 1);
    }
}

#[test]
fn a_java_markdown_doc_run_never_groups() {
    let source = "class A {\n    /// alpha doc line\n    /// beta doc line\n    void f() {}\n}\n";
    let inventory = inventory_of(Java, "A.java", source);
    assert_eq!(
        texts(&inventory),
        ["/// alpha doc line", "/// beta doc line"]
    );
    assert!(
        inventory
            .comments
            .iter()
            .all(|comment| comment.kind == CommentKind::Doc)
    );
}

#[test]
fn crlf_runs_group() {
    let source = "def f():\r\n    # alpha prose\r\n    # beta prose\r\n    return 1\r\n";
    let inventory = inventory_of(Python, "a.py", source);
    assert_eq!(texts(&inventory), ["# alpha prose\r\n    # beta prose"]);
}

#[test]
fn group_fields_are_derived_from_their_members() {
    let source = "\
function f() {
  let a = 1; // trailing first
  // alpha prose
  // TODO beta prose
  // gamma prose
  return a;
}
";
    let inventory = inventory_of(Javascript, "a.js", source);
    assert_eq!(texts(&inventory).len(), 2);
    let group = &inventory.comments[1];

    // The members, as the classifier sees them.
    let file = FileRef {
        language: Javascript,
        file_name: "a.js".to_string(),
    };
    let scanned = scan_file(source, file).expect("scan");
    let members = classify_all(
        scanned.comments,
        ClassifyCtx {
            text: source,
            file_name: "a.js",
            language: Javascript,
            has_shebang: false,
        },
    )
    .expect("classify");
    let (first, last) = (&members[1], &members[3]);

    assert_eq!(group.id, "C2");
    assert_eq!(group.start, first.start);
    assert_eq!(group.end, last.end);
    assert_eq!(group.text, &source[first.start..last.end]);
    assert_eq!((group.line, group.column), (first.line, first.column));
    assert_eq!(group.anchor, last.anchor);
    assert_eq!(group.position, last.position);
    assert_eq!(group.next_code_start, last.next_code_start);
    assert_eq!(group.prev_code_end, first.prev_code_end);
    assert_eq!(group.kind, CommentKind::Line);
    assert_eq!(
        group.members,
        members[1..=3]
            .iter()
            .map(|member| member.start..member.end)
            .collect::<Vec<_>>(),
    );
    // The union of the members' tags: the `TODO` flags the whole paragraph.
    assert!(group.tags.contains("todo"));
    assert_eq!(group.tags, members[2].tags);
    assert_eq!(group.anchor.node_kind, "return_statement");
}

// ---------------------------------------------------------------------------
// Ids, the editable flag and determinism.

#[test]
fn ids_are_gap_free_in_offset_order_after_grouping() {
    let source = "\
// alpha prose
// beta prose
let x = 1;
/* gamma prose */
let y = 2; // delta prose
// epsilon prose
// zeta prose
";
    let inventory = inventory_of(Javascript, "a.js", source);
    let ids: Vec<&str> = inventory
        .comments
        .iter()
        .map(|comment| comment.id.as_str())
        .collect();
    assert_eq!(ids, ["C1", "C2", "C3", "C4"]);
    assert_eq!(
        texts(&inventory),
        [
            "// alpha prose\n// beta prose",
            "/* gamma prose */",
            "// delta prose",
            "// epsilon prose\n// zeta prose",
        ],
    );
    assert!(
        inventory
            .comments
            .windows(2)
            .all(|pair| pair[0].end <= pair[1].start)
    );
}

#[test]
fn jsx_container_comments_are_prose_but_not_editable() {
    let source = "const a = (\n  <div>\n    {/* hidden prose */}\n  </div>\n);\n";
    let inventory = inventory_of(Tsx, "a.tsx", source);
    let comment = record(&inventory, "hidden prose");
    assert_eq!(comment.classification, Prose);
    assert!(comment.tags.contains("jsx-expression"));
    assert!(!comment.editable);
    assert_eq!(inventory.style_stats.line + inventory.style_stats.block, 0);
}

#[test]
fn shebang_is_reported_and_excluded() {
    let source = "#!/usr/bin/env python3\n# alpha prose\nx = 1\n";
    let inventory = inventory_of(Python, "a.py", source);
    assert!(inventory.has_shebang);
    assert_eq!(texts(&inventory), ["# alpha prose"]);
    assert_eq!(inventory.comments[0].id, "C1");
}

#[test]
fn inventory_is_deterministic() {
    for lang in LANGS {
        let source = lang.wrap(&[
            lang.next_directive.to_string(),
            lang.prose("alpha prose"),
            lang.stmt.to_string(),
            lang.prose("beta prose"),
            lang.prose("gamma prose"),
            lang.stmt.to_string(),
        ]);
        assert_eq!(
            lang.inventory(&source),
            lang.inventory(&source),
            "{}",
            lang.file_name
        );
    }
}

// ---------------------------------------------------------------------------
// Style statistics.

#[test]
fn style_stats_count_editable_records_only() {
    let source = "\
// eslint-disable-next-line no-console
console.log(1);
// alpha prose
// beta prose
let a = 1;
/* block prose */
let b = 2;
/** doc prose */
function g() {}
/** exported doc */
export function h() {}
";
    let inventory = inventory_of(Javascript, "a.mjs", source);
    let stats = inventory.style_stats;
    // The directive (functional) and the exported JSDoc (doc-protected)
    // are excluded; the two-line group counts once.
    assert_eq!((stats.line, stats.block, stats.doc), (1, 1, 1));
    let lengths = [
        "// alpha prose\n// beta prose".chars().count(),
        "/* block prose */".chars().count(),
        "/** doc prose */".chars().count(),
    ];
    let expected = lengths.iter().sum::<usize>() as f64 / 3.0;
    assert!(
        (stats.avg_length - expected).abs() < 1e-9,
        "{}",
        stats.avg_length
    );
    assert!(stats.language_has_line_comments);
    assert_eq!(stats.prevailing(), CommentKind::Line);
    assert_eq!(stats.prevailing_new(), CommentKind::Line);
}

#[test]
fn style_stats_avg_length_counts_chars_not_bytes() {
    let source = "# héllo wörld\nx = 1\n";
    let inventory = inventory_of(Python, "a.py", source);
    assert!((inventory.style_stats.avg_length - 13.0).abs() < 1e-9);
}

#[test]
fn style_stats_of_a_file_without_editable_prose() {
    let inventory = inventory_of(Go, "a.go", "package p\n");
    let stats = inventory.style_stats;
    assert_eq!((stats.line, stats.block, stats.doc), (0, 0, 0));
    assert_eq!(stats.avg_length, 0.0);
    assert_eq!(stats.prevailing(), CommentKind::Line);
    assert_eq!(stats.prevailing_new(), CommentKind::Line);
}

fn stats(line: u32, block: u32, doc: u32, language_has_line_comments: bool) -> StyleStats {
    StyleStats {
        line,
        block,
        doc,
        avg_length: 0.0,
        language_has_line_comments,
    }
}

#[test]
fn prevailing_style_is_the_argmax_with_line_block_doc_tie_breaks() {
    use CommentKind::{Block, Doc, Line};
    let cases = [
        // (line, block, doc) → (prevailing, prevailing NEW)
        ((3, 1, 1), (Line, Line)),
        ((1, 3, 1), (Block, Block)),
        ((1, 1, 3), (Doc, Line)),
        ((1, 2, 3), (Doc, Block)),
        ((2, 2, 1), (Line, Line)),
        ((2, 2, 2), (Line, Line)),
        ((0, 2, 2), (Block, Block)),
        ((1, 0, 1), (Line, Line)),
        ((0, 0, 4), (Doc, Line)),
        ((0, 0, 0), (Line, Line)),
    ];
    for ((line, block, doc), (prevailing, prevailing_new)) in cases {
        let stats = stats(line, block, doc, true);
        assert_eq!(stats.prevailing(), prevailing, "{line}/{block}/{doc}");
        assert_eq!(
            stats.prevailing_new(),
            prevailing_new,
            "{line}/{block}/{doc}"
        );
    }
}

#[test]
fn a_language_without_line_comments_defaults_to_block() {
    use CommentKind::{Block, Doc};
    assert_eq!(stats(0, 0, 0, false).prevailing(), Block);
    assert_eq!(stats(0, 0, 0, false).prevailing_new(), Block);
    assert_eq!(stats(0, 0, 2, false).prevailing(), Doc);
    assert_eq!(stats(0, 0, 2, false).prevailing_new(), Block);
    assert_eq!(stats(0, 1, 2, false).prevailing_new(), Block);
}
