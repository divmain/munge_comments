use std::collections::BTreeSet;

use super::*;
use crate::artifact::types::Difficulty;
use crate::comments::inventory::inventory_file;
use crate::comments::types::FileRef;
use crate::config::Mode;
use crate::lang::adapters::adapter_for;
use crate::llm::chunk::{build_chunk_texts, chunk_file};

/// A file under test: its text, inventory and remove-mode chunk texts.
struct File {
    text: String,
    language: LanguageId,
    inventory: Inventory,
    targets: BTreeSet<String>,
    chunk_texts: Vec<ChunkText>,
}

impl File {
    /// One file, targets = every editable record not tagged `todo`, cut
    /// into chunks of at most `max_chars`.
    fn chunked(text: &str, language: LanguageId, file_name: &str, max_chars: usize) -> File {
        let inventory = inventory_file(
            text,
            FileRef {
                language,
                file_name: file_name.to_string(),
            },
        )
        .expect("test sources inventory cleanly");
        let targets: BTreeSet<String> = inventory
            .comments
            .iter()
            .filter(|comment| comment.editable && !comment.tags.contains("todo"))
            .map(|comment| comment.id.clone())
            .collect();
        let (chunks, _) =
            chunk_file(text, language, &inventory, max_chars, None).expect("chunking succeeds");
        let chunk_texts = build_chunk_texts(
            text,
            &inventory,
            adapter_for(language),
            &chunks,
            Mode::Remove,
            Some(&targets),
        )
        .expect("remove-mode chunk texts build");
        File {
            text: text.to_string(),
            language,
            inventory,
            targets,
            chunk_texts,
        }
    }

    /// One file in a single chunk.
    fn new(text: &str, language: LanguageId, file_name: &str) -> File {
        File::chunked(text, language, file_name, 1_000_000)
    }

    fn align(&self, citations: Vec<(usize, RemoveModeComment)>) -> Alignment {
        let first = align_remove_mode(
            &self.text,
            adapter_for(self.language),
            &self.inventory,
            &self.targets,
            &self.chunk_texts,
            citations.clone(),
        )
        .expect("alignment succeeds");
        let second = align_remove_mode(
            &self.text,
            adapter_for(self.language),
            &self.inventory,
            &self.targets,
            &self.chunk_texts,
            citations,
        )
        .expect("alignment succeeds");
        assert_eq!(first, second, "alignment is deterministic");
        first
    }

    /// The id of the record whose text contains `needle`.
    fn id(&self, needle: &str) -> String {
        self.inventory
            .comments
            .iter()
            .find(|comment| comment.text.contains(needle))
            .unwrap_or_else(|| panic!("no comment contains {needle:?}"))
            .id
            .clone()
    }

    /// The offset of the line start of the first line containing
    /// `needle`.
    fn line_of(&self, needle: &str) -> usize {
        let at = self.text.find(needle).expect("needle occurs");
        line_start_of(&self.text, at)
    }
}

fn cite(
    node_kind: &str,
    name: Option<&str>,
    snippet: &str,
    position: CitedPosition,
    comment: &str,
) -> RemoveModeComment {
    RemoveModeComment {
        anchor: Anchor {
            node_kind: node_kind.to_string(),
            name: name.map(str::to_string),
            snippet: snippet.to_string(),
        },
        position,
        comment: comment.to_string(),
        difficulty: Difficulty::Medium,
        rationale: "test".to_string(),
        missing_context: None,
    }
}

fn leading(node_kind: &str, name: Option<&str>, snippet: &str, comment: &str) -> RemoveModeComment {
    cite(node_kind, name, snippet, CitedPosition::Leading, comment)
}

fn trailing(
    node_kind: &str,
    name: Option<&str>,
    snippet: &str,
    comment: &str,
) -> RemoveModeComment {
    cite(node_kind, name, snippet, CitedPosition::Trailing, comment)
}

fn paired_ids(alignment: &Alignment) -> Vec<(&str, &str)> {
    alignment
        .pairs
        .iter()
        .map(|(id, comment)| (id.as_str(), comment.comment.as_str()))
        .collect()
}

