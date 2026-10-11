use super::*;

use crate::artifact::types::DecidedBy;
use crate::comments::types::{Anchor, CommentKind, Position};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;

/// The default `--similarity-threshold`.
const THRESHOLD: f64 = 0.6;

fn js() -> &'static LanguageAdapter {
    adapter_for(LanguageId::Javascript)
}

fn py() -> &'static LanguageAdapter {
    adapter_for(LanguageId::Python)
}

/// An undecided scanned prose record with `original` and `tags`, every
/// model field null.
fn draft(original: &str, tags: &[&str]) -> DraftRecord {
    let mut tags: Vec<String> = tags.iter().map(|tag| (*tag).to_owned()).collect();
    tags.sort();
    DraftRecord {
        id: "C1".to_owned(),
        kind: CommentKind::Line,
        start: 0,
        end: original.len(),
        line: 1,
        column: 1,
        position: Position::Leading,
        classification: Classification::Prose,
        tags,
        original: Some(original.to_owned()),
        anchor: Anchor {
            node_kind: "expression_statement".to_owned(),
            name: None,
            snippet: "count += 1".to_owned(),
        },
        regenerated: None,
        would_you_comment_here: None,
        difficulty: None,
        difficulty_rationale: None,
        missing_context: None,
        decision: None,
        final_comment: None,
        confidence: None,
        decision_rationale: None,
        decided_by: None,
    }
}

/// `draft` with call #1's fields filled.
fn rated(
    original: &str,
    tags: &[&str],
    difficulty: Option<Difficulty>,
    regenerated: Option<&str>,
    would_you_comment_here: Option<bool>,
) -> DraftRecord {
    DraftRecord {
        difficulty,
        regenerated: regenerated.map(str::to_owned),
        would_you_comment_here,
        ..draft(original, tags)
    }
}

fn decided(decision: Decision) -> Route {
    Route::Decided {
        decision,
        tags: Vec::new(),
    }
}

fn flagged_keep() -> Route {
    Route::Decided {
        decision: Decision::KeepOriginal,
        tags: vec![FLAGGED_FOR_REVIEW.to_owned()],
    }
}

fn judged(allowed: &[Decision]) -> Route {
    Route::Judge {
        allowed: allowed.to_vec(),
    }
}

// ---- Similarity. ------------------------------------------------------------

