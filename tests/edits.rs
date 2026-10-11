//! Edit computation and application (`apply::edits`): every decision type
//! against byte-exact expected output, line-break hazards, CRLF/BOM/shebang
//! preservation, the JSX backstop, skipped edits and the edit invariants.
//!
//! Artifacts are built with `common::file_plan_from_inventory`, deciding
//! each scanned record by its raw text.

mod common;

use std::path::Path;

use munge_comments::apply::edits::{ComputedEdits, EditOpts, apply_edits, compute_edits};
use munge_comments::apply::types::{SkippedEdit, SkippedEditReason};
use munge_comments::artifact::types::{CommentRecord, DecidedBy, Decision, FilePlan};
use munge_comments::comments::types::{Anchor, Classification, CommentKind, Edit, Position};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{Error, FileRef, Inventory, inventory_file, language_for};

use Decision::{Hybrid, KeepGenerated, KeepProtected, NeedsReview, New, Remove};

/// A file under test: its text, inventory and (hand-authored) plan.
struct Case {
    text: String,
    inventory: Inventory,
    plan: FilePlan,
}

impl Case {
    /// Plans `text` (language from `name`'s extension), deciding each
    /// scanned record whose raw text equals a `decisions` key; every other
    /// record is `KEEP_ORIGINAL`.
    fn new(name: &str, text: &str, decisions: &[(&str, Decision, Option<&str>)]) -> Case {
        let language = language_for(Path::new(name)).expect("known extension");
        let inventory = inventory_file(
            text,
            FileRef {
                language,
                file_name: name.to_owned(),
            },
        )
        .unwrap_or_else(|err| panic!("inventorying {name}: {err}"));
        for (raw, _, _) in decisions {
            assert!(
                inventory
                    .comments
                    .iter()
                    .any(|comment| comment.text == *raw),
                "{name}: no record {raw:?} in {:?}",
                inventory
                    .comments
                    .iter()
                    .map(|comment| &comment.text)
                    .collect::<Vec<_>>()
            );
        }
        let plan = common::file_plan_from_inventory(name, text, &inventory, language, |comment| {
            decisions
                .iter()
                .find(|(raw, _, _)| *raw == comment.text)
                .map_or((Decision::KeepOriginal, None), |(_, decision, text)| {
                    (*decision, text.map(str::to_owned))
                })
        });
        Case {
            text: text.to_owned(),
            inventory,
            plan,
        }
    }

    /// Appends a NEW-origin record `id` inserting `final_comment` at
    /// `offset`.
    fn with_new(
        mut self,
        id: &str,
        offset: usize,
        position: Position,
        final_comment: &str,
    ) -> Case {
        let kind = self.inventory.style_stats.prevailing_new();
        self.plan
            .comments
            .push(new_record(id, kind, offset, position, final_comment));
        self
    }

    fn compute(&self, allow_add: bool) -> Result<ComputedEdits, Error> {
        compute_edits(
            &self.text,
            &self.plan,
            &self.inventory,
            adapter_for(self.plan.language),
            EditOpts { allow_add },
        )
    }

    /// The edited text and the skipped edits, with `allowAdd` on.
    fn run(&self) -> (String, Vec<SkippedEdit>) {
        let computed = self
            .compute(true)
            .unwrap_or_else(|err| panic!("compute_edits on {:?}: {err}", self.text));
        let edited = apply_edits(&self.text, &computed.edits)
            .unwrap_or_else(|err| panic!("apply_edits on {:?}: {err}", self.text));
        (edited, computed.skipped)
    }

    /// The edited text, asserting nothing was skipped.
    fn edited(&self) -> String {
        let (edited, skipped) = self.run();
        assert!(skipped.is_empty(), "unexpected skips {skipped:?}");
        edited
    }
}

