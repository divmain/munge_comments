//! Statistics (`compute_summary`, `convergence_pct`) and the markdown
//! report, over hand-built artifacts whose numbers are reconciled with the
//! records by hand.

mod common;

use munge_comments::apply::types::{
    ApplyFileResult, ApplyResult, ApplyStatus, SkippedEdit, SkippedEditReason,
};
use munge_comments::artifact::types::{
    ARTIFACT_VERSION, CommentRecord, DecidedBy, Decision, Difficulty, FailureKind, FilePlan,
    FileStatus, FileWarning, FileWarningKind, PlanArtifact, SkipReason, Summary,
};
use munge_comments::artifact::validate::content_hash;
use munge_comments::comments::types::{Anchor, Classification, CommentKind, Position};
use munge_comments::{
    Error, FileRef, LanguageId, Mode, PlanConfig, compute_summary, convergence_pct, inventory_file,
    render_markdown,
};

use common::file_plan_from_inventory;

/// A small Rust file exercising every scanned bucket: a license header
/// (functional), a doc comment (doc-protected), and four prose comments.
const LIB_RS: &str = "\
// SPDX-License-Identifier: MIT

pub const VERSION: u32 = 1;

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    // add them together
    a + b
}

// TODO: overflow handling
pub fn mul(a: i32, b: i32) -> i32 {
    a * b // multiply
}

// Uses wrapping arithmetic because the protocol mandates modulo 2^32.
pub fn sub(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}

pub fn neg(a: i32) -> i32 {
    -a
}
";

const WRAPPING: &str = "// Uses wrapping arithmetic because the protocol mandates modulo 2^32.";

fn config(allow_add: bool) -> PlanConfig {
    PlanConfig {
        mode: Mode::Redact,
        model: "gpt-4.1".to_owned(),
        base_url: None,
        allow_add,
        similarity_threshold: 0.6,
        max_file_chars: 120_000,
        concurrency: 4,
        offline: false,
        paths: vec![".".to_owned()],
        include: None,
        exclude: Vec::new(),
    }
}

