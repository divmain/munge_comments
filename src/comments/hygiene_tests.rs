use std::collections::BTreeSet;

use super::*;

use crate::comments::inventory::{FileRef, inventory_file};
use crate::comments::types::{Anchor, Classification, CommentKind, Position};
use crate::lang::ids::LanguageId;

/// A hand-built record over `members` of `text` — the hazard inputs trip
/// the plan-time syntax gate, so they never come from a real inventory.
/// Hygiene reads only the member ranges; the rest is filler.
fn record(text: &str, members: Vec<Range<usize>>) -> InventoryComment {
    let start = members[0].start;
    let end = members[members.len() - 1].end;
    InventoryComment {
        id: format!("C{start}"),
        kind: CommentKind::Block,
        start,
        end,
        line: 1,
        column: 1,
        text: text[start..end].to_string(),
        position: Position::Leading,
        anchor: Anchor {
            node_kind: "end_of_file".to_string(),
            name: None,
            snippet: String::new(),
        },
        anchor_exported: false,
        jsx_container_sole: false,
        prev_code_end: None,
        next_code_start: text.len(),
        enclosing_top_level: None,
        classification: Classification::Prose,
        tags: BTreeSet::new(),
        directive_scope: None,
        directive_line_window: None,
        members,
        editable: true,
    }
}

/// The ranges of `needles` in `text`, each found after the previous one.
fn locate(text: &str, needles: &[&str]) -> Vec<Range<usize>> {
    let mut from = 0;
    needles
        .iter()
        .map(|needle| {
            let start = from
                + text[from..]
                    .find(needle)
                    .unwrap_or_else(|| panic!("{needle:?} not in {text:?}"));
            from = start + needle.len();
            start..from
        })
        .collect()
}

/// Applies ascending, non-overlapping edits.
fn apply(text: &str, edits: &[Edit]) -> String {
    for pair in edits.windows(2) {
        assert!(pair[0].start + pair[0].length <= pair[1].start, "{edits:?}");
    }
    let mut out = text.to_string();
    for edit in edits.iter().rev() {
        out.replace_range(edit.start..edit.start + edit.length, &edit.replacement);
    }
    out
}

/// Removes each of `comments` (located in order) as a lone record.
fn remove(text: &str, comments: &[&str]) -> String {
    let records: Vec<InventoryComment> = locate(text, comments)
        .into_iter()
        .map(|span| record(text, vec![span]))
        .collect();
    let refs: Vec<&InventoryComment> = records.iter().collect();
    apply(text, &compute_removal_edits(text, &refs))
}

#[test]
fn an_inline_comment_keeps_the_left_separator() {
    assert_eq!(remove("a /* c */ b", &["/* c */"]), "a b");
    assert_eq!(remove("return /* c */ x", &["/* c */"]), "return x");
}

#[test]
fn touching_tokens_get_one_space() {
    assert_eq!(remove("let/**/x", &["/**/"]), "let x");
    assert_eq!(remove("a/* c */ b", &["/* c */"]), "a b");
}

#[test]
fn never_merging_pairs_get_no_space() {
    assert_eq!(remove("f(/* c */)", &["/* c */"]), "f()");
    assert_eq!(remove("[/* c */1]", &["/* c */"]), "[1]");
    assert_eq!(remove("f(a,/* c */b)", &["/* c */"]), "f(a,b)");
    assert_eq!(remove("g(x/* c */);", &["/* c */"]), "g(x);");
}

#[test]
fn a_kept_comment_counts_as_code() {
    assert_eq!(remove("x /* a */ // b", &["// b"]), "x /* a */");
    assert_eq!(remove("x /* a */ // b\ny", &["// b"]), "x /* a */\ny");
    // A kept neighbour is code on the right too.
    assert_eq!(remove("/* r */ /* k */ x", &["/* r */"]), "/* k */ x");
}