fn warning_kinds(alignment: &Alignment) -> Vec<FileWarningKind> {
    alignment
        .warnings
        .iter()
        .map(|warning| warning.kind)
        .collect()
}

/// Asserts that the alignment produced no NEW candidate and exactly one
/// `invalid-insertion` warning whose detail mentions `reason`.
fn assert_dropped(alignment: &Alignment, reason: &str) {
    assert!(alignment.new_candidates.is_empty(), "{alignment:#?}");
    assert_eq!(
        warning_kinds(alignment),
        [FileWarningKind::InvalidInsertion]
    );
    assert!(
        alignment.warnings[0].detail.contains(reason),
        "{:?} does not mention {reason:?}",
        alignment.warnings[0].detail
    );
}

const TS_ADD: &str =
    "// Adds two numbers.\nfunction add(a: number, b: number) {\n    return a + b;\n}\n";

#[test]
fn an_exact_citation_pairs_with_its_original() {
    let file = File::new(TS_ADD, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading(
            "function_declaration",
            Some("add"),
            "function add(a: number, b: number) {\n    return a + b;\n}",
            "// Sum of the operands.",
        ),
    )]);
    // Pairing is by anchor; the citation's text replaces the original's, normalized on the way through (a wrapped line opener is stripped).
    assert_eq!(
        paired_ids(&alignment),
        [(file.id("Adds").as_str(), "Sum of the operands.")]
    );
    assert!(alignment.unmatched_originals.is_empty());
    assert!(alignment.new_candidates.is_empty());
    assert!(alignment.warnings.is_empty());
}

#[test]
fn different_names_never_pair_even_with_the_same_kind_and_snippet() {
    let file = File::new(TS_ADD, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading(
            "function_declaration",
            Some("sum"),
            "function add(a: number, b: number) {\n    return a + b;\n}",
            "Sum.",
        ),
    )]);
    assert!(alignment.pairs.is_empty());
    assert_eq!(alignment.unmatched_originals, [file.id("Adds")]);
    // It resolves by snippet, but its slot is the original's, so the citation is dropped and the original stays unmatched.
    assert_dropped(&alignment, "already holds a comment");
}

#[test]
fn an_unnamed_statement_aligns_by_snippet_alone() {
    let text = "x = compute()\n# Double it for the margin.\nresult = x * 2\n";
    let file = File::new(text, LanguageId::Python, "a.py");
    // The model's kind guess differs from the scanner's; statement-level
    // kinds pair on snippet overlap alone.
    let alignment = file.align(vec![(
        0,
        leading("assignment_statement", None, "result = x * 2", "Margin."),
    )]);
    assert_eq!(
        paired_ids(&alignment),
        [(file.id("Double").as_str(), "Margin.")]
    );
    assert!(alignment.warnings.is_empty());
}

#[test]
fn an_original_without_a_citation_is_unmatched() {
    let file = File::new(TS_ADD, LanguageId::Typescript, "a.ts");
    let alignment = file.align(Vec::new());
    assert!(alignment.pairs.is_empty());
    assert_eq!(alignment.unmatched_originals, [file.id("Adds")]);
    assert!(alignment.new_candidates.is_empty());
    assert!(alignment.warnings.is_empty());
}

#[test]
fn a_hallucinated_anchor_is_unresolvable() {
    let file = File::new(TS_ADD, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading(
            "function_declaration",
            Some("multiply"),
            "function multiply(a: number, b: number) {",
            "Product.",
        ),
    )]);
    assert!(alignment.pairs.is_empty());
    assert_eq!(alignment.unmatched_originals, [file.id("Adds")]);
    assert!(alignment.new_candidates.is_empty());
    assert_eq!(
        warning_kinds(&alignment),
        [FileWarningKind::UnresolvableAnchor]
    );
    assert!(alignment.warnings[0].detail.contains("multiply"));
}

