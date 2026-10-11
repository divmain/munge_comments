//! The semantic verifier (`apply::verify`): the five checks against
//! correct edits (which must pass) and against raw-text edits that change
//! code, line-break structure or the functional-comment sequence (which
//! must fail), the golden gap-residue snapshot, and near-limit depth
//! safety.
//!
//! Correct edits are produced by the real edit engine
//! (`compute_edits`/`apply_edits`) over plans built with
//! `common::file_plan_from_inventory`; hazard edits are raw text and
//! bypass it.

mod common;

use std::fs;
use std::path::Path;

use munge_comments::apply::edits::{EditOpts, apply_edits, compute_edits};
use munge_comments::apply::verify::{
    FunctionalComment, functional_sequence, gap_residues, verify_semantic_equality,
};
use munge_comments::artifact::types::Decision;
use munge_comments::comments::types::Classification;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::lang::types::MAX_TREE_DEPTH;
use munge_comments::{Error, FileRef, InventoryComment, inventory_file, language_for};

/// The file reference for `name` (language from its extension).
fn file_ref(name: &str) -> FileRef {
    FileRef {
        language: language_for(Path::new(name)).expect("known extension"),
        file_name: name.to_owned(),
    }
}

/// The ordered functional comments of `text`, as the firewall's
/// `inventory_file` run yields them.
fn functional(name: &str, text: &str) -> Vec<FunctionalComment> {
    functional_sequence(
        &inventory_file(text, file_ref(name))
            .unwrap_or_else(|err| panic!("inventorying {name}: {err}")),
    )
}

/// Verifies a raw-text edit of `original` into `edited`.
fn verify(name: &str, original: &str, edited: &str) -> Result<(), Error> {
    verify_semantic_equality(
        original,
        edited,
        file_ref(name),
        &functional(name, original),
    )
}

/// The reason of a verification failure; panics on success or on any
/// other error.
fn rejection(result: Result<(), Error>) -> String {
    match result {
        Err(Error::Verify(reason)) => reason,
        other => panic!("expected a verification failure, got {other:?}"),
    }
}

/// Removes, through the edit engine, every record whose raw text is in
/// `removed`, and returns the edited text.
fn remove(name: &str, text: &str, removed: &[&str]) -> String {
    let inventory = inventory_file(text, file_ref(name))
        .unwrap_or_else(|err| panic!("inventorying {name}: {err}"));
    for raw in removed {
        assert!(
            inventory
                .comments
                .iter()
                .any(|comment| comment.text == *raw),
            "{name}: no record {raw:?}"
        );
    }
    remove_where(name, text, |comment| {
        removed.contains(&comment.text.as_str())
    })
}

/// Removes, through the edit engine, every record satisfying `pick`, and
/// returns the edited text.
fn remove_where(name: &str, text: &str, pick: impl Fn(&InventoryComment) -> bool) -> String {
    edit_where(name, text, |comment| {
        if pick(comment) {
            (Decision::Remove, None)
        } else {
            (Decision::KeepOriginal, None)
        }
    })
}

/// Applies, through the edit engine, the decision `decide` makes for each
/// record, and returns the edited text.
fn edit_where(
    name: &str,
    text: &str,
    decide: impl Fn(&InventoryComment) -> (Decision, Option<String>),
) -> String {
    let file = file_ref(name);
    let inventory = inventory_file(text, file.clone())
        .unwrap_or_else(|err| panic!("inventorying {name}: {err}"));
    let plan = common::file_plan_from_inventory(name, text, &inventory, file.language, decide);
    let computed = compute_edits(
        text,
        &plan,
        &inventory,
        adapter_for(file.language),
        EditOpts { allow_add: false },
    )
    .unwrap_or_else(|err| panic!("compute_edits on {name}: {err}"));
    assert!(
        computed.skipped.is_empty(),
        "unexpected skips {:?}",
        computed.skipped
    );
    apply_edits(text, &computed.edits).unwrap_or_else(|err| panic!("apply_edits on {name}: {err}"))
}

