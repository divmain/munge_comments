use super::*;

use crate::comments::inventory::{FileRef, inventory_file};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;

/// Inventories `text` as `file_name` under `language` and redacts it.
fn run(language: LanguageId, file_name: &str, text: &str, opts: TransformOpts<'_>) -> RedactOutput {
    let inventory = inventory_file(
        text,
        FileRef {
            language,
            file_name: file_name.to_string(),
        },
    )
    .expect("test input inventories");
    redact(text, &inventory, adapter_for(language), opts)
}

fn ts(text: &str) -> RedactOutput {
    run(
        LanguageId::Typescript,
        "a.ts",
        text,
        TransformOpts::default(),
    )
}

fn ids(output: &RedactOutput) -> Vec<(&str, Range<usize>)> {
    output
        .id_map
        .iter()
        .map(|(id, range)| (id.as_str(), range.clone()))
        .collect()
}

#[test]
fn a_line_comment_keeps_its_line_delimiter() {
    let output = ts("// note\nlet x = 1;\n");
    assert_eq!(output.blind_text, "//⟦C1⟧\nlet x = 1;\n");
    assert_eq!(ids(&output), [("C1", 0..7)]);
    assert!(!output.fallback);
}

#[test]
fn python_line_comments_use_the_hash_delimiter() {
    let output = run(
        LanguageId::Python,
        "a.py",
        "x = 1  # note\n",
        TransformOpts::default(),
    );
    assert_eq!(output.blind_text, "x = 1  #⟦C1⟧\n");
}

#[test]
fn block_and_doc_comments_become_plain_block_placeholders() {
    let output = ts("/* a */ let x = 1;\n/** doc */\nfunction f() {}\n");
    assert_eq!(
        output.blind_text,
        "/*⟦C1⟧*/ let x = 1;\n/*⟦C2⟧*/\nfunction f() {}\n"
    );
    assert_eq!(ids(&output), [("C1", 0..7), ("C2", 19..29)]);
}

#[test]
fn a_group_becomes_one_placeholder_spanning_every_member() {
    let text = "// one\n// two\n// three\nlet x = 1;\n";
    let output = ts(text);
    assert_eq!(output.blind_text, "//⟦C1⟧\nlet x = 1;\n");
    assert_eq!(ids(&output), [("C1", 0..22)]);
}

#[test]
fn functional_comments_stay_verbatim() {
    let text = "// @ts-ignore\nlet x: number = 'a';\n// prose\nlet y = 2;\n";
    let output = ts(text);
    assert_eq!(
        output.blind_text,
        "// @ts-ignore\nlet x: number = 'a';\n//⟦C2⟧\nlet y = 2;\n"
    );
    assert_eq!(ids(&output), [("C2", 35..43)]);
}

#[test]
fn doc_protected_comments_stay_verbatim() {
    let text = "/// Docs.\nfn f() {}\n// prose\nfn g() {}\n";
    let output = run(LanguageId::Rust, "a.rs", text, TransformOpts::default());
    assert_eq!(
        output.blind_text,
        "/// Docs.\nfn f() {}\n//⟦C2⟧\nfn g() {}\n"
    );
}

#[test]
fn jsx_expression_comments_stay_verbatim() {
    let text = "const a = <div>{/* hidden? */}</div>; // prose\n";
    let output = run(LanguageId::Tsx, "a.tsx", text, TransformOpts::default());
    assert_eq!(
        output.blind_text,
        "const a = <div>{/* hidden? */}</div>; //⟦C2⟧\n"
    );
    assert_eq!(ids(&output), [("C2", 38..46)]);
}

#[test]
fn only_targets_are_redacted_and_non_editable_targets_are_ignored() {
    let text = "// @ts-ignore\nlet x = 1;\n// a\nlet y = 2;\n// b\nlet z = 3;\n";
    // C1 is functional: naming it hides nothing. C9 names no record.
    let targets: BTreeSet<String> = ["C1", "C3", "C9"].map(String::from).into();
    let output = run(
        LanguageId::Typescript,
        "a.ts",
        text,
        TransformOpts {
            targets: Some(&targets),
            range: None,
        },
    );
    assert_eq!(
        output.blind_text,
        "// @ts-ignore\nlet x = 1;\n// a\nlet y = 2;\n//⟦C3⟧\nlet z = 3;\n"
    );
    assert_eq!(ids(&output), [("C3", 41..45)]);
}