fn new_record(
    id: &str,
    kind: CommentKind,
    offset: usize,
    position: Position,
    final_comment: &str,
) -> CommentRecord {
    CommentRecord {
        id: id.to_owned(),
        kind,
        start: offset,
        end: offset,
        line: 1,
        column: 1,
        position,
        classification: Classification::Prose,
        tags: Vec::new(),
        original: None,
        anchor: Anchor {
            node_kind: "expression_statement".to_owned(),
            name: None,
            snippet: String::new(),
        },
        regenerated: Some(final_comment.to_owned()),
        would_you_comment_here: None,
        difficulty: None,
        difficulty_rationale: None,
        missing_context: None,
        decision: Decision::New,
        final_comment: Some(final_comment.to_owned()),
        confidence: 1.0,
        decision_rationale: None,
        decided_by: DecidedBy::Human,
    }
}

fn skip(id: &str, reason: SkippedEditReason) -> SkippedEdit {
    SkippedEdit {
        id: id.to_owned(),
        reason,
    }
}

// ---------------------------------------------------------------------
// Keep decisions
// ---------------------------------------------------------------------

#[test]
fn keep_decisions_make_no_edit() {
    let text = "// a\nconst x = 1; // b\n/* c */\nfoo();\n";
    for decision in [Decision::KeepOriginal, NeedsReview, KeepProtected] {
        let case = Case::new(
            "keep.js",
            text,
            &[
                ("// a", decision, Some("ignored")),
                ("// b", decision, None),
                ("/* c */", decision, None),
            ],
        );
        let computed = case.compute(true).expect("computes");
        assert_eq!(computed, ComputedEdits::default(), "{decision:?}");
        assert_eq!(case.edited(), text, "{decision:?}");
    }
}

// ---------------------------------------------------------------------
// REMOVE
// ---------------------------------------------------------------------

#[test]
fn remove_leading_comment_deletes_its_line() {
    let case = Case::new(
        "remove.js",
        "function f() {\n  // c\n  return 1;\n}\n",
        &[("// c", Remove, None)],
    );
    assert_eq!(case.edited(), "function f() {\n  return 1;\n}\n");
}

#[test]
fn remove_trailing_comment_takes_the_whitespace_before_it() {
    let case = Case::new(
        "remove.js",
        "foo();   // c\nbar();\n",
        &[("// c", Remove, None)],
    );
    assert_eq!(case.edited(), "foo();\nbar();\n");
}

#[test]
fn remove_trailing_comment_with_trailing_whitespace() {
    // A line comment runs to the end of its line, trailing whitespace
    // included, so nothing of the comment line survives after the code.
    let text = "foo(); // c   \nbar();\n";
    let case = Case::new("remove.js", text, &[("// c   ", Remove, None)]);
    assert_eq!(case.edited(), "foo();\nbar();\n");
}

#[test]
fn several_removals_on_one_file_are_judged_together() {
    let case = Case::new(
        "remove.js",
        "// a\n\n// b\n\nfoo(); /* c */ // d\n",
        &[
            ("// a", Remove, None),
            ("// b", Remove, None),
            ("/* c */", Remove, None),
            ("// d", Remove, None),
        ],
    );
    assert_eq!(case.edited(), "\nfoo();\n");
}

#[test]
fn inline_block_ts_removals() {
    let text = include_str!("fixtures/golden/typescript/inline-block.ts");
    let case = Case::new(
        "inline-block.ts",
        text,
        &[("/* c */", Remove, None), ("/**/", Remove, None)],
    );
    assert_eq!(
        case.edited(),
        "declare const a: number;\ndeclare const b: number;\nexport const sum = a + b;\n\n\
         export function f(x: number): number {\n  return x;\n}\n\nlet y = 1;\nexport { y };\n"
    );
}

#[test]
fn inline_block_ts_rewrites() {
    let text = include_str!("fixtures/golden/typescript/inline-block.ts");
    let case = Case::new(
        "inline-block.ts",
        text,
        &[
            ("/* c */", KeepGenerated, Some("the sum")),
            ("/**/", Hybrid, Some("binding")),
        ],
    );
    assert_eq!(
        case.edited(),
        "declare const a: number;\ndeclare const b: number;\nexport const sum = a /* the sum */ + b;\n\n\
         export function f(x: number): number {\n  return /* the sum */ x;\n}\n\n\
         let/* binding */y = 1;\nexport { y };\n"
    );
}