const JAVA_TWO_RUNS: &str = "class A {\n    // Runs A.\n    public void run() {\n        alpha();\n    }\n}\n\nclass B {\n    // Runs B.\n    public void run() {\n        beta();\n    }\n}\n";

#[test]
fn an_ambiguous_anchor_resolves_by_snippet_then_document_order() {
    let file = File::new(JAVA_TWO_RUNS, LanguageId::Java, "A.java");
    // The full snippet singles out B's method.
    let alignment = file.align(vec![(
        0,
        leading(
            "method_declaration",
            Some("run"),
            "public void run() {\n        beta();\n    }",
            "Runs b.",
        ),
    )]);
    assert_eq!(
        paired_ids(&alignment),
        [(file.id("Runs B").as_str(), "Runs b.")]
    );
    assert_eq!(alignment.unmatched_originals, [file.id("Runs A")]);
    // A snippet shared by both resolves to the first in document order.
    let alignment = file.align(vec![(
        0,
        leading(
            "method_declaration",
            Some("run"),
            "public void run() {",
            "Runs.",
        ),
    )]);
    assert_eq!(
        paired_ids(&alignment),
        [(file.id("Runs A").as_str(), "Runs.")]
    );
    // Two such citations claim both.
    let alignment = file.align(vec![
        (
            0,
            leading(
                "method_declaration",
                Some("run"),
                "public void run() {",
                "One.",
            ),
        ),
        (
            0,
            leading(
                "method_declaration",
                Some("run"),
                "public void run() {",
                "Two.",
            ),
        ),
    ]);
    assert_eq!(alignment.pairs.len(), 2);
    assert!(alignment.unmatched_originals.is_empty());
}

#[test]
fn leading_and_trailing_citations_pair_by_position() {
    let text = "// a\nfoo(); // b\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    // The trailing citation comes first: position compatibility, not
    // citation order, decides.
    let alignment = file.align(vec![
        (0, trailing("expression_statement", None, "foo();", "B.")),
        (0, leading("expression_statement", None, "foo();", "A.")),
    ]);
    assert_eq!(
        paired_ids(&alignment),
        [
            (file.id("// a").as_str(), "A."),
            (file.id("// b").as_str(), "B.")
        ]
    );
    assert!(alignment.warnings.is_empty());
}

const PY_EOF: &str = "def first():\n    return 1\n\n\ndef second():\n    return 2\n\n# A dangling note at the end.\n";

#[test]
fn a_dangling_eof_original_pairs_only_with_an_end_of_file_citation_from_the_last_chunk() {
    let file = File::new(PY_EOF, LanguageId::Python, "a.py");
    let eof = file.id("dangling");
    assert_eq!(file.inventory.comments[0].anchor.node_kind, END_OF_FILE);
    let alignment = file.align(vec![(0, leading(END_OF_FILE, None, "", "The end."))]);
    assert_eq!(paired_ids(&alignment), [(eof.as_str(), "The end.")]);
    assert!(alignment.warnings.is_empty());

    // Chunked: the EOF original lives only in the last chunk, so the same citation is unresolvable from the first chunk.
    let file = File::chunked(PY_EOF, LanguageId::Python, "a.py", 30);
    assert!(file.chunk_texts.len() >= 2, "{:#?}", file.chunk_texts);
    let last = file.chunk_texts.len() - 1;
    let alignment = file.align(vec![(0, leading(END_OF_FILE, None, "", "Early."))]);
    assert!(alignment.pairs.is_empty());
    assert_eq!(alignment.unmatched_originals, std::slice::from_ref(&eof));
    assert_eq!(
        warning_kinds(&alignment),
        [FileWarningKind::UnresolvableAnchor]
    );
    let alignment = file.align(vec![(last, leading(END_OF_FILE, None, "", "Late."))]);
    assert_eq!(paired_ids(&alignment), [(eof.as_str(), "Late.")]);
}