/// Removes `removed` through the edit engine and verifies the result.
fn verify_removal(name: &str, text: &str, removed: &[&str]) -> Result<(), Error> {
    let edited = remove(name, text, removed);
    assert_ne!(edited, text, "the removal must change the text");
    verify(name, text, &edited)
}

#[test]
fn an_edited_text_that_does_not_parse_cleanly_fails() {
    let reason = rejection(verify("a.js", "let x = 1; /* c */\n", "let x = ;\n"));
    assert!(reason.contains("does not parse cleanly"), "{reason}");
}

#[test]
fn a_too_deep_edited_text_fails_as_verification() {
    let edited = nested(2 * MAX_TREE_DEPTH);
    let reason = rejection(verify("a.js", "let x = 1 /* Inner. */;\n", &edited));
    assert!(reason.contains("maximum tree depth"), "{reason}");
}

#[test]
fn an_unchanged_text_verifies() {
    let text = "fn main() {\n    // c\n    let x = 1;\n}\n";
    verify("a.rs", text, text).expect("identical texts verify");
}

/// Every golden fixture file, sorted.
fn golden_paths() -> Vec<std::path::PathBuf> {
    let root = common::fixture_path("golden");
    let mut paths = Vec::new();
    for directory in fs::read_dir(&root).expect("tests/fixtures/golden is readable") {
        let directory = directory.expect("golden/ is listable").path();
        for file in fs::read_dir(&directory).expect("a golden directory is readable") {
            paths.push(file.expect("a golden directory is listable").path());
        }
    }
    paths.sort();
    paths
}

/// Whether the edit engine may remove `comment`: editable prose.
fn editable(comment: &InventoryComment) -> bool {
    comment.classification == Classification::Prose && !comment.tags.contains("jsx-expression")
}

// Golden-fixture edits the verifier rejects although the edit is correct (see the list's documentation): a shared table of (fixture, comment text, reason prefix) triples the golden sweep consults, living in `common` so the fixture tests and the verifier agree on one source of truth.
use common::EXPECTED_VERIFIER_REJECTIONS as EXPECTED_REJECTIONS;

/// The fixture key (`language/file`) of a golden path.
fn golden_key(path: &Path) -> String {
    let directory = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let file = path.file_name().unwrap().to_str().unwrap();
    format!("{directory}/{file}")
}

/// The expected rejection reason for removing `comment` from `key`.
fn expected_rejection(key: &str, comment: &InventoryComment) -> Option<&'static str> {
    EXPECTED_REJECTIONS
        .iter()
        .find(|(fixture, text, _)| *fixture == key && *text == comment.text)
        .map(|(_, _, reason)| *reason)
}

#[test]
fn removing_editable_comments_verifies_on_every_golden_fixture() {
    let mut rejections_seen = 0;
    for path in golden_paths() {
        let key = golden_key(&path);
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let text = fs::read_to_string(&path).unwrap();
        let inventory = inventory_file(&text, file_ref(&name)).unwrap();
        // Each editable comment is removed on its own: the table says whether that removal must verify or must be rejected, and with which reason prefix.
        for comment in inventory
            .comments
            .iter()
            .filter(|comment| editable(comment))
        {
            let edited = remove_where(&name, &text, |other| other.id == comment.id);
            let result = verify(&name, &text, &edited);
            match expected_rejection(&key, comment) {
                None => {
                    if let Err(err) = result {
                        panic!(
                            "{key}: removing {:?}: {err}\n--- edited ---\n{edited}",
                            comment.text
                        );
                    }
                }
                Some(prefix) => {
                    let reason = rejection(result);
                    assert!(reason.starts_with(prefix), "{key}: {reason}");
                    rejections_seen += 1;
                }
            }
        }
        // Removing every editable comment that is not expected to be rejected must still verify: the rejections are local to a comment, not cumulative.
        let edited = remove_where(&name, &text, |comment| {
            editable(comment) && expected_rejection(&key, comment).is_none()
        });
        if let Err(err) = verify(&name, &text, &edited) {
            panic!("{key}: removing every editable comment: {err}\n--- edited ---\n{edited}");
        }
    }
    assert_eq!(rejections_seen, EXPECTED_REJECTIONS.len());
}