// ---------------------------------------------------------------------
// Line-break hazards
// ---------------------------------------------------------------------
// Removing or rewriting a comment must not join lines in a way that changes ASI or statement boundaries.

/// `asi-hazard.ts`: removing either comment keeps the line break between
/// `return` and `x`, so ASI still ends the `return` statement.
#[test]
fn line_break_hazards_survive_removal() {
    let text = include_str!("fixtures/golden/typescript/asi-hazard.ts");
    let case = Case::new(
        "asi-hazard.ts",
        text,
        &[("// c", Remove, None), ("/*\n  */", Remove, None)],
    );
    assert_eq!(
        case.edited(),
        "export function lineComment(x: number): number | undefined {\n  return\n    x;\n}\n\n\
         export function blockComment(x: number): number | undefined {\n  return\n  x;\n}\n"
    );
}

/// `asi-hazard.ts` rewritten: the trailing line slot keeps its own line
/// terminator (outside its range); the inline block slot whose span held
/// a line break is followed by one EOL.
#[test]
fn line_break_hazards_survive_rewrite() {
    let text = include_str!("fixtures/golden/typescript/asi-hazard.ts");
    let case = Case::new(
        "asi-hazard.ts",
        text,
        &[
            ("// c", KeepGenerated, Some("first\nsecond")),
            ("/*\n  */", Hybrid, Some("one\ntwo")),
        ],
    );
    assert_eq!(
        case.edited(),
        "export function lineComment(x: number): number | undefined {\n  return // first second\n    x;\n}\n\n\
         export function blockComment(x: number): number | undefined {\n  return /* one two */\n x;\n}\n"
    );
}

/// The Go analog: a multi-line general comment acts as a newline.
#[test]
fn go_multi_line_comment_keeps_its_line_break() {
    let text = "package p\n\nvar i = 0 /*\n*/ var j = 1\n";
    let removed = Case::new("hazard.go", text, &[("/*\n*/", Remove, None)]);
    assert_eq!(removed.edited(), "package p\n\nvar i = 0\nvar j = 1\n");
    let rewritten = Case::new("hazard.go", text, &[("/*\n*/", KeepGenerated, Some("gap"))]);
    assert_eq!(
        rewritten.edited(),
        "package p\n\nvar i = 0 /* gap */\n var j = 1\n"
    );
}

// ---------------------------------------------------------------------
// KEEP_GENERATED / HYBRID layouts
// ---------------------------------------------------------------------
// A rewritten comment keeps its slot's indentation and opener, and multi-line text is re-laid out for the comment kind.

#[test]
fn rewrite_line_group_keeps_indent_and_opener() {
    let case = Case::new(
        "group.rs",
        "fn f() {\n    // a\n    // b\n    g();\n}\n",
        &[("// a\n    // b", KeepGenerated, Some("x\n\ny"))],
    );
    assert_eq!(
        case.edited(),
        "fn f() {\n    // x\n    //\n    // y\n    g();\n}\n"
    );
}

#[test]
fn rewrite_block_multi_line_layout() {
    let case = Case::new(
        "block.js",
        "  /* old */\n  foo();\n",
        &[("/* old */", Hybrid, Some("first\nsecond"))],
    );
    assert_eq!(
        case.edited(),
        "  /*\n   * first\n   * second\n   */\n  foo();\n"
    );
}

#[test]
fn rewrite_normalizes_final_comment_before_rendering() {
    // A hand-authored finalComment carrying its own opener, and a bare
    // `\r` line break, render as two clean lines rather than `// // …`.
    let case = Case::new(
        "norm.js",
        "// old\nfoo();\n",
        &[("// old", KeepGenerated, Some("// one\r// two"))],
    );
    assert_eq!(case.edited(), "// one\n// two\nfoo();\n");
}