#[test]
fn a_range_restricts_the_output_but_keeps_original_offsets() {
    let text = "// a\nlet x = 1;\n\n// b\nlet y = 2; // c\n\n// d\nlet z = 3;\n";
    let start = text.find("// b").expect("present");
    let end = text.find("// d").expect("present");
    let output = run(
        LanguageId::Typescript,
        "a.ts",
        text,
        TransformOpts {
            targets: None,
            range: Some(start..end),
        },
    );
    assert_eq!(output.blind_text, "//⟦C2⟧\nlet y = 2; //⟦C3⟧\n\n");
    assert_eq!(ids(&output), [("C2", 17..21), ("C3", 33..37)]);
}

#[test]
fn a_target_straddling_the_range_is_not_redacted() {
    let text = "/* a\n b */\nlet x = 1;\n";
    let output = run(
        LanguageId::Typescript,
        "a.ts",
        text,
        TransformOpts {
            targets: None,
            range: Some(5..text.len()),
        },
    );
    assert_eq!(output.blind_text, " b */\nlet x = 1;\n");
    assert!(output.id_map.is_empty());
}

#[test]
fn placeholder_shaped_text_forces_the_fallback() {
    let output = ts("// see ⟦C1⟧\nlet x = 1;\n");
    assert!(output.fallback);
    assert!(output.blind_text.is_empty());
    assert!(output.id_map.is_empty());
}

#[test]
fn the_collision_probe_matches_exactly_the_placeholder_pattern() {
    let pattern = regex::Regex::new("⟦C[0-9]+⟧").expect("compiles");
    for text in [
        "",
        "⟦C1⟧",
        "x ⟦C12⟧ y",
        "⟦⟦C3⟧",
        "⟦C⟧",
        "⟦c1⟧",
        "⟦C1",
        "⟦C1a⟧",
        "⟦ C1⟧",
        "C1⟧",
        "⟦C٣⟧",
        "⟦C1⟧⟦",
        "⟦C⟦C7⟧",
    ] {
        assert_eq!(
            has_placeholder_collision(text),
            pattern.is_match(text),
            "{text:?}"
        );
    }
}

#[test]
fn crlf_outside_redacted_ranges_survives() {
    let text = "// a\r\nlet x = 1; // b\r\n";
    let output = ts(text);
    assert_eq!(output.blind_text, "//⟦C1⟧\r\nlet x = 1; //⟦C2⟧\r\n");
}

#[test]
fn a_multi_line_block_between_code_keeps_a_line_break() {
    let text = "function f(x: number) {\n  return /*\n  */ x;\n}\n";
    let output = ts(text);
    assert_eq!(
        output.blind_text,
        "function f(x: number) {\n  return /*⟦C1⟧\n*/ x;\n}\n"
    );
    let crlf = "function f(x: number) {\r\n  return /*\r\n  */ x;\r\n}\r\n";
    assert_eq!(
        ts(crlf).blind_text,
        "function f(x: number) {\r\n  return /*⟦C1⟧\r\n*/ x;\r\n}\r\n"
    );
}

#[test]
fn every_line_terminator_counts_for_the_kept_line_break() {
    // U+2028 and U+2029 are line terminators in JavaScript, so a comment
    // holding one ends the statement just like one holding `\n`.
    for terminator in ["\u{2028}", "\u{2029}"] {
        let text = format!("function f(x) {{\n  return /*{terminator}*/ x;\n}}\n");
        assert_eq!(
            ts(&text).blind_text,
            format!("function f(x) {{\n  return /*⟦C1⟧{terminator}*/ x;\n}}\n")
        );
    }
    // Without code on both sides, it collapses like any other.
    assert_eq!(
        ts("let a = 1; /*\u{2028}*/\nlet b = 2;\n").blind_text,
        "let a = 1; /*⟦C1⟧*/\nlet b = 2;\n"
    );
}

#[test]
fn a_multi_line_block_with_its_own_line_break_around_collapses() {
    // Code only before: the line break after the comment separates lines.
    assert_eq!(
        ts("let a = 1; /* one\n two */\nlet b = 2;\n").blind_text,
        "let a = 1; /*⟦C1⟧*/\nlet b = 2;\n"
    );
    // Code only after: the line break before the comment separates lines.
    assert_eq!(
        ts("let a = 1;\n  /* one\n two */ let b = 2;\n").blind_text,
        "let a = 1;\n  /*⟦C1⟧*/ let b = 2;\n"
    );
    // A multi-line doc comment on its own lines.
    assert_eq!(
        ts("/**\n * Doc.\n */\nfunction f() {}\n").blind_text,
        "/*⟦C1⟧*/\nfunction f() {}\n"
    );
}

// ---- remove mode ----

/// Inventories `text` as TypeScript and removes its targets.
fn ts_remove(text: &str, opts: TransformOpts<'_>) -> RemoveOutput {
    let inventory = inventory_file(
        text,
        FileRef {
            language: LanguageId::Typescript,
            file_name: "a.ts".to_string(),
        },
    )
    .expect("test input inventories");
    remove_comments(text, &inventory, opts)
}