#[test]
fn a_trailing_comment_takes_its_whitespace_and_keeps_the_newline() {
    assert_eq!(
        remove("foo(); // c\nbar();\n", &["// c"]),
        "foo();\nbar();\n"
    );
    assert_eq!(remove("x = 1  # c\ny = 2\n", &["# c"]), "x = 1\ny = 2\n");
    assert_eq!(remove("foo(); // c", &["// c"]), "foo();");
    assert_eq!(remove("x = 1 /* c */   \n", &["/* c */"]), "x = 1\n");
}

#[test]
fn a_leading_inline_comment_keeps_the_indentation() {
    assert_eq!(remove("    /* c */ x = 1;\n", &["/* c */"]), "    x = 1;\n");
}

#[test]
fn a_multi_line_inline_comment_keeps_one_line_break() {
    // JavaScript ASI: `return` + line break must not become `return x`.
    assert_eq!(
        remove("return /*\n  c */ x;\n", &["/*\n  c */"]),
        "return\n  x;\n"
    );
    assert_eq!(remove("a /*\n*/ b", &["/*\n*/"]), "a\nb");
    // Go: a general comment containing a newline acts like a newline.
    assert_eq!(
        remove("var i = 0 /*\n*/ var j = 1\n", &["/*\n*/"]),
        "var i = 0\nvar j = 1\n"
    );
}

#[test]
fn a_multi_line_trailing_comment_keeps_the_line_terminator() {
    assert_eq!(
        remove("var i = 0 /*\n*/\nx\n", &["/*\n*/"]),
        "var i = 0\nx\n"
    );
    assert_eq!(
        remove("return /* a\n b */\n  x;\n", &["/* a\n b */"]),
        "return\n  x;\n"
    );
}

#[test]
fn a_multi_line_comment_before_a_kept_comment_keeps_its_line_break() {
    // The kept comment is code: `return /* k */ x` would end no statement.
    assert_eq!(
        remove("return /* r\n */ /* k */ x;\n", &["/* r\n */"]),
        "return\n /* k */ x;\n"
    );
}

#[test]
fn a_single_line_inline_comment_never_adds_a_line_break() {
    assert_eq!(remove("a = b /* c */ + d;\n", &["/* c */"]), "a = b + d;\n");
}

#[test]
fn two_comments_on_one_line_are_judged_together() {
    assert_eq!(
        remove("a /* x */ b /* y */ c\n", &["/* x */", "/* y */"]),
        "a b c\n"
    );
    assert_eq!(
        remove("a /* x */ /* y */ b\n", &["/* x */", "/* y */"]),
        "a b\n"
    );
    assert_eq!(
        remove("a; /* x */ // y\nb\n", &["/* x */", "// y"]),
        "a;\nb\n"
    );
    assert_eq!(
        remove("x\n/* x */ /* y */\ny\n", &["/* x */", "/* y */"]),
        "x\ny\n"
    );
}

#[test]
fn an_emptied_line_is_deleted_with_its_terminator() {
    assert_eq!(remove("x\n// c\ny\n", &["// c"]), "x\ny\n");
    assert_eq!(remove("x\n// c", &["// c"]), "x\n");
    assert_eq!(remove("// c\nx\n", &["// c"]), "x\n");
    assert_eq!(
        remove("x\n  /* a\n  b */  \ny\n", &["/* a\n  b */"]),
        "x\ny\n"
    );
}

#[test]
fn an_indented_python_comment_line_is_deleted_cleanly() {
    let text = "def f():\n    # c\n    return 1\n";
    assert_eq!(remove(text, &["# c"]), "def f():\n    return 1\n");
}

#[test]
fn a_group_is_removed_member_by_member() {
    let text = "x = 1\n  // a\n  // b\ny = 2\n";
    let members = locate(text, &["// a", "// b"]);
    let group = record(text, members);
    assert_eq!(
        apply(text, &compute_removal_edits(text, &[&group])),
        "x = 1\ny = 2\n"
    );
}