#[test]
fn python_hash_slots_render_through_compute_edits() {
    let text = "def f():\n    # a\n    # b\n    return 1  # c\n\n#: doc\nX = 1\n";
    let case = Case::new(
        "slots.py",
        text,
        &[
            ("# a\n    # b", KeepGenerated, Some("x\ny")),
            ("# c", Hybrid, Some("the one\nvalue")),
            ("#: doc", KeepGenerated, Some("the constant")),
        ],
    );
    assert_eq!(
        case.edited(),
        "def f():\n    # x\n    # y\n    return 1  # the one value\n\n#: the constant\nX = 1\n"
    );
}

// ---------------------------------------------------------------------
// Byte preservation: CRLF, BOM, shebang
// ---------------------------------------------------------------------
// CRLF stays CRLF, and a BOM or shebang is preserved while edits are placed after it.

#[test]
fn crlf_stays_crlf() {
    let text = "// a\r\n// b\r\nfoo(); // c\r\n/* d */\r\nbar();\r\n";
    let case = Case::new(
        "crlf.js",
        text,
        &[
            ("// a\r\n// b", KeepGenerated, Some("x\ny")),
            ("// c", Remove, None),
            ("/* d */", Hybrid, Some("p\nq")),
        ],
    )
    .with_new(
        "N1",
        text.find("bar").expect("bar"),
        Position::Leading,
        "added",
    );
    assert_eq!(
        case.edited(),
        "// x\r\n// y\r\nfoo();\r\n/*\r\n * p\r\n * q\r\n */\r\n// added\r\nbar();\r\n"
    );
}

#[test]
fn bom_and_shebang_are_preserved() {
    let bom = "\u{FEFF}// a\nfoo();\n";
    let removed = Case::new("bom.ts", bom, &[("// a", Remove, None)]);
    assert_eq!(removed.edited(), "\u{FEFF}foo();\n");
    let rewritten = Case::new("bom.ts", bom, &[("// a", KeepGenerated, Some("b"))]);
    assert_eq!(rewritten.edited(), "\u{FEFF}// b\nfoo();\n");
    let inserted = Case::new("bom.ts", bom, &[("// a", Remove, None)]).with_new(
        "N1",
        '\u{FEFF}'.len_utf8(),
        Position::Leading,
        "new",
    );
    assert_eq!(inserted.edited(), "\u{FEFF}// new\nfoo();\n");

    let shebang = "#!/usr/bin/env node\n// a\nfoo(); // b\n";
    let case = Case::new(
        "tool.js",
        shebang,
        &[
            ("// a", Remove, None),
            ("// b", KeepGenerated, Some("call")),
        ],
    );
    assert_eq!(case.edited(), "#!/usr/bin/env node\nfoo(); // call\n");
}

// ---------------------------------------------------------------------
// Skipped edits
// ---------------------------------------------------------------------
// A JSX container comment, an invalid final comment, or a new comment without allow_add is skipped rather than fatal.

#[test]
fn jsx_container_comment_is_never_edited() {
    let text = "const a = <div>{/* sole */}</div>; // b\n";
    for decision in [Remove, KeepGenerated, Hybrid] {
        let case = Case::new(
            "a.tsx",
            text,
            &[
                ("/* sole */", decision, Some("other")),
                ("// b", Remove, None),
            ],
        );
        let (edited, skipped) = case.run();
        assert_eq!(
            edited, "const a = <div>{/* sole */}</div>;\n",
            "{decision:?}"
        );
        assert_eq!(
            skipped,
            [skip("C1", SkippedEditReason::JsxExpression)],
            "{decision:?}"
        );
    }
}

#[test]
fn block_closer_in_a_block_slot_is_skipped_not_fatal() {
    let case = Case::new(
        "close.js",
        "/* a */\nfoo(); // b\n",
        &[
            ("/* a */", KeepGenerated, Some("ends */ early")),
            ("// b", KeepGenerated, Some("ends */ fine")),
        ],
    );
    let (edited, skipped) = case.run();
    assert_eq!(edited, "/* a */\nfoo(); // ends */ fine\n");
    assert_eq!(
        skipped,
        [skip("C1", SkippedEditReason::FinalCommentInvalid)]
    );
}