/// `src/lib.rs`, planned from its real inventory, with hand edits:
///
/// - the SPDX header stays KEEP_PROTECTED;
/// - the `///` doc comment is hand-overridden to REMOVE (an override);
/// - `// add them together` → REMOVE; `// TODO…` → KEEP_ORIGINAL (flagged);
///   `// multiply` → KEEP_GENERATED; the wrapping comment → NEEDS_REVIEW;
/// - N1 is a NEW insertion above `neg`, N2 a verify-demoted (held) NEW
///   above `sub`.
///
/// The file fell back to remove mode under a redact config.
fn lib_rs() -> FilePlan {
    let inventory = inventory_file(
        LIB_RS,
        FileRef {
            language: LanguageId::Rust,
            file_name: "lib.rs".to_owned(),
        },
    )
    .unwrap();
    let mut plan = file_plan_from_inventory(
        "src/lib.rs",
        LIB_RS,
        &inventory,
        LanguageId::Rust,
        |comment| match comment.text.as_str() {
            "// SPDX-License-Identifier: MIT" => (Decision::KeepProtected, None),
            "/// Adds two numbers." => (Decision::Remove, None),
            "// add them together" => (Decision::Remove, None),
            "// TODO: overflow handling" => (Decision::KeepOriginal, None),
            "// multiply" => (Decision::KeepGenerated, Some("Product.".to_owned())),
            WRAPPING => (Decision::NeedsReview, None),
            other => panic!("unexpected comment {other:?}"),
        },
    );
    let classes: Vec<(&str, Classification)> = plan
        .comments
        .iter()
        .map(|r| (r.original.as_deref().unwrap(), r.classification))
        .collect();
    assert_eq!(
        classes,
        [
            (
                "// SPDX-License-Identifier: MIT",
                Classification::Functional
            ),
            ("/// Adds two numbers.", Classification::DocProtected),
            ("// add them together", Classification::Prose),
            ("// TODO: overflow handling", Classification::Prose),
            ("// multiply", Classification::Prose),
            (WRAPPING, Classification::Prose),
        ],
        "the fixture's classifications"
    );
    for record in &mut plan.comments {
        match record.original.as_deref().unwrap() {
            "// SPDX-License-Identifier: MIT" => record.decided_by = DecidedBy::Protected,
            "// add them together" => {
                record.difficulty = Some(Difficulty::Easy);
                record.decided_by = DecidedBy::Judge;
                record.confidence = 0.95;
                record.decision_rationale = Some("restates `a + b`".to_owned());
            }
            "// TODO: overflow handling" => {
                record.tags.push("flagged-for-review".to_owned());
                record.tags.sort();
                record.decided_by = DecidedBy::Heuristic;
            }
            "// multiply" => {
                record.difficulty = Some(Difficulty::Medium);
                record.regenerated = Some("// Product.".to_owned());
                record.decided_by = DecidedBy::Judge;
                record.confidence = 0.82;
                record.decision_rationale =
                    Some("the regenerated text\nnames the result".to_owned());
            }
            WRAPPING => {
                record.difficulty = Some(Difficulty::Difficult);
                record.difficulty_rationale = Some("the protocol is not in the code".to_owned());
                record.missing_context =
                    Some("the protocol mandates modulo 2^32 arithmetic".to_owned());
                record.regenerated = Some("// Subtracts b from a.".to_owned());
                record.decided_by = DecidedBy::Judge;
                record.confidence = 0.6;
                record.decision_rationale = Some("judge and rails disagree".to_owned());
            }
            _ => {}
        }
    }
    let neg = LIB_RS.find("pub fn neg").unwrap();
    let sub = LIB_RS.find("pub fn sub").unwrap();
    plan.comments.push(new_record(
        "N1",
        neg,
        21,
        "neg",
        Decision::New,
        "Negates a.",
    ));
    let mut held = new_record(
        "N2",
        sub,
        17,
        "sub",
        Decision::NeedsReview,
        "Wraps on overflow.",
    );
    held.tags = vec!["verify-rail".to_owned()];
    held.decision_rationale = Some("demoted from NEW by the verify rail".to_owned());
    plan.comments.push(held);
    plan.comments
        .sort_by(|a, b| (a.start, &a.id).cmp(&(b.start, &b.id)));
    plan.warnings = vec![
        FileWarning {
            kind: FileWarningKind::PlaceholderCollision,
            detail: "placeholder `__MC_1__` occurs in the file; fell back to remove mode"
                .to_owned(),
        },
        FileWarning {
            kind: FileWarningKind::VerifyDemoted,
            detail: "N2 demoted to NEEDS_REVIEW (was NEW)".to_owned(),
        },
    ];
    plan
}

fn new_record(
    id: &str,
    offset: usize,
    line: u32,
    anchor: &str,
    decision: Decision,
    text: &str,
) -> CommentRecord {
    CommentRecord {
        id: id.to_owned(),
        kind: CommentKind::Line,
        start: offset,
        end: offset,
        line,
        column: 1,
        position: Position::Leading,
        classification: Classification::Prose,
        tags: Vec::new(),
        original: None,
        anchor: Anchor {
            node_kind: "function_item".to_owned(),
            name: Some(anchor.to_owned()),
            snippet: format!("pub fn {anchor}("),
        },
        regenerated: None,
        would_you_comment_here: None,
        difficulty: None,
        difficulty_rationale: None,
        missing_context: None,
        decision,
        final_comment: Some(text.to_owned()),
        confidence: 1.0,
        decision_rationale: None,
        decided_by: DecidedBy::Engine,
    }
}