#[test]
fn word_tokens_are_lowercase_uax29_words() {
    let tokens = word_tokens("Parse the URL, then parse-it again: 3.14!");
    let expected: BTreeSet<String> = ["parse", "the", "url", "then", "it", "again", "3.14"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(tokens, expected);
    assert!(word_tokens("// ----- */").is_empty());
}

#[test]
fn ideographs_segment_into_single_character_tokens() {
    let tokens = word_tokens("计算总和");
    assert_eq!(tokens.len(), 4);
    assert!(tokens.contains("计"));
}

#[test]
fn similarity_is_jaccard_over_comment_bodies() {
    // Delimiters are stripped; a delimiter-free regeneration passes through.
    assert_eq!(
        similarity("// Increment the counter", "increment THE counter", js()),
        1.0
    );
    assert_eq!(similarity("/* alpha beta */", "gamma delta", js()), 0.0);
    // {increment, the, counter} vs {increment, counter, by, one}: 2 / 5.
    let score = similarity("# Increment the counter", "Increment counter by one", py());
    assert!((score - 0.4).abs() < 1e-12, "{score}");
    // Symmetric and deterministic.
    assert_eq!(
        similarity("// a b c", "b c d", js()),
        similarity("b c d", "// a b c", js())
    );
}

#[test]
fn two_empty_token_sets_score_zero() {
    assert_eq!(similarity("//", "/* */", js()), 0.0);
    assert_eq!(similarity("# -----", "", py()), 0.0);
}

#[test]
fn a_near_identical_cjk_regeneration_is_similar() {
    let score = similarity("// 计算所有订单的总和", "计算所有订单总和", js());
    assert!(score > 0.6, "{score}");
    assert!(score < 1.0, "{score}");
}

// ---- Row 1: protected. --------------------------------------------------------

#[test]
fn row_1_protected_classifications_keep_protected() {
    for classification in [Classification::Functional, Classification::DocProtected] {
        let record = DraftRecord {
            classification,
            ..rated("// eslint-disable-next-line", &[], None, None, None)
        };
        assert_eq!(
            route(&record, THRESHOLD, js()),
            decided(Decision::KeepProtected)
        );
        assert_eq!(pre_llm_decision(&record, js()), None);
    }
    // Prose is never protected, whatever its rating.
    let prose = rated(
        "// keeps a running total",
        &[],
        Some(Difficulty::Medium),
        Some("x"),
        None,
    );
    assert_ne!(
        route(&prose, THRESHOLD, js()),
        decided(Decision::KeepProtected)
    );
}

#[test]
fn row_1_precedes_every_other_row() {
    // A functional, tag-flagged, content-empty-looking EASY record: row 1 outranks tags, difficulty and the would-you flag.
    let record = DraftRecord {
        classification: Classification::Functional,
        ..rated(
            "//",
            &["todo", JSX_EXPRESSION],
            Some(Difficulty::Easy),
            None,
            Some(false),
        )
    };
    assert_eq!(
        route(&record, THRESHOLD, js()),
        decided(Decision::KeepProtected)
    );
}

// ---- Row 1b: JSX container. ---------------------------------------------------

#[test]
fn row_1b_jsx_container_comment_keeps_original() {
    let record = draft("/* The sole content of a container. */", &[JSX_EXPRESSION]);
    assert_eq!(
        pre_llm_decision(&record, js()),
        Some((Decision::KeepOriginal, Vec::new()))
    );
    assert_eq!(
        route(&record, THRESHOLD, js()),
        decided(Decision::KeepOriginal)
    );
    // Without the tag the same comment is not pre-decided.
    let plain = draft("/* The sole content of a container. */", &[]);
    assert_eq!(pre_llm_decision(&plain, js()), None);
}

#[test]
fn an_empty_jsx_container_comment_keeps_via_row_1b_not_row_3() {
    let record = draft("/* */", &[JSX_EXPRESSION]);
    assert_eq!(
        pre_llm_decision(&record, js()),
        Some((Decision::KeepOriginal, Vec::new()))
    );
    // The untagged empty comment is removed by row 3.
    assert_eq!(
        pre_llm_decision(&draft("/* */", &[]), js()),
        Some((Decision::Remove, Vec::new()))
    );
}

// ---- Row 2: tag-flagged. ------------------------------------------------------

#[test]
fn row_2_every_tag_flag_keeps_and_flags_for_review() {
    for tag in TAG_FLAG_TAGS {
        let record = draft("// FIXME later", &[tag]);
        assert_eq!(
            pre_llm_decision(&record, js()),
            Some((Decision::KeepOriginal, vec![FLAGGED_FOR_REVIEW.to_owned()])),
            "{tag}"
        );
    }
    // An untagged comment (e.g. lowercase `todo`, which the classifier does
    // not flag) falls through.
    assert_eq!(pre_llm_decision(&draft("// todo later", &[]), js()), None);
}

#[test]
fn a_short_todo_keeps_despite_being_short() {
    let record = rated(
        "# TODO: x",
        &["todo"],
        Some(Difficulty::Easy),
        Some("TODO: x"),
        Some(false),
    );
    assert_eq!(route(&record, THRESHOLD, py()), flagged_keep());
}

#[test]
fn a_tagged_easy_similar_comment_keeps() {
    let record = rated(
        "// NOTE: increment the counter",
        &["note"],
        Some(Difficulty::Easy),
        Some("NOTE: increment the counter"),
        Some(true),
    );
    assert!(
        similarity(
            "// NOTE: increment the counter",
            "NOTE: increment the counter",
            js()
        ) >= THRESHOLD
    );
    assert_eq!(route(&record, THRESHOLD, js()), flagged_keep());
}

#[test]
fn row_1b_precedes_row_2() {
    let record = draft("/* TODO: wire up */", &[JSX_EXPRESSION, "todo"]);
    assert_eq!(
        route(&record, THRESHOLD, js()),
        decided(Decision::KeepOriginal)
    );
}

// ---- Row 3: content-empty. ----------------------------------------------------

#[test]
fn row_3_content_empty_comments_are_removed() {
    for (original, adapter) in [
        ("//", js()),
        ("/* */", js()),
        ("/***/", js()),
        ("# -----", py()),
    ] {
        let record = draft(original, &[]);
        assert_eq!(
            pre_llm_decision(&record, adapter),
            Some((Decision::Remove, Vec::new())),
            "{original}"
        );
        assert_eq!(
            route(&record, THRESHOLD, adapter),
            decided(Decision::Remove)
        );
    }
    // One word is content.
    assert_eq!(pre_llm_decision(&draft("# ----- x", &[]), py()), None);
}

#[test]
fn rows_1b_to_3_need_no_regeneration_and_skip_new_origin_records() {
    // Decided with every model field null.
    assert!(pre_llm_decision(&draft("//", &[]), js()).is_some());
    let new_origin = DraftRecord {
        original: None,
        ..draft("", &[])
    };
    assert_eq!(pre_llm_decision(&new_origin, js()), None);
}

// ---- Row 4: EASY + wouldYouCommentHere == false. -----------------------------

#[test]
fn row_4_easy_unwanted_comment_is_removed() {
    let dissimilar = rated(
        "// keeps a running total",
        &[],
        Some(Difficulty::Easy),
        Some("accumulate the sum of orders"),
        Some(false),
    );
    assert_eq!(
        route(&dissimilar, THRESHOLD, js()),
        decided(Decision::Remove)
    );
    // `true` (or null, remove mode) does not fire row 4: dissimilar → row 6.
    for would in [Some(true), None] {
        let record = DraftRecord {
            would_you_comment_here: would,
            ..dissimilar.clone()
        };
        assert_eq!(route(&record, THRESHOLD, js()), judged(&RESTRICTED_SET));
    }
    // Not EASY: row 4 does not fire.
    let medium = DraftRecord {
        difficulty: Some(Difficulty::Medium),
        ..dissimilar
    };
    assert_eq!(route(&medium, THRESHOLD, js()), judged(&FULL_SET));
}

#[test]
fn row_4_precedes_row_9_for_an_empty_regeneration() {
    let record = rated(
        "// keeps a running total",
        &[],
        Some(Difficulty::Easy),
        None,
        Some(false),
    );
    assert_eq!(route(&record, THRESHOLD, js()), decided(Decision::Remove));
}

// ---- Row 5: EASY + similar. ---------------------------------------------------

#[test]
fn row_5_easy_redundant_restatement_is_removed() {
    let similar = rated(
        "// Increment the counter",
        &[],
        Some(Difficulty::Easy),
        Some("Increment the counter."),
        Some(true),
    );
    assert_eq!(route(&similar, THRESHOLD, js()), decided(Decision::Remove));
    // Below the threshold: row 6.
    let dissimilar = DraftRecord {
        regenerated: Some("Bump the retry budget".to_owned()),
        ..similar.clone()
    };
    assert_eq!(route(&dissimilar, THRESHOLD, js()), judged(&RESTRICTED_SET));
    // Not EASY: similarity alone never decides.
    for difficulty in [Difficulty::Medium, Difficulty::Difficult] {
        let record = DraftRecord {
            difficulty: Some(difficulty),
            ..similar.clone()
        };
        assert_eq!(route(&record, THRESHOLD, js()), judged(&FULL_SET));
    }
}

#[test]
fn row_5_threshold_is_inclusive_and_configurable() {
    // {increment, the, counter} vs {increment, counter, by, one}: 0.4, so the threshold is inclusive.
    let record = rated(
        "// Increment the counter",
        &[],
        Some(Difficulty::Easy),
        Some("Increment counter by one"),
        None,
    );
    assert_eq!(route(&record, 0.4, js()), decided(Decision::Remove));
    assert_eq!(route(&record, 0.41, js()), judged(&RESTRICTED_SET));
}

// ---- Rows 6–8: judge routing. -------------------------------------------------

#[test]
fn row_6_easy_dissimilar_goes_to_the_restricted_set() {
    let record = rated(
        "// keeps a running total",
        &[],
        Some(Difficulty::Easy),
        Some("Accumulate order sums"),
        Some(true),
    );
    assert_eq!(route(&record, THRESHOLD, js()), judged(&RESTRICTED_SET));
    assert!(!RESTRICTED_SET.contains(&Decision::KeepGenerated));
    assert!(!RESTRICTED_SET.contains(&Decision::Hybrid));
}

#[test]
fn rows_7_and_8_medium_and_difficult_go_to_the_full_set() {
    for difficulty in [Difficulty::Medium, Difficulty::Difficult] {
        let record = rated(
            "// retries are capped to stay under the gateway timeout",
            &[],
            Some(difficulty),
            Some("Cap retries."),
            Some(false),
        );
        assert_eq!(
            route(&record, THRESHOLD, js()),
            judged(&FULL_SET),
            "{difficulty:?}"
        );
    }
    assert_eq!(
        FULL_SET.to_vec(),
        crate::artifact::types::JudgeDecision::ALL
            .map(Decision::from)
            .to_vec()
    );
}

#[test]
fn an_unrated_regeneration_is_never_offered_a_rewrite() {
    let record = rated(
        "// keeps a running total",
        &[],
        None,
        Some("Sum orders"),
        None,
    );
    assert_eq!(route(&record, THRESHOLD, js()), judged(&RESTRICTED_SET));
}

// ---- Row 9: no regeneration. --------------------------------------------------

#[test]
fn row_9_no_regeneration_goes_to_the_reduced_set_whatever_the_difficulty() {
    for difficulty in [
        None,
        Some(Difficulty::Easy),
        Some(Difficulty::Medium),
        Some(Difficulty::Difficult),
    ] {
        let record = rated(
            "// keeps a running total",
            &[],
            difficulty,
            None,
            Some(true),
        );
        assert_eq!(
            route(&record, THRESHOLD, js()),
            judged(&REDUCED_SET),
            "{difficulty:?}"
        );
    }
    // MEDIUM / DIFFICULT with `wouldYouCommentHere == false` and no
    // regeneration: row 9, never the full set.
    for difficulty in [Difficulty::Medium, Difficulty::Difficult] {
        let record = rated(
            "// keeps a running total",
            &[],
            Some(difficulty),
            None,
            Some(false),
        );
        assert_eq!(
            route(&record, THRESHOLD, js()),
            judged(&REDUCED_SET),
            "{difficulty:?}"
        );
    }
    assert!(!REDUCED_SET.contains(&Decision::KeepGenerated));
    assert!(!REDUCED_SET.contains(&Decision::Hybrid));
}

#[test]
fn routing_ignores_an_existing_decision() {
    // `route` is a pure function of the matrix inputs.
    let mut record = rated(
        "// keeps a running total",
        &[],
        Some(Difficulty::Medium),
        Some("x"),
        None,
    );
    let before = route(&record, THRESHOLD, js());
    record.decision = Some(Decision::Remove);
    record.decided_by = Some(DecidedBy::Human);
    assert_eq!(route(&record, THRESHOLD, js()), before);
}