#[test]
fn citations_resolve_only_inside_their_chunk() {
    let file = File::chunked(PY_EOF, LanguageId::Python, "a.py", 30);
    let second = file
        .chunk_texts
        .iter()
        .position(|chunk_text| chunk_text.prompt_text.contains("def second"))
        .expect("a chunk holds second()");
    assert_ne!(second, 0);
    let citation = || {
        leading(
            "function_definition",
            Some("second"),
            "def second():",
            "Second.",
        )
    };
    let alignment = file.align(vec![(0, citation())]);
    assert_eq!(
        warning_kinds(&alignment),
        [FileWarningKind::UnresolvableAnchor]
    );
    let alignment = file.align(vec![(second, citation())]);
    assert_eq!(alignment.new_candidates.len(), 1);
    assert_eq!(
        alignment.new_candidates[0].offset,
        file.line_of("def second")
    );
    assert_eq!(alignment.new_candidates[0].chunk, second);
}

#[test]
fn an_end_of_file_citation_never_becomes_a_new_comment() {
    let file = File::new("def f():\n    pass\n", LanguageId::Python, "a.py");
    let alignment = file.align(vec![(0, leading(END_OF_FILE, None, "", "The end."))]);
    assert_dropped(&alignment, "end_of_file");
}

#[test]
fn a_trailing_new_on_a_line_ending_in_a_comment_becomes_leading() {
    let text = "init();\nfoo(); // eslint-disable-line no-console\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        trailing("expression_statement", None, "foo();", "Why."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    let candidate = &alignment.new_candidates[0];
    assert_eq!(candidate.position, CitedPosition::Leading);
    assert_eq!(candidate.comment.position, CitedPosition::Leading);
    assert_eq!(candidate.offset, file.line_of("foo();"));
}

#[test]
fn a_trailing_new_lands_at_the_end_of_its_line_before_the_eol() {
    let text = "init();\r\nfoo();\r\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        trailing("expression_statement", None, "foo();", "Why."),
    )]);
    let candidate = &alignment.new_candidates[0];
    assert_eq!(candidate.position, CitedPosition::Trailing);
    assert_eq!(candidate.offset, text.rfind("\r\n").expect("crlf"));
}