/// A file planned in redact mode with no comments at all.
fn empty_ts() -> FilePlan {
    FilePlan {
        path: "web/empty.ts".to_owned(),
        content_hash: Some(content_hash(b"export {};\n")),
        byte_length: 11,
        language: LanguageId::Typescript,
        effective_mode: Mode::Redact,
        status: FileStatus::Complete,
        failure_kind: None,
        skip_reason: None,
        error: None,
        warnings: Vec::new(),
        comments: Vec::new(),
    }
}

fn not_processed(
    path: &str,
    language: LanguageId,
    status: FileStatus,
    failure_kind: Option<FailureKind>,
    skip_reason: Option<SkipReason>,
    error: &str,
) -> FilePlan {
    let never_read = failure_kind == Some(FailureKind::Io);
    FilePlan {
        path: path.to_owned(),
        content_hash: (!never_read).then(|| content_hash(path.as_bytes())),
        byte_length: if never_read { 0 } else { path.len() as u64 },
        language,
        effective_mode: Mode::Redact,
        status,
        failure_kind,
        skip_reason,
        error: Some(error.to_owned()),
        warnings: Vec::new(),
        comments: Vec::new(),
    }
}

/// Every unprocessed-file shape: failed llm / io, skipped too-deep /
/// declared-encoding.
fn unprocessed_files() -> Vec<FilePlan> {
    vec![
        not_processed(
            "deep/nested.go",
            LanguageId::Go,
            FileStatus::Skipped,
            None,
            Some(SkipReason::TooDeep),
            "syntax tree deeper than 2000 levels",
        ),
        not_processed(
            "legacy/latin.py",
            LanguageId::Python,
            FileStatus::Skipped,
            None,
            Some(SkipReason::DeclaredEncoding),
            "coding cookie declares latin-1",
        ),
        not_processed(
            "src/locked.py",
            LanguageId::Python,
            FileStatus::Failed,
            Some(FailureKind::Io),
            None,
            "permission denied",
        ),
        not_processed(
            "src/main.rs",
            LanguageId::Rust,
            FileStatus::Failed,
            Some(FailureKind::Llm),
            None,
            "llm error (timeout)",
        ),
    ]
}

/// The full fixture artifact, validated; its stored `summary` is
/// deliberately bogus (stale), proving everything is recomputed.
fn artifact(allow_add: bool) -> PlanArtifact {
    let mut files = vec![lib_rs(), empty_ts()];
    files.extend(unprocessed_files());
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let stale = Summary {
        comments_total: 999,
        reduction_pct: 0.5,
        ..Summary::default()
    };
    let artifact = PlanArtifact {
        version: ARTIFACT_VERSION,
        created_at: "2026-10-09T14:03:11Z".to_owned(),
        root: ".".to_owned(),
        glob_base: ".".to_owned(),
        config: config(allow_add),
        files,
        summary: stale,
    };
    artifact.validate().unwrap().into_inner()
}

fn summary_of(artifact: &PlanArtifact) -> Summary {
    compute_summary(&artifact.files, &artifact.config).unwrap()
}

fn records_mut<'a>(artifact: &'a mut PlanArtifact, path: &str) -> &'a mut Vec<CommentRecord> {
    &mut artifact
        .files
        .iter_mut()
        .find(|file| file.path == path)
        .unwrap()
        .comments
}

// ---- stats ----