#[test]
fn golden_gap_residues_match_their_snapshots() {
    insta::glob!("fixtures/golden/*/*", |path| {
        let name = path.file_name().unwrap().to_str().unwrap();
        let text = fs::read_to_string(path).unwrap();
        let residues = gap_residues(&text, file_ref(name).language)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        insta::assert_json_snapshot!(residues);
    });
}

// ---- Gap computation -------------------------------------------------

#[test]
fn removing_the_comment_after_a_rust_shebang_verifies() {
    // tree-sitter-rust's `shebang` token includes its `\n`: the boundary
    // keeps its line break through that token's own trailing terminator.
    let text = "#!/usr/bin/env rust-script\n// Prose on line 2.\nfn main() {}\n";
    let edited = remove("main.rs", text, &["// Prose on line 2."]);
    assert_eq!(edited, "#!/usr/bin/env rust-script\nfn main() {}\n");
    verify("main.rs", text, &edited).expect("the shebang boundary keeps its line break");
}

#[test]
fn changing_raw_string_delimiters_fails_on_residue() {
    // The delimiters sit in no leaf: only the residue check sees them.
    let original = "fn main() {\n    let s = r#\"x\"#;\n}\n";
    let edited = "fn main() {\n    let s = r##\"x\"##;\n}\n";
    let reason = rejection(verify("main.rs", original, edited));
    assert!(reason.starts_with("uncovered text changed"), "{reason}");
}

#[test]
fn removing_the_comment_beside_a_c_define_verifies() {
    // `preproc_arg` absorbs the whitespace before the comment (`2 `).
    let text = "#define TWO 2 /* Two. */\nint x = TWO;\n";
    let edited = remove("a.c", text, &["/* Two. */"]);
    assert_eq!(edited, "#define TWO 2\nint x = TWO;\n");
    verify("a.c", text, &edited).expect("the token edge whitespace belongs to the gap");
}

#[test]
fn changing_string_edge_whitespace_fails() {
    // Leading whitespace inside a string literal is part of the token, so dropping it is a token-stream mismatch rather than a gap change.
    let original = "const s = \" a\"; // c\n";
    let edited = "const s = \"a\"; // c\n";
    let reason = rejection(verify("a.ts", original, edited));
    assert!(reason.starts_with("token-stream mismatch"), "{reason}");
}

// ---- Functional-sequence check ---------------------------------------

#[test]
fn removing_prose_above_a_triple_slash_directive_verifies() {
    let text = "// Prose.\n/// <reference path=\"./globals.d.ts\" />\nexport const x = 1;\n";
    verify_removal("a.ts", text, &["// Prose."]).expect("the directive survives");
}

#[test]
fn removing_prose_above_a_go_build_constraint_verifies() {
    let text = "// Prose.\n//go:build linux\n\npackage tags\n";
    verify_removal("tags.go", text, &["// Prose."]).expect("the constraint survives");
}

#[test]
fn deleting_or_altering_a_directive_fails() {
    let typescript = "/// <reference path=\"./globals.d.ts\" />\nexport const x = 1;\n";
    for edited in [
        "export const x = 1;\n",
        "/// <reference path=\"./other.d.ts\" />\nexport const x = 1;\n",
    ] {
        let reason = rejection(verify("a.ts", typescript, edited));
        assert!(
            reason.starts_with("functional-comment mismatch"),
            "{reason}"
        );
    }
    let go = "//go:build linux\n\npackage tags\n";
    for edited in ["\npackage tags\n", "//go:build darwin\n\npackage tags\n"] {
        let reason = rejection(verify("tags.go", go, edited));
        assert!(
            reason.starts_with("functional-comment mismatch"),
            "{reason}"
        );
    }
}

#[test]
fn introducing_a_directive_fails() {
    let python = "x = f()  # A note.\n";
    let reason = rejection(verify("a.py", python, "x = f()  # type: ignore\n"));
    assert!(
        reason.starts_with("functional-comment mismatch"),
        "{reason}"
    );
    let javascript = "f(); // A note.\n";
    let reason = rejection(verify("a.js", javascript, "f(); // eslint-disable-line\n"));
    assert!(
        reason.starts_with("functional-comment mismatch"),
        "{reason}"
    );
}