#[test]
fn an_insertion_inside_a_multi_line_template_literal_is_dropped() {
    let text = "const s = `\n  hello\n  world`;\nfoo();\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading("expression_statement", None, "world`;", "Inside."),
    )]);
    assert_dropped(&alignment, "not a token gap");
    let alignment = file.align(vec![(
        0,
        trailing("expression_statement", None, "hello", "Inside."),
    )]);
    assert_dropped(&alignment, "not a token gap");
}

#[test]
fn a_leading_new_below_a_next_line_directive_is_dropped() {
    let text = "init();\n// eslint-disable-next-line no-console\nconsole.log(x);\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading("expression_statement", None, "console.log(x);", "Log it."),
    )]);
    assert_dropped(&alignment, "next-line directive");
}

#[test]
fn line_window_directives_exclude_their_window() {
    let text = "/* c8 ignore next 2 */\nfoo();\nbar();\nbaz();\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading("expression_statement", None, "bar();", "Bar."),
    )]);
    assert_dropped(&alignment, "line-window");
    let alignment = file.align(vec![(
        0,
        leading("expression_statement", None, "baz();", "Baz."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(alignment.new_candidates[0].offset, file.line_of("baz();"));
}

#[test]
fn a_new_above_import_c_is_dropped() {
    let text = "package p\n\nimport \"C\"\n\nfunc f() {}\n";
    let file = File::new(text, LanguageId::Go, "a.go");
    let alignment = file.align(vec![(
        0,
        leading("import_declaration", None, "import \"C\"", "Cgo."),
    )]);
    assert_dropped(&alignment, "import \"C\"");
    // Elsewhere in the same file, insertion is fine.
    let alignment = file.align(vec![(
        0,
        leading("function_declaration", Some("f"), "func f() {}", "F."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
}

#[test]
fn a_new_between_a_go_directive_and_its_declaration_is_dropped() {
    let text = "package p\n\n//go:noinline\nfunc f() {}\n";
    let file = File::new(text, LanguageId::Go, "a.go");
    let alignment = file.align(vec![(
        0,
        leading("function_declaration", Some("f"), "func f() {}", "F."),
    )]);
    assert_dropped(&alignment, "//go: directive");
}

#[test]
fn a_new_inside_a_go_example_output_block_is_dropped() {
    let text = "package p\n\nfunc ExampleF() {\n\tfmt.Println(\"x\")\n\t// Output:\n\t// x\n}\n";
    let file = File::new(text, LanguageId::Go, "a_test.go");
    let alignment = file.align(vec![(0, trailing("comment", None, "// x", "Inside."))]);
    assert_dropped(&alignment, "output block");
}

#[test]
fn a_leading_new_lands_above_python_decorators() {
    let text = "import functools\n\n\n@functools.cache\n@other\ndef f():\n    pass\n";
    let file = File::new(text, LanguageId::Python, "a.py");
    let alignment = file.align(vec![(
        0,
        leading("function_definition", Some("f"), "def f():", "F."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(
        alignment.new_candidates[0].offset,
        file.line_of("@functools.cache")
    );
}

#[test]
fn a_leading_new_lands_above_rust_attributes() {
    let text = "use std::fmt;\n\n#[derive(Debug)]\n#[allow(dead_code)]\npub struct S;\n";
    let file = File::new(text, LanguageId::Rust, "a.rs");
    let alignment = file.align(vec![(
        0,
        leading("struct_item", Some("S"), "pub struct S;", "S."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(alignment.new_candidates[0].offset, file.line_of("#[derive"));
}

#[test]
fn a_leading_new_lands_above_an_export_wrapper() {
    // Decorators before `export` start the `export_statement` a line
    // above the class.
    let text = "const a = 1;\n@dec\nexport class C {}\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading("class_declaration", Some("C"), "export class C {}", "C."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(alignment.new_candidates[0].offset, file.line_of("@dec"));
}

#[test]
fn a_new_for_a_declaration_with_a_kept_leading_comment_is_dropped() {
    // TODO-flagged prose (never a target), a functional directive and a
    // protected doc comment all occupy the slot.
    let cases = [
        (
            "# TODO: speed this up\ndef f():\n    pass\n",
            LanguageId::Python,
            "a.py",
            "function_definition",
            "def f():",
        ),
        (
            "// @ts-nocheck\nfunction f() {}\n",
            LanguageId::Typescript,
            "a.ts",
            "function_declaration",
            "function f() {}",
        ),
        (
            "/// Documented.\nfn f() {}\n",
            LanguageId::Rust,
            "a.rs",
            "function_item",
            "fn f() {}",
        ),
    ];
    for (text, language, file_name, kind, snippet) in cases {
        let file = File::new(text, language, file_name);
        assert!(file.targets.is_empty(), "{text:?}");
        let alignment = file.align(vec![(0, leading(kind, Some("f"), snippet, "F."))]);
        assert_dropped(&alignment, "already holds a comment");
    }
}

#[test]
fn a_new_at_a_slot_holding_a_target_is_dropped() {
    let text = "// Existing.\nfunction f() {}\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![
        (
            0,
            leading(
                "function_declaration",
                Some("f"),
                "function f() {}",
                "Paired.",
            ),
        ),
        (
            0,
            leading(
                "function_declaration",
                Some("f"),
                "function f() {}",
                "Extra.",
            ),
        ),
    ]);
    assert_eq!(
        paired_ids(&alignment),
        [(file.id("Existing").as_str(), "Paired.")]
    );
    assert_dropped(&alignment, "already holds a comment");
}

#[test]
fn a_trailing_new_on_a_line_holding_a_trailing_comment_is_dropped() {
    let text = "foo(/* why */ 1);\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        trailing("expression_statement", None, "foo(", "New."),
    )]);
    assert_dropped(&alignment, "already holds a comment");
}

#[test]
fn two_news_on_one_slot_keep_the_first_in_citation_order() {
    let text = "function f() {}\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![
        (
            0,
            leading(
                "function_declaration",
                Some("f"),
                "function f() {}",
                "First.",
            ),
        ),
        (
            0,
            leading(
                "function_declaration",
                Some("f"),
                "function f() {}",
                "Second.",
            ),
        ),
    ]);
    // The offset-0 boundary is a valid slot.
    assert_eq!(alignment.new_candidates.len(), 1);
    assert_eq!(alignment.new_candidates[0].offset, 0);
    assert_eq!(alignment.new_candidates[0].comment.comment, "First.");
    assert_dropped(
        &Alignment {
            new_candidates: Vec::new(),
            ..alignment
        },
        "earlier NEW",
    );
}

#[test]
fn citation_order_is_chunk_order_then_model_order() {
    let file = File::chunked(PY_EOF, LanguageId::Python, "a.py", 30);
    let second = file
        .chunk_texts
        .iter()
        .position(|chunk_text| chunk_text.prompt_text.contains("def second"))
        .expect("a chunk holds second()");
    let first = || {
        leading(
            "function_definition",
            Some("first"),
            "def first():",
            "First.",
        )
    };
    let other = || {
        leading(
            "function_definition",
            Some("second"),
            "def second():",
            "Second.",
        )
    };
    let alignment = file.align(vec![(second, other()), (0, first())]);
    let texts: Vec<&str> = alignment
        .new_candidates
        .iter()
        .map(|candidate| candidate.comment.comment.as_str())
        .collect();
    assert_eq!(texts, ["First.", "Second."]);
}

#[test]
fn a_shebang_line_is_never_preceded() {
    let text = "#!/usr/bin/env python3\ndef f():\n    pass\n";
    let file = File::new(text, LanguageId::Python, "a.py");
    let alignment = file.align(vec![(
        0,
        leading("function_definition", Some("f"), "def f():", "F."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(alignment.new_candidates[0].offset, file.line_of("def f"));
    for citation in [
        leading("comment", None, "#!/usr/bin/env python3", "Shebang."),
        trailing("comment", None, "#!/usr/bin/env python3", "Shebang."),
    ] {
        let alignment = file.align(vec![(0, citation)]);
        assert_dropped(&alignment, "shebang");
    }
}

#[test]
fn a_leading_new_on_line_one_goes_after_the_bom() {
    let text = "\u{FEFF}function f() {}\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    let alignment = file.align(vec![(
        0,
        leading("function_declaration", Some("f"), "function f() {}", "F."),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(alignment.new_candidates[0].offset, '\u{FEFF}'.len_utf8());
}

#[test]
fn a_clean_text_that_does_not_parse_degrades_to_snippet_resolution() {
    // The source itself has a syntax error, so its clean text does too, but the leading comment is still clean.
    let text = "// Starts things.\nfunction start() {\n    go(;\n}\n";
    let file = File::new(text, LanguageId::Typescript, "a.ts");
    // A name-only citation has nothing to match without declarations...
    let alignment = file.align(vec![(
        0,
        leading("function_declaration", Some("start"), "", "Start."),
    )]);
    assert_eq!(
        warning_kinds(&alignment),
        [FileWarningKind::UnresolvableAnchor]
    );
    // ...but its snippet still resolves it.
    let alignment = file.align(vec![(
        0,
        leading(
            "function_declaration",
            Some("start"),
            "function start() {",
            "Start.",
        ),
    )]);
    assert!(alignment.warnings.is_empty(), "{alignment:#?}");
    assert_eq!(
        paired_ids(&alignment),
        [(file.id("Starts").as_str(), "Start.")]
    );
}

#[test]
fn a_regeneration_naming_a_missing_chunk_is_an_invariant_error() {
    let file = File::new(TS_ADD, LanguageId::Typescript, "a.ts");
    let result = align_remove_mode(
        &file.text,
        adapter_for(file.language),
        &file.inventory,
        &file.targets,
        &file.chunk_texts,
        vec![(1, leading(END_OF_FILE, None, "", "x"))],
    );
    assert!(matches!(result, Err(Error::Invariant(_))), "{result:?}");
}