#[test]
fn the_summary_reconciles_with_the_records_exactly() {
    let summary = summary_of(&artifact(false));
    assert_eq!(summary.files_considered, 6);
    assert_eq!(summary.files_failed, 2);
    assert_eq!(summary.files_skipped, 2);
    let languages: Vec<(&str, u64)> = summary
        .languages
        .iter()
        .map(|(language, n)| (language.as_str(), *n))
        .collect();
    assert_eq!(
        languages,
        [("typescript", 1), ("python", 2), ("go", 1), ("rust", 2)]
    );
    assert_eq!(summary.comments_total, 6, "N1/N2 are not scanned");
    assert_eq!(summary.prose_total, 4);
    assert_eq!(
        summary.removed, 2,
        "`// add them together` + the doc override"
    );
    assert_eq!(summary.rewritten, 1);
    assert_eq!(summary.kept, 1);
    assert_eq!(summary.needs_review, 1);
    assert_eq!(summary.protected, 1);
    assert_eq!(
        summary.removed
            + summary.rewritten
            + summary.kept
            + summary.needs_review
            + summary.protected,
        summary.comments_total,
        "the runtime partition"
    );
    assert_eq!(summary.overrides, 1, "doc-protected → REMOVE");
    assert_eq!(summary.new, 1);
    assert_eq!(summary.new_held, 1, "the verify-demoted NEW");
    assert_eq!(summary.warnings, 2);
    assert_eq!(
        summary.reduction_pct,
        1.0 / 4.0,
        "one of four prose records removed"
    );
    // Original prose chars: 20 + 26 + 11 + 70 = 127. Final: REMOVE → 0,
    // TODO kept → 26, `// Product.` → 11, NEEDS_REVIEW kept → 70.
    assert_eq!(summary.char_reduction_pct, (127.0 - 107.0) / 127.0);
    let histogram = &summary.difficulty_histogram;
    assert_eq!(
        (
            histogram.easy,
            histogram.medium,
            histogram.difficult,
            histogram.unrated
        ),
        (1, 1, 1, 1),
        "over prose records only (the TODO is unrated)"
    );
    insta::assert_json_snapshot!("summary", summary);
}

#[test]
fn new_insertions_count_toward_char_reduction_only_under_allow_add() {
    let off = summary_of(&artifact(false));
    let on = summary_of(&artifact(true));
    // With allowAdd on, N1 renders as `// Negates a.` (13 chars) and its insertion adds 13 chars; the held N2 never counts.
    assert_eq!(on.char_reduction_pct, (127.0 - (107.0 + 13.0)) / 127.0);
    assert!(on.char_reduction_pct < off.char_reduction_pct);
    assert_eq!(
        Summary {
            char_reduction_pct: off.char_reduction_pct,
            ..on
        },
        off,
        "nothing else depends on allowAdd"
    );
}

#[test]
fn a_char_reduction_that_lengthens_is_reported_unclamped() {
    let mut artifact = artifact(false);
    for record in records_mut(&mut artifact, "src/lib.rs") {
        if record.decision == Decision::Remove {
            record.decision = Decision::KeepOriginal;
        }
    }
    let summary = summary_of(&artifact);
    // Only `// multiply` (11) → `// Product.` (11) changes: 0 net, plus
    // nothing removed.
    assert_eq!(summary.char_reduction_pct, 0.0);
    for record in records_mut(&mut artifact, "src/lib.rs") {
        if record.decision == Decision::KeepGenerated {
            record.final_comment = Some("The product of a and b.".to_owned());
        }
    }
    let summary = summary_of(&artifact);
    // `// The product of a and b.` is 26 chars: 15 more than the original, so the ratio goes negative instead of clamping at zero.
    assert_eq!(summary.char_reduction_pct, -15.0 / 127.0);
}

#[test]
fn a_scanned_new_decision_violates_the_runtime_partition() {
    let mut artifact = artifact(false);
    let record = records_mut(&mut artifact, "src/lib.rs")
        .iter_mut()
        .find(|r| r.id == "C3")
        .unwrap();
    record.decision = Decision::New;
    record.final_comment = Some("x".to_owned());
    let err = compute_summary(&artifact.files, &artifact.config).unwrap_err();
    assert!(matches!(err, Error::Invariant(_)), "{err:?}");
    let message = err.to_string();
    assert!(
        message.contains("src/lib.rs") && message.contains("C3") && message.contains("NEW"),
        "{message}"
    );
    assert_eq!(err.exit_code(), 7);
    assert!(matches!(
        render_markdown(&artifact, None),
        Err(Error::Invariant(_))
    ));
}

