//! The deterministic heuristic layer of the decision matrix: similarity
//! plus the short-circuit decisions that run before (and instead of) the
//! judge.
//!
//! Rows are keyed by the Design's matrix labels and evaluated in its
//! precedence order — 1, 1b, 2, 3, 4, 5, then routing with row 9 ahead of
//! rows 6–8:
//!
//! | Row | Situation | Outcome |
//! |---|---|---|
//! | 1 | protected classification (`functional`, `doc-protected`) | `KEEP_PROTECTED` |
//! | 1b | tag `jsx-expression` | `KEEP_ORIGINAL` |
//! | 2 | a tag-flag tag (`todo`, `fixme`, …) | `KEEP_ORIGINAL` + `flagged-for-review` |
//! | 3 | content-empty original | `REMOVE` |
//! | 4 | EASY + `wouldYouCommentHere == false` | `REMOVE` |
//! | 5 | EASY + similarity ≥ threshold | `REMOVE` |
//! | 9 | no regeneration | judge, reduced set |
//! | 6 | EASY, dissimilar | judge, restricted set |
//! | 7–8 | MEDIUM / DIFFICULT | judge, full set |
//!
//! Everything here is a pure function of its arguments: no I/O, no clock,
//! no randomness.

use std::collections::BTreeSet;

use unicode_segmentation::UnicodeSegmentation;

use crate::artifact::types::{Decision, Difficulty};
use crate::comments::classify::TAG_FLAG_TAGS;
use crate::comments::inventory::JSX_EXPRESSION;
use crate::comments::sanitize::{comment_body, is_content_empty};
use crate::comments::types::Classification;
use crate::lang::adapter::LanguageAdapter;

use super::records::DraftRecord;

/// The record tag row 2 adds.
pub const FLAGGED_FOR_REVIEW: &str = "flagged-for-review";

/// The judge's full decision set (rows 7–8).
pub const FULL_SET: [Decision; 5] = [
    Decision::Remove,
    Decision::KeepOriginal,
    Decision::KeepGenerated,
    Decision::Hybrid,
    Decision::NeedsReview,
];

/// The judge's restricted decision set (row 6): an EASY comment may be
/// judged on whether it earns its place, but never rewritten.
pub const RESTRICTED_SET: [Decision; 3] = [
    Decision::Remove,
    Decision::KeepOriginal,
    Decision::NeedsReview,
];

/// The judge's reduced decision set (row 9): with no regeneration there is
/// nothing to keep or merge in.
pub const REDUCED_SET: [Decision; 3] = [
    Decision::Remove,
    Decision::KeepOriginal,
    Decision::NeedsReview,
];

/// The lowercase word tokens of `text`: its `unicode_words()` segments (UAX
/// #29 word boundaries), each lowercased with `str::to_lowercase`.
/// Ideographic scripts segment into single-character tokens, so the set
/// stays meaningful without a dictionary. Shared with remove-mode
/// alignment.
pub fn word_tokens(text: &str) -> BTreeSet<String> {
    text.unicode_words().map(str::to_lowercase).collect()
}

/// Jaccard similarity of the word-token sets of `comment_body(a)` and
/// `comment_body(b)`, in `[0, 1]`. A delimiter-free text (a regeneration)
/// passes through `comment_body` unchanged. Two empty sets score 0.
pub fn similarity(a: &str, b: &str, adapter: &LanguageAdapter) -> f64 {
    let a = word_tokens(&comment_body(a, adapter));
    let b = word_tokens(&comment_body(b, adapter));
    let union = a.union(&b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / union as f64
}

/// Where the short-circuit function sends a record.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Decided without the judge (rows 1–5). `tags` are the record tags the
    /// row adds (row 2's `flagged-for-review`), sorted; usually empty.
    Decided {
        /// The heuristic decision.
        decision: Decision,
        /// Tags to add to the record.
        tags: Vec<String>,
    },
    /// Sent to the judge (rows 6–9) with the decisions it may return.
    Judge {
        /// The packet's allowed decision set.
        allowed: Vec<Decision>,
    },
}