#[test]
fn moving_a_dormant_encoding_cookie_onto_line_2_fails() {
    // Line 3 is too late for a PEP 263 cookie; after the removal it sits
    // on line 2 under a comment line, where CPython honors it.
    let original = "# A note.\n# Another note.\n# -*- coding: latin-1 -*-\nVALUE = 1\n";
    let edited = "# Another note.\n# -*- coding: latin-1 -*-\nVALUE = 1\n";
    let reason = rejection(verify("a.py", original, edited));
    assert!(
        reason.starts_with("functional-comment mismatch"),
        "{reason}"
    );
    // The same activation with line 1 left blank.
    let original = "\n# A note.\n# -*- coding: latin-1 -*-\nVALUE = 1\n";
    let edited = "\n# -*- coding: latin-1 -*-\nVALUE = 1\n";
    let reason = rejection(verify("a.py", original, edited));
    assert!(
        reason.starts_with("functional-comment mismatch"),
        "{reason}"
    );
}

#[test]
fn activating_a_functional_modeline_as_an_encoding_cookie_fails() {
    // The vim modeline is functional on line 3 too, so its text is in both
    // functional sequences; only its gained `encoding-cookie` role shows
    // that CPython would now decode the file as Latin-1.
    let original =
        "# A note.\n# Another note.\n# vim: set fileencoding=latin-1 :\nVALUE = 'café'\n";
    let edited = remove("a.py", original, &["# A note.\n# Another note."]);
    assert_eq!(
        edited,
        "# vim: set fileencoding=latin-1 :\nVALUE = 'café'\n"
    );
    let reason = rejection(verify("a.py", original, &edited));
    assert!(
        reason.starts_with("functional-comment mismatch") && reason.contains("encoding-cookie"),
        "{reason}"
    );
}

#[test]
fn a_string_literal_containing_a_directive_never_counts() {
    let text = "// Prose.\nconst s = \"// @ts-ignore\";\n";
    assert!(functional("a.ts", text).is_empty());
    verify_removal("a.ts", text, &["// Prose."]).expect("string contents are not comments");
}

#[test]
fn go_example_output_blocks_are_checked_both_ways() {
    let text = "package greet_test\n\nimport \"fmt\"\n\nfunc ExampleHello() {\n\
                \t// Print the greeting.\n\tfmt.Println(\"hello\")\n\t// Output: hello\n}\n";
    verify_removal("greet_test.go", text, &["// Print the greeting."])
        .expect("removing prose above the output block verifies");
    let edited = text.replace("\t// Output: hello\n", "");
    let reason = rejection(verify("greet_test.go", text, &edited));
    assert!(
        reason.starts_with("functional-comment mismatch"),
        "{reason}"
    );
}

// ---- Raw-text hazards (bypassing the edit engine) --------------------

#[test]
fn joining_return_across_a_line_comment_fails() {
    // `return // c\n  x` is `return; x;`; `return x` returns `x`.
    for name in ["a.js", "a.ts"] {
        let original = "function f() {\n  return // c\n  x;\n}\n";
        let edited = "function f() {\n  return x;\n}\n";
        let reason = rejection(verify(name, original, edited));
        assert!(
            reason.starts_with("tree-shape mismatch"),
            "{name}: {reason}"
        );
    }
}

#[test]
fn joining_across_a_multi_line_block_comment_fails() {
    // The Design's `a /*\n*/ b` → `a b`. The pinned grammar does not read
    // the comment's line break as a line terminator, so the original
    // itself does not parse (tests/fixtures/syntax-errors), and nothing
    // can be verified against it.
    rejection(verify("a.js", "a /*\n*/ b\n", "a b\n"));
    // Where the original parses, the line-break check alone catches the
    // flip: tokens and shape are unchanged.
    let reason = rejection(verify("a.js", "f(a /*\n*/ , b);\n", "f(a , b);\n"));
    assert!(
        reason.starts_with("line-break boundary flipped"),
        "{reason}"
    );
}