#[test]
fn functional_new_comment_is_skipped() {
    let text = "foo();\n";
    let case = Case::new("new.ts", text, &[]).with_new("N1", 0, Position::Leading, "// @ts-ignore");
    let (edited, skipped) = case.run();
    assert_eq!(edited, text);
    assert_eq!(
        skipped,
        [skip("N1", SkippedEditReason::FinalCommentInvalid)]
    );
}

#[test]
fn new_without_allow_add_is_skipped() {
    let case = Case::new("new.js", "foo();\n", &[]).with_new("N1", 0, Position::Leading, "call");
    let computed = case.compute(false).expect("computes");
    assert!(computed.edits.is_empty());
    assert_eq!(
        computed.skipped,
        [skip("N1", SkippedEditReason::AllowAddOff)]
    );
}

#[test]
fn held_new_record_makes_no_edit() {
    let mut case =
        Case::new("new.js", "foo();\n", &[]).with_new("N1", 0, Position::Leading, "call");
    case.plan.comments[0].decision = NeedsReview;
    assert_eq!(
        case.compute(true).expect("computes"),
        ComputedEdits::default()
    );
}

// ---------------------------------------------------------------------
// NEW insertions
// ---------------------------------------------------------------------
// Leading insertions get their own line, trailing insertions share the line, and ordering at one offset is by record id.

#[test]
fn new_leading_and_trailing_insertions() {
    let text = "function f() {\n    foo();\n    bar();\n}\n";
    let foo = text.find("foo").expect("foo");
    let bar_line = text.find("    bar").expect("bar");
    let bar_eol = text.find("bar();").expect("bar") + "bar();".len();
    let case = Case::new("new.js", text, &[])
        .with_new("N1", foo - 4, Position::Leading, "calls foo\nfirst")
        .with_new("N2", bar_eol, Position::Trailing, "then\nbar")
        .with_new("N3", bar_line, Position::Leading, "second");
    assert_eq!(
        case.edited(),
        "function f() {\n    // calls foo\n    // first\n    foo();\n    // second\n    bar(); // then bar\n}\n"
    );
}

#[test]
fn trailing_new_after_non_whitespace_gets_a_gap_only_when_needed() {
    let case = Case::new("new.py", "x = 1 \ny = 2\n", &[])
        .with_new("N1", "x = 1 ".len(), Position::Trailing, "one")
        .with_new("N2", "x = 1 \ny = 2".len(), Position::Trailing, "two");
    assert_eq!(case.edited(), "x = 1 # one\ny = 2 # two\n");
}

#[test]
fn insertions_at_one_offset_are_ordered_by_record_id() {
    let case = Case::new("new.js", "foo();\n", &[])
        .with_new("N10", 0, Position::Leading, "ten")
        .with_new("N2", 0, Position::Leading, "two");
    assert_eq!(case.edited(), "// two\n// ten\nfoo();\n");
}

#[test]
fn new_and_remove_at_the_same_offset() {
    let text = "// old\nfoo();\n";
    // At the start of the removed line: the insertion precedes the
    // deletion.
    let at_start = Case::new("new.js", text, &[("// old", Remove, None)]).with_new(
        "N1",
        0,
        Position::Leading,
        "new",
    );
    assert_eq!(at_start.edited(), "// new\nfoo();\n");
    let computed = at_start.compute(true).expect("computes");
    assert_eq!(
        computed.edits,
        [
            Edit {
                start: 0,
                length: 0,
                replacement: "// new\n".to_owned(),
            },
            Edit {
                start: 0,
                length: "// old\n".len(),
                replacement: String::new(),
            },
        ]
    );
    // At the end of the deleted range: also legal.
    let at_end = Case::new("new.js", text, &[("// old", Remove, None)]).with_new(
        "N1",
        "// old\n".len(),
        Position::Leading,
        "new",
    );
    assert_eq!(at_end.edited(), "// new\nfoo();\n");
}