/// Rows 1b–3: the decisions that need no regeneration, so they are made
/// before any LLM call (and also fire under `--offline`). Returns the
/// decision plus the tags it adds, or `None` when none of these rows
/// applies.
///
/// Only scanned prose records are considered: row 1 (protection) precedes
/// these rows, and NEW-origin records (no original) are outside the
/// matrix, so both yield `None`.
pub fn pre_llm_decision(
    record: &DraftRecord,
    adapter: &LanguageAdapter,
) -> Option<(Decision, Vec<String>)> {
    if record.classification != Classification::Prose {
        return None;
    }
    let original = record.original.as_deref()?;
    // Row 1b: the sole content of a JSX `{/* … */}` container.
    if record.has_tag(JSX_EXPRESSION) {
        return Some((Decision::KeepOriginal, Vec::new()));
    }
    // Row 2: tag-flagged (TODO, FIXME, …).
    if TAG_FLAG_TAGS.iter().any(|tag| record.has_tag(tag)) {
        return Some((Decision::KeepOriginal, vec![FLAGGED_FOR_REVIEW.to_owned()]));
    }
    if is_content_empty(original, adapter) {
        return Some((Decision::Remove, Vec::new()));
    }
    None
}

/// The full short-circuit function (rows 1–9, in precedence order): a
/// heuristic decision, or the judge with the record's allowed set.
///
/// - Rows 1–3 decide first (row 1: protected → `KEEP_PROTECTED`; rows
///   1b–3 per [`pre_llm_decision`]).
/// - Row 4: EASY + `wouldYouCommentHere == false` → `REMOVE`, with or
///   without a regeneration.
/// - Row 5: EASY + `similarity(original, regenerated) ≥ threshold` →
///   `REMOVE`.
/// - Row 9 (ahead of rows 6–8): no regeneration → judge, reduced set,
///   whatever the difficulty.
/// - Row 6: EASY and dissimilar → judge, restricted set.
/// - Rows 7–8: MEDIUM / DIFFICULT → judge, full set (the DIFFICULT bias
///   and downgrade are the judge's prompt and post-judge rails).
///
/// A record with a regeneration but no difficulty rating matches none of
/// rows 6–8; it goes to the judge with the restricted set, so an unrated
/// rewrite is never written.
///
/// Meant for scanned records: a NEW-origin record is outside the matrix.
pub fn route(record: &DraftRecord, threshold: f64, adapter: &LanguageAdapter) -> Route {
    if record.classification != Classification::Prose {
        return Route::Decided {
            decision: Decision::KeepProtected,
            tags: Vec::new(),
        };
    }
    // Rows 1b–3.
    if let Some((decision, tags)) = pre_llm_decision(record, adapter) {
        return Route::Decided { decision, tags };
    }
    let easy = record.difficulty == Some(Difficulty::Easy);
    // Row 4: EASY + would not comment here.
    if easy && record.would_you_comment_here == Some(false) {
        return Route::Decided {
            decision: Decision::Remove,
            tags: Vec::new(),
        };
    }
    let Some(regenerated) = record.regenerated.as_deref() else {
        return judge(&REDUCED_SET);
    };
    // Row 5: EASY + redundant restatement.
    let original = record.original.as_deref().unwrap_or_default();
    if easy && similarity(original, regenerated, adapter) >= threshold {
        return Route::Decided {
            decision: Decision::Remove,
            tags: Vec::new(),
        };
    }
    match record.difficulty {
        Some(Difficulty::Medium | Difficulty::Difficult) => judge(&FULL_SET),
        // Row 6 (and an unrated regeneration, conservatively).
        Some(Difficulty::Easy) | None => judge(&RESTRICTED_SET),
    }
}

fn judge(allowed: &[Decision]) -> Route {
    Route::Judge {
        allowed: allowed.to_vec(),
    }
}

#[cfg(test)]
#[path = "heuristics_tests.rs"]
mod tests;