#[test]
fn remove_deletes_editable_comments_and_keeps_functional_ones() {
    let text = "// @ts-ignore\nlet x: number = 'a';\n// prose\nlet y = 2; // tail\n";
    let output = ts_remove(text, TransformOpts::default());
    assert_eq!(
        output.clean_text,
        "// @ts-ignore\nlet x: number = 'a';\nlet y = 2;\n"
    );
    assert_eq!(output.original_base, 0);
}

#[test]
fn remove_segments_partition_both_texts() {
    let text = "let a = 1;\n// gone\nlet/* c */b = 2;\n";
    let output = ts_remove(text, TransformOpts::default());
    assert_eq!(output.clean_text, "let a = 1;\nlet b = 2;\n");
    let segments: Vec<(Range<usize>, Range<usize>, bool)> = output
        .offset_map
        .iter()
        .map(|s| {
            (
                s.clean_start..s.clean_end,
                s.original_start..s.original_end,
                s.verbatim,
            )
        })
        .collect();
    assert_eq!(
        segments,
        [
            (0..11, 0..11, true),
            (11..11, 11..19, false),
            (11..14, 19..22, true),
            (14..15, 22..29, false),
            (15..22, 29..36, true),
        ]
    );
}

#[test]
fn clean_offsets_map_back_to_original_offsets() {
    let text = "let a = 1;\n// gone\nlet/* c */b = 2;\n";
    let output = ts_remove(text, TransformOpts::default());
    let map = &output.offset_map;
    // Verbatim bytes map to themselves.
    assert_eq!(map_clean_offset(map, 4), 4);
    // Right after a deleted line: the first byte after the deletion.
    assert_eq!(map_clean_offset(map, 11), 19);
    // The inserted space maps to the start of the span it replaced.
    assert_eq!(map_clean_offset(map, 14), 22);
    assert_eq!(map_clean_offset(map, 15), 29);
    // The end (and beyond) maps to the end of the slice.
    assert_eq!(map_clean_offset(map, 22), 36);
    assert_eq!(map_clean_offset(map, 99), 36);
}

#[test]
fn remove_with_a_range_clips_to_the_slice() {
    let text = "let a = 1;\n// one\nlet b = 2; // two\n// three\nlet c = 3;\n";
    let second = text.find("let b").unwrap();
    let fourth = text.find("let c").unwrap();
    let output = ts_remove(
        text,
        TransformOpts {
            targets: None,
            range: Some(second..fourth),
        },
    );
    assert_eq!(output.clean_text, "let b = 2;\n");
    assert_eq!(output.original_base, second);
    assert_eq!(map_clean_offset(&output.offset_map, 0), second);
    assert_eq!(
        map_clean_offset(&output.offset_map, output.clean_text.len()),
        fourth
    );
}

#[test]
fn remove_honors_an_explicit_target_subset() {
    let text = "// one\nlet a = 1; // two\n";
    let targets: BTreeSet<String> = ["C2".to_string()].into();
    let output = ts_remove(
        text,
        TransformOpts {
            targets: Some(&targets),
            range: None,
        },
    );
    assert_eq!(output.clean_text, "// one\nlet a = 1;\n");
}

#[test]
fn a_straddling_target_stays_verbatim_in_remove_mode() {
    let text = "let a = 1; /* one\n two */\nlet b = 2;\n";
    let second_line = text.find('\n').unwrap() + 1;
    let output = ts_remove(
        text,
        TransformOpts {
            targets: None,
            range: Some(second_line..text.len()),
        },
    );
    assert_eq!(output.clean_text, " two */\nlet b = 2;\n");
}

#[test]
fn a_blank_run_collapse_across_a_range_boundary_matches_the_whole_file() {
    let text = "let a = 1;\n\n// gone\n\n\nlet b = 2;\n";
    let whole = ts_remove(text, TransformOpts::default());
    assert_eq!(whole.clean_text, "let a = 1;\n\nlet b = 2;\n");
    let cut = text.find("\n\nlet b").unwrap() + 1;
    let parts: String = [0..cut, cut..text.len()]
        .into_iter()
        .map(|range| {
            ts_remove(
                text,
                TransformOpts {
                    targets: None,
                    range: Some(range),
                },
            )
            .clean_text
        })
        .collect();
    assert_eq!(parts, whole.clean_text);
}

#[test]
fn an_empty_range_has_one_empty_segment() {
    let output = ts_remove(
        "let a = 1; // c\n",
        TransformOpts {
            targets: None,
            range: Some(3..3),
        },
    );
    assert_eq!(output.clean_text, "");
    assert_eq!(map_clean_offset(&output.offset_map, 0), 3);
}