#[test]
fn a_new_origin_record_outside_new_and_held_violates_the_runtime_partition() {
    let mut artifact = artifact(false);
    let record = records_mut(&mut artifact, "src/lib.rs")
        .iter_mut()
        .find(|r| r.id == "N1")
        .unwrap();
    record.decision = Decision::Remove;
    let err = compute_summary(&artifact.files, &artifact.config).unwrap_err();
    assert!(matches!(err, Error::Invariant(_)), "{err:?}");
    assert!(err.to_string().contains("N1"), "{err}");
}

#[test]
fn zero_denominators_are_guarded() {
    let empty = compute_summary(&[], &config(true)).unwrap();
    assert_eq!(empty, Summary::default());

    // Files but no prose (and no records at all): both fractions are 0.
    let mut files = vec![empty_ts()];
    files.extend(unprocessed_files());
    let summary = compute_summary(&files, &config(false)).unwrap();
    assert_eq!(summary.files_considered, 5);
    assert_eq!(summary.comments_total, 0);
    assert_eq!(summary.reduction_pct, 0.0);
    assert_eq!(summary.char_reduction_pct, 0.0);
}

#[test]
fn convergence_counts_no_change_decisions_over_scanned_records() {
    let artifact = artifact(false);
    // Scanned: 6. No-change: KEEP_ORIGINAL (TODO) + KEEP_PROTECTED (SPDX)
    // + NEEDS_REVIEW (wrapping) = 3. N1/N2 are excluded entirely.
    assert_eq!(convergence_pct(&artifact), 3.0 / 6.0);

    let mut converged = artifact.clone();
    for record in records_mut(&mut converged, "src/lib.rs") {
        if !record.is_new_origin() {
            record.decision = Decision::KeepOriginal;
        }
    }
    assert_eq!(convergence_pct(&converged), 1.0, "NEW records never count");
}

#[test]
fn convergence_with_no_scanned_records_is_one() {
    let mut artifact = artifact(false);
    artifact.files.retain(|file| file.path != "src/lib.rs");
    assert_eq!(convergence_pct(&artifact), 1.0);
    artifact.files.clear();
    assert_eq!(convergence_pct(&artifact), 1.0);
}

// ---- markdown ----

/// An apply outcome exercising every status, reasons, skipped edits, a
/// failed-invariant file and a (hand-set) partial-apply hint.
fn apply_result() -> ApplyResult {
    let file = |path: &str, status: ApplyStatus, reason: Option<&str>| ApplyFileResult {
        path: path.to_owned(),
        status,
        reason: reason.map(str::to_owned),
        skipped_edits: Vec::new(),
    };
    let mut applied = file("src/lib.rs", ApplyStatus::Applied, None);
    applied.skipped_edits = vec![
        SkippedEdit {
            id: "N1".to_owned(),
            reason: SkippedEditReason::AllowAddOff,
        },
        SkippedEdit {
            id: "C5".to_owned(),
            reason: SkippedEditReason::FinalCommentInvalid,
        },
    ];
    ApplyResult {
        files: vec![
            applied,
            file("web/empty.ts", ApplyStatus::Unchanged, None),
            file("web/dry.ts", ApplyStatus::DryRun, None),
            file("web/other.ts", ApplyStatus::NotSelected, None),
            file(
                "deep/nested.go",
                ApplyStatus::SkippedPlanStatus,
                Some("planned as skipped (too-deep)"),
            ),
            file(
                "web/fw.tsx",
                ApplyStatus::SkippedFirewall,
                Some("edit C2 intersects functional comment `// @ts-ignore`"),
            ),
            file(
                "web/asi.ts",
                ApplyStatus::SkippedVerify,
                Some("tree-shape mismatch\nat node 12"),
            ),
            file(
                "src/overlap.rs",
                ApplyStatus::FailedInvariant,
                Some("overlapping edits C1 [0, 9) and C2 [4, 12)"),
            ),
            file(
                "src/locked.py",
                ApplyStatus::FailedIo,
                Some("permission denied"),
            ),
        ],
        partial_apply_hint: Some(
            "munge-comments apply munge-comments-plan.json --only src/locked.py".to_owned(),
        ),
    }
}