#[test]
fn joining_go_declarations_across_a_multi_line_comment_fails() {
    // A multi-line general comment acts as a newline in Go. As in
    // JavaScript, the pinned grammar rejects the original outright.
    let original = "package p\n\nvar i = 0 /*\n*/ var j = 1\n";
    rejection(verify(
        "a.go",
        original,
        "package p\n\nvar i = 0 var j = 1\n",
    ));
    let original = "package p\n\nvar i = f(0 /*\n*/ , 1)\n";
    let reason = rejection(verify("a.go", original, "package p\n\nvar i = f(0 , 1)\n"));
    assert!(
        reason.starts_with("line-break boundary flipped"),
        "{reason}"
    );
}

#[test]
fn a_line_break_after_an_inline_comment_in_a_c_define_fails() {
    // The inline slot's span had a line break, so both the rewrite (one
    // EOL after the rendered comment) and the line-break-preserving
    // removal end the directive early, moving `VALUE` out of the macro.
    let text = "#define X /* First\n   line. */ VALUE\nint y;\n";
    let rewritten = edit_where("a.c", text, |comment| {
        (
            Decision::KeepGenerated,
            Some(format!("Rewritten {}.", comment.id)),
        )
    });
    assert_eq!(rewritten, "#define X /* Rewritten C1. */\n VALUE\nint y;\n");
    rejection(verify("a.c", text, &rewritten));
    let removed = edit_where("a.c", text, |_| (Decision::Remove, None));
    assert!(removed.starts_with("#define X\n"), "{removed:?}");
    rejection(verify("a.c", text, &removed));
}

// ---- Shape walk ------------------------------------------------------

#[test]
fn the_python_dedent_pair_fails_the_shape_check() {
    // Identical tokens and gaps (see the unit test in apply::verify); only the nesting of `y()` differs — removing the comment dedents `y()` out of the `if` body, which the tree-shape check must catch.
    let original = "if a:\n    x()\n    # c\n    y()\nz()\n";
    let edited = "if a:\n    x()\n    # c\ny()\nz()\n";
    let reason = rejection(verify("a.py", original, edited));
    assert!(reason.starts_with("tree-shape mismatch"), "{reason}");
}

// ---- Near-limit depth safety -----------------------------------------

/// A JavaScript file nesting `levels` parentheses around a commented
/// literal.
fn nested(levels: usize) -> String {
    format!(
        "let x = {}1 /* Inner. */{};\n",
        "(".repeat(levels),
        ")".repeat(levels)
    )
}

/// Whether the tree for `nested(levels)` is refused as too deep.
fn too_deep(levels: usize) -> bool {
    munge_comments::lang::parse(&nested(levels), file_ref("a.js").language)
        .unwrap()
        .too_deep
}

#[test]
fn a_tree_just_under_the_depth_limit_verifies_on_a_2_mib_stack() {
    // The deepest nesting the registry still converts (binary search: 1
    // level is accepted, MAX_TREE_DEPTH levels are refused).
    assert!(!too_deep(1) && too_deep(MAX_TREE_DEPTH));
    let (mut accepted, mut refused) = (1, MAX_TREE_DEPTH);
    while refused - accepted > 1 {
        let middle = accepted + (refused - accepted) / 2;
        if too_deep(middle) {
            refused = middle;
        } else {
            accepted = middle;
        }
    }
    let text = nested(accepted);
    let tree = munge_comments::lang::parse(&text, file_ref("a.js").language).unwrap();
    let mut depth = 0;
    let mut stack = vec![(&tree.root, 1)];
    while let Some((node, level)) = stack.pop() {
        depth = depth.max(level);
        stack.extend(node.children.iter().map(|child| (child, level + 1)));
    }
    assert!(
        depth > MAX_TREE_DEPTH - 8,
        "{accepted} levels nest only {depth} deep"
    );
    let outcome = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let inventory = inventory_file(&text, file_ref("a.js")).unwrap();
            assert_eq!(inventory.comments.len(), 1);
            let edited = remove("a.js", &text, &["/* Inner. */"]);
            verify("a.js", &text, &edited)
        })
        .unwrap()
        .join()
        .expect("the walks complete without overflowing a 2 MiB stack");
    outcome.expect("the near-limit removal verifies");
}