#[test]
fn new_inside_whitespace_a_trailing_remove_deletes_is_an_artifact_error() {
    let text = "foo();   // c\n";
    let case = Case::new("new.js", text, &[("// c", Remove, None)]).with_new(
        "N1",
        "foo(); ".len(),
        Position::Trailing,
        "inside",
    );
    let err = case
        .compute(true)
        .expect_err("an insertion inside a deletion");
    assert!(matches!(err, Error::Artifact(_)), "{err:?}");
    assert!(err.to_string().contains("N1"), "{err}");
}

// ---------------------------------------------------------------------
// Mixed decisions and edit invariants
// ---------------------------------------------------------------------
// Rewrites and removals on one line are computed so their ranges do not overlap.

#[test]
fn rewrite_and_remove_on_one_line_do_not_overlap() {
    let case = Case::new(
        "mixed.js",
        "let x = 1 /* a */ // b\nfoo();\n",
        &[
            ("/* a */", KeepGenerated, Some("one")),
            ("// b", Remove, None),
        ],
    );
    let computed = case.compute(true).expect("no overlap");
    assert_eq!(computed.edits.len(), 2);
    assert_eq!(case.edited(), "let x = 1 /* one */\nfoo();\n");
}

#[test]
fn overlapping_edits_are_an_invariant_error() {
    let text = "abcdef";
    let edit = |start, length| Edit {
        start,
        length,
        replacement: "X".to_owned(),
    };
    for edits in [
        vec![edit(0, 3), edit(2, 2)],
        vec![edit(2, 2), edit(2, 1)],
        vec![edit(1, 3), edit(2, 0)],
    ] {
        let err = apply_edits(text, &edits).expect_err("overlap");
        assert!(matches!(err, Error::Invariant(_)), "{edits:?}: {err:?}");
    }
    // Touching edits and an insertion at either boundary are fine; non-overlapping edits are applied left to right, and zero-length insertions at the same offset keep their relative order.
    assert_eq!(
        apply_edits(text, &[edit(3, 0), edit(1, 2), edit(3, 2), edit(1, 0)]).expect("no overlap"),
        "aXXXXf"
    );
}

#[test]
fn edit_endpoints_must_be_in_bounds_and_on_char_boundaries() {
    let text = "a\u{e9}b";
    for (start, length) in [(2, 0), (1, 1), (0, 9), (usize::MAX, 2)] {
        let err = apply_edits(
            text,
            &[Edit {
                start,
                length,
                replacement: String::new(),
            }],
        )
        .expect_err("bad endpoint");
        assert!(
            matches!(err, Error::Invariant(_)),
            "{start}+{length}: {err:?}"
        );
    }
}

#[test]
fn records_that_do_not_fit_the_text_are_artifact_errors() {
    let text = "foo(); // c\n";
    let shifted = {
        let mut case = Case::new("bad.js", text, &[("// c", Remove, None)]);
        case.plan.comments[0].start += 1;
        case
    };
    let scanned_new = Case::new("bad.js", text, &[("// c", New, Some("x"))]);
    let removed_new = {
        let mut case = Case::new("bad.js", text, &[]).with_new("N1", 0, Position::Leading, "x");
        case.plan.comments[1].decision = Remove;
        case
    };
    let missing_final = Case::new("bad.js", text, &[("// c", KeepGenerated, None)]);
    let new_off_boundary =
        Case::new("bad.js", "\u{e9};\n", &[]).with_new("N1", 1, Position::Leading, "x");
    for (case, id) in [
        (shifted, "C1"),
        (scanned_new, "C1"),
        (removed_new, "N1"),
        (missing_final, "C1"),
        (new_off_boundary, "N1"),
    ] {
        let err = case.compute(true).expect_err("does not fit");
        assert!(matches!(err, Error::Artifact(_)), "{err:?}");
        assert!(err.to_string().contains(id), "{err}");
    }
}