#[test]
fn the_plan_report_renders_every_section() {
    let markdown = render_markdown(&artifact(false), None).unwrap();
    for needle in [
        "## Summary",
        "Difficulty: EASY 1 · MEDIUM 1 · DIFFICULT 1 · unrated 1",
        "Languages: typescript 1 · python 2 · go 1 · rust 2",
        "Warnings: 2",
        "## `src/lib.rs` (rust)",
        "- missing context: the protocol mandates modulo 2^32 arithmetic",
        "## Needs human review",
        "held NEW proposal `\"// Wraps on overflow.\"`",
        "## Flagged",
        "`flagged-for-review`, `todo`",
        "## Protected policy items",
        "**semantic: re-run `cargo test --doc` and `cargo build`**",
        "## New comment proposals",
        "allowAdd is off: every proposal is report-only.",
        "## Files not processed",
        "`deep/nested.go` (go): skipped — `too-deep`",
        "`legacy/latin.py` (python): skipped — `declared-encoding`",
        "## Mode fallbacks",
        "`src/lib.rs` (rust): effective mode `remove` (config: `redact`)",
        "## Warnings",
        "`verify-demoted`",
    ] {
        assert!(
            markdown.contains(needle),
            "missing {needle:?} in:\n{markdown}"
        );
    }
    assert!(!markdown.contains("## Apply outcome"));
    assert!(
        !markdown.contains("999"),
        "the stale stored summary is never read"
    );
    insta::assert_snapshot!("plan_report", markdown);
}

#[test]
fn allow_add_changes_how_new_proposals_are_marked() {
    let markdown = render_markdown(&artifact(true), None).unwrap();
    assert!(markdown.contains("allowAdd is on: apply inserts records decided NEW."));
    assert!(!markdown.contains("report-only"));
    insta::assert_snapshot!("plan_report_allow_add", markdown);
}

#[test]
fn the_post_apply_report_appends_the_outcome() {
    let artifact = artifact(false);
    let plain = render_markdown(&artifact, None).unwrap();
    let markdown = render_markdown(&artifact, Some(&apply_result())).unwrap();
    assert!(
        markdown.starts_with(&plain),
        "the outcome is appended to the plan report"
    );
    let outcome = &markdown[plain.len()..];
    for status in [
        "applied",
        "unchanged",
        "dry-run",
        "not-selected",
        "skipped-plan-status",
        "skipped-firewall",
        "skipped-verify",
        "failed-invariant",
        "failed-io",
    ] {
        assert!(
            outcome.contains(&format!("`{status}`")),
            "{status}:\n{outcome}"
        );
    }
    let bugs = outcome
        .split("### Failed invariant (bug reports)")
        .nth(1)
        .unwrap();
    assert!(bugs.contains("`src/overlap.rs`"));
    let files = outcome.split("### Failed invariant").next().unwrap();
    assert!(
        !files.contains("src/overlap.rs"),
        "failed-invariant files are listed separately"
    );
    assert!(outcome.contains("skipped edit N1: `allow-add-off`"));
    assert!(
        outcome.contains(
            "```\nmunge-comments apply munge-comments-plan.json --only src/locked.py\n```"
        )
    );
    insta::assert_snapshot!("apply_report", outcome);
}

#[test]
fn an_empty_artifact_renders_a_complete_report() {
    let mut artifact = artifact(false);
    artifact.files.clear();
    let markdown = render_markdown(&artifact, Some(&ApplyResult::default())).unwrap();
    assert!(markdown.contains("Languages: none"));
    assert!(markdown.contains("| 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0.0% | 0.0% |"));
    assert!(!markdown.contains("### Partial apply"));
    insta::assert_snapshot!("empty_report", markdown);
}