#[test]
fn blank_runs_collapse_only_at_deletions() {
    // A deleted line inside a run.
    assert_eq!(remove("x\n\n// c\n\ny\n", &["// c"]), "x\n\ny\n");
    // Adjacent to a pre-existing run, before or after it.
    assert_eq!(remove("x\n\n\n// c\ny\n", &["// c"]), "x\n\ny\n");
    assert_eq!(remove("x\n// c\n  \n\ny\n", &["// c"]), "x\n  \ny\n");
    // A single blank line next to a deletion stays.
    assert_eq!(remove("x\n\n// c\ny\n", &["// c"]), "x\n\ny\n");
    // Runs elsewhere are untouched.
    assert_eq!(
        remove("a\n\n\nb\n// c\nd\n\n\ne\n", &["// c"]),
        "a\n\n\nb\nd\n\n\ne\n"
    );
    // A trailing-comment removal deletes no line, so collapses nothing.
    assert_eq!(
        remove("a\n\n\nb // c\n\n\nd\n", &["// c"]),
        "a\n\n\nb\n\n\nd\n"
    );
}

#[test]
fn consecutive_deletions_merge_their_runs() {
    let text = "x\n\n// a\n\n// b\n\ny\n";
    assert_eq!(remove(text, &["// a", "// b"]), "x\n\ny\n");
}

#[test]
fn crlf_input_keeps_crlf() {
    assert_eq!(
        remove("foo(); // c\r\nbar();\r\n", &["// c"]),
        "foo();\r\nbar();\r\n"
    );
    assert_eq!(remove("x\r\n// c\r\ny\r\n", &["// c"]), "x\r\ny\r\n");
    assert_eq!(
        remove("x\r\n\r\n// c\r\n\r\ny\r\n", &["// c"]),
        "x\r\n\r\ny\r\n"
    );
    assert_eq!(
        remove("return /*\r\n  c */ x;\r\n", &["/*\r\n  c */"]),
        "return\r\n  x;\r\n"
    );
}

#[test]
fn a_bom_is_preserved() {
    assert_eq!(remove("\u{FEFF}// c\nx\n", &["// c"]), "\u{FEFF}x\n");
    assert_eq!(
        remove("\u{FEFF}\n\n// c\n\nx\n", &["// c"]),
        "\u{FEFF}\nx\n"
    );
}

#[test]
fn removing_nothing_is_a_no_op() {
    assert!(compute_removal_edits("x // c\n", &[]).is_empty());
}

#[test]
fn edits_are_ascending_and_strictly_shrinking() {
    let text = "a /* x */ b\n\n\n// c\n\nlet/**/y; // z\nreturn /*\n*/ w\n";
    let records: Vec<InventoryComment> =
        locate(text, &["/* x */", "// c", "/**/", "// z", "/*\n*/"])
            .into_iter()
            .map(|span| record(text, vec![span]))
            .collect();
    let refs: Vec<&InventoryComment> = records.iter().collect();
    let edits = compute_removal_edits(text, &refs);
    for edit in &edits {
        assert!(edit.replacement.len() < edit.length, "{edit:?}");
    }
    assert_eq!(apply(text, &edits), "a b\n\nlet y;\nreturn\nw\n");
}

#[test]
fn inventoried_inline_block_comments_never_merge_tokens() {
    let text =
        "const a = 1/* c */+2;\nconst b = [/* c */1, 2/* c */];\nlet/* c */x = f(/* c */);\n";
    let inventory = inventory_file(
        text,
        FileRef {
            language: LanguageId::Typescript,
            file_name: "a.ts".to_string(),
        },
    )
    .expect("inventories");
    let refs: Vec<&InventoryComment> = inventory.comments.iter().collect();
    assert_eq!(refs.len(), 5);
    assert_eq!(
        apply(text, &compute_removal_edits(text, &refs)),
        "const a = 1 +2;\nconst b = [1, 2];\nlet x = f();\n"
    );
}

#[test]
#[should_panic(expected = "overlap")]
fn overlapping_records_are_rejected() {
    let text = "/* a */ x";
    let a = record(text, locate(text, &["/* a */"]));
    let b = record(text, locate(text, &["a */"]));
    compute_removal_edits(text, &[&a, &b]);
}
