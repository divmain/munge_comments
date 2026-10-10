//! The markdown report, rendered from the artifact alone (the source tree
//! need not exist; nothing is parsed).
//!
//! Layout, in order:
//!
//! 1. the **summary**: file totals, a decision table with both reduction
//!    fractions, the difficulty histogram, files per language and the
//!    warning count — all recomputed from the records by
//!    [`compute_summary`], never read from the stored `summary`;
//! 2. one **per-file section** per artifact file (path + language id),
//!    listing every record with its location, difficulty, decision,
//!    `before → after`, rationale, `missingContext` and flags;
//! 3. the **dedicated sections**: needs human review, flagged, protected
//!    policy items, new comment proposals, files not processed, mode
//!    fallbacks and warnings (each always present, "None." when empty);
//! 4. with an [`ApplyResult`], the **apply outcome**.
//!
//! Text conventions: comment text (original, regenerated, final, rendered)
//! is shown as a quoted, escaped single-line code span, so delimiters,
//! line breaks and markdown metacharacters survive verbatim; paths and tags
//! are code spans; free text (rationales, errors, warning details) is
//! inline markdown with line breaks folded to spaces; `partial_apply_hint`
//! is a fenced code block, verbatim.

use std::fmt::Write as _;

use super::stats::compute_summary;
use crate::apply::render::{render_comment, slot_from_record};
use crate::apply::types::{ApplyFileResult, ApplyResult, ApplyStatus, SkippedEditReason};
use crate::artifact::types::{
    CommentRecord, DecidedBy, Decision, Difficulty, FailureKind, FilePlan, FileStatus,
    FileWarningKind, PlanArtifact, SkipReason, Summary,
};
use crate::comments::types::{Classification, CommentKind, Position};
use crate::config::Mode;
use crate::errors::Error;
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;

/// Tags that mark a record as flagged for human attention: the engine's
/// `flagged-for-review` record tag and the classifier's tag-flag tags
/// (`TODO`, `FIXME`, … stored lowercase).
pub const FLAG_TAGS: [&str; 7] = [
    "flagged-for-review",
    "todo",
    "fixme",
    "hack",
    "warning",
    "xxx",
    "note",
];

/// The marker the report attaches to every human override of a Rust doc
/// comment: editing one is a semantic change the verifier cannot see.
pub const RUST_DOC_OVERRIDE_MARKER: &str = "semantic: re-run `cargo test --doc` and `cargo build`";

/// Renders `artifact` (plus, after `apply`, its `apply` outcome) as a
/// markdown report. Deterministic: the same inputs give the same bytes.
///
/// # Errors
///
/// [`Error::Invariant`] when the records violate the stats partition (see
/// [`compute_summary`]); a validated artifact never does.
pub fn render_markdown(
    artifact: &PlanArtifact,
    apply: Option<&ApplyResult>,
) -> Result<String, Error> {
    let summary = compute_summary(&artifact.files, &artifact.config)?;
    let mut out = String::new();
    push_header(&mut out, artifact);
    push_summary(&mut out, &summary);
    for file in &artifact.files {
        push_file(&mut out, artifact, file);
    }
    push_needs_review(&mut out, artifact);
    push_flagged(&mut out, artifact);
    push_protected(&mut out, artifact);
    push_new_proposals(&mut out, artifact);
    push_not_processed(&mut out, artifact);
    push_mode_fallbacks(&mut out, artifact);
    push_warnings(&mut out, artifact);
    if let Some(apply) = apply {
        push_apply_outcome(&mut out, apply);
    }
    Ok(out)
}

/// `writeln!` into a `String`, which cannot fail.
macro_rules! line {
    ($out:expr) => {
        $out.push('\n')
    };
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
    }};
}

fn push_header(out: &mut String, artifact: &PlanArtifact) {
    let config = &artifact.config;
    line!(out, "# munge-comments report — {}", artifact.created_at);
    line!(out);
    line!(
        out,
        "Mode: {} · model: {} · allowAdd: {} · offline: {}",
        code(mode_name(config.mode)),
        code(&config.model),
        on_off(config.allow_add),
        on_off(config.offline),
    );
    line!(out);
}

fn push_summary(out: &mut String, summary: &Summary) {
    line!(out, "## Summary");
    line!(out);
    line!(
        out,
        "Files: {} considered · {} failed · {} skipped",
        summary.files_considered,
        summary.files_failed,
        summary.files_skipped
    );
    line!(out);
    line!(
        out,
        "| comments | prose comments | removed | rewritten | kept | needs review | protected | overrides | new | new held | reduction | char reduction |"
    );
    line!(
        out,
        "|---------:|---------------:|--------:|----------:|-----:|-------------:|----------:|----------:|----:|---------:|----------:|---------------:|"
    );
    line!(
        out,
        "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        summary.comments_total,
        summary.prose_total,
        summary.removed,
        summary.rewritten,
        summary.kept,
        summary.needs_review,
        summary.protected,
        summary.overrides,
        summary.new,
        summary.new_held,
        percent(summary.reduction_pct),
        percent(summary.char_reduction_pct),
    );
    line!(out);
    let histogram = &summary.difficulty_histogram;
    line!(
        out,
        "Difficulty: EASY {} · MEDIUM {} · DIFFICULT {} · unrated {}",
        histogram.easy,
        histogram.medium,
        histogram.difficult,
        histogram.unrated
    );
    let languages: Vec<String> = summary
        .languages
        .iter()
        .map(|(language, files)| format!("{} {files}", language.as_str()))
        .collect();
    line!(out, "Languages: {}", joined_or_none(&languages));
    line!(out, "Warnings: {}", summary.warnings);
    line!(out);
}

fn push_file(out: &mut String, artifact: &PlanArtifact, file: &FilePlan) {
    line!(out, "## {} ({})", code(&file.path), file.language.as_str());
    line!(out);
    match file.status {
        FileStatus::Complete => line!(out, "Status: complete"),
        FileStatus::Failed => line!(
            out,
            "Status: failed ({}){}",
            file.failure_kind.map_or("unknown", failure_kind_name),
            error_suffix(file)
        ),
        FileStatus::Skipped => line!(
            out,
            "Status: skipped ({}){}",
            file.skip_reason.map_or("unknown", skip_reason_name),
            error_suffix(file)
        ),
    }
    if file.effective_mode != artifact.config.mode {
        line!(
            out,
            "Effective mode: {} (fallback from {})",
            code(mode_name(file.effective_mode)),
            code(mode_name(artifact.config.mode))
        );
    }
    if !file.warnings.is_empty() {
        line!(out, "Warnings: {} (see Warnings)", file.warnings.len());
    }
    line!(out);
    if file.comments.is_empty() {
        if file.status == FileStatus::Complete {
            line!(out, "No comments.");
            line!(out);
        }
        return;
    }
    for record in &file.comments {
        push_record(out, artifact, file, record);
    }
}

fn push_record(out: &mut String, artifact: &PlanArtifact, file: &FilePlan, record: &CommentRecord) {
    line!(
        out,
        "### {} — line {} — {} — {} ({}, conf {:.2})",
        record.id,
        record.line,
        record.difficulty.map_or("unrated", difficulty_name),
        record.decision.as_str(),
        decided_by_name(record.decided_by),
        record.confidence
    );
    line!(out);
    line!(
        out,
        "- {} · {} · {} · column {}",
        comment_kind_name(record.kind),
        position_name(record.position),
        classification_name(record.classification),
        record.column
    );
    if !record.tags.is_empty() {
        line!(out, "- flags: {}", code_list(&record.tags));
    }
    line!(
        out,
        "- before → after: {} → {}",
        before(record),
        after(artifact, file, record)
    );
    if let Some(regenerated) = &record.regenerated {
        line!(out, "- regenerated: {}", quoted(regenerated));
    }
    if record.decision == Decision::NeedsReview && record.final_comment.is_some() {
        line!(out, "- proposed: {}", rendered(file, record));
    }
    if let Some(rationale) = &record.decision_rationale {
        line!(out, "- rationale: {}", prose(rationale));
    }
    if let Some(rationale) = &record.difficulty_rationale {
        line!(out, "- difficulty rationale: {}", prose(rationale));
    }
    if let Some(missing) = &record.missing_context {
        line!(out, "- missing context: {}", prose(missing));
    }
    line!(out);
}

/// The `before` side: the original text, or `(none)` for a NEW-origin
/// record.
fn before(record: &CommentRecord) -> String {
    record
        .original
        .as_deref()
        .map_or_else(|| "(none)".to_owned(), quoted)
}

/// The `after` side: what apply would leave in the file.
fn after(artifact: &PlanArtifact, file: &FilePlan, record: &CommentRecord) -> String {
    match record.decision {
        Decision::Remove => "(removed)".to_owned(),
        Decision::KeepGenerated | Decision::Hybrid => rendered(file, record),
        Decision::New if artifact.config.allow_add => rendered(file, record),
        Decision::New => format!("{} (report-only: allowAdd off)", rendered(file, record)),
        Decision::NeedsReview if record.is_new_origin() => "(held for review)".to_owned(),
        Decision::KeepOriginal | Decision::NeedsReview | Decision::KeepProtected => {
            "(unchanged)".to_owned()
        }
    }
}

/// `finalComment` rendered into the record-only slot (as the statistics
/// measure it), quoted.
fn rendered(file: &FilePlan, record: &CommentRecord) -> String {
    match &record.final_comment {
        Some(text) => {
            let slot = slot_from_record(record, adapter_for(file.language));
            quoted(&render_comment(text, &slot))
        }
        None => "(no finalComment)".to_owned(),
    }
}

/// What a record proposes, quoted: its `finalComment` rendered into the
/// record-only slot, else its regenerated text verbatim.
fn proposal(file: &FilePlan, record: &CommentRecord) -> Option<String> {
    if record.final_comment.is_some() {
        Some(rendered(file, record))
    } else {
        record.regenerated.as_deref().map(quoted)
    }
}

fn error_suffix(file: &FilePlan) -> String {
    file.error
        .as_deref()
        .map_or_else(String::new, |error| format!(" — {}", prose(error)))
}

/// Every `(file, record)` pair of the artifact, in artifact order.
fn records(artifact: &PlanArtifact) -> impl Iterator<Item = (&FilePlan, &CommentRecord)> {
    artifact
        .files
        .iter()
        .flat_map(|file| file.comments.iter().map(move |record| (file, record)))
}

/// `` `path` C3 (line 5) `` — a record's location.
fn locator(file: &FilePlan, record: &CommentRecord) -> String {
    format!("{} {} (line {})", code(&file.path), record.id, record.line)
}

/// A dedicated section: its heading, then its items or "None.".
fn push_section(out: &mut String, title: &str, intro: Option<&str>, items: &[String]) {
    line!(out, "## {title}");
    line!(out);
    if let Some(intro) = intro {
        line!(out, "{intro}");
        line!(out);
    }
    if items.is_empty() {
        line!(out, "None.");
    }
    for item in items {
        line!(out, "- {item}");
    }
    line!(out);
}

fn push_needs_review(out: &mut String, artifact: &PlanArtifact) {
    let items: Vec<String> = records(artifact)
        .filter(|(_, record)| record.decision == Decision::NeedsReview)
        .map(|(file, record)| {
            let mut item = locator(file, record);
            let proposed = proposal(file, record);
            if record.is_new_origin() {
                let proposed = proposed.unwrap_or_else(|| "(none)".to_owned());
                let _ = write!(item, ": held NEW proposal {proposed}");
            } else {
                let _ = write!(item, ": {}", before(record));
                if let Some(proposed) = proposed {
                    let _ = write!(item, " → proposed {proposed}");
                }
            }
            if let Some(rationale) = &record.decision_rationale {
                let _ = write!(item, " — {}", prose(rationale));
            }
            item
        })
        .collect();
    push_section(out, "Needs human review", None, &items);
}

fn push_flagged(out: &mut String, artifact: &PlanArtifact) {
    let items: Vec<String> = records(artifact)
        .filter_map(|(file, record)| {
            let flags: Vec<String> = record
                .tags
                .iter()
                .filter(|tag| FLAG_TAGS.contains(&tag.as_str()))
                .cloned()
                .collect();
            (!flags.is_empty()).then(|| {
                format!(
                    "{}: {} — {} — {}",
                    locator(file, record),
                    code_list(&flags),
                    record.decision.as_str(),
                    before(record)
                )
            })
        })
        .collect();
    push_section(out, "Flagged", None, &items);
}

fn push_protected(out: &mut String, artifact: &PlanArtifact) {
    let items: Vec<String> = records(artifact)
        .filter(|(_, record)| record.classification == Classification::DocProtected)
        .map(|(file, record)| {
            let mut item = locator(file, record);
            if record.decision == Decision::KeepProtected {
                item.push_str(": KEEP_PROTECTED");
            } else {
                let _ = write!(
                    item,
                    ": **override** → {} ({})",
                    record.decision.as_str(),
                    decided_by_name(record.decided_by)
                );
                if file.language == LanguageId::Rust {
                    let _ = write!(item, " — **{RUST_DOC_OVERRIDE_MARKER}**");
                }
            }
            item
        })
        .collect();
    push_section(out, "Protected policy items", None, &items);
}

fn push_new_proposals(out: &mut String, artifact: &PlanArtifact) {
    let intro = if artifact.config.allow_add {
        "allowAdd is on: apply inserts records decided NEW."
    } else {
        "allowAdd is off: every proposal is report-only."
    };
    let items: Vec<String> = records(artifact)
        .filter(|(_, record)| record.is_new_origin())
        .map(|(file, record)| {
            let status = match record.decision {
                Decision::New if artifact.config.allow_add => "NEW",
                Decision::New => "NEW, report-only",
                _ => "held for review",
            };
            let text = proposal(file, record).unwrap_or_else(|| "(none)".to_owned());
            format!("{} [{status}]: {text}", locator(file, record))
        })
        .collect();
    push_section(out, "New comment proposals", Some(intro), &items);
}

fn push_not_processed(out: &mut String, artifact: &PlanArtifact) {
    let items: Vec<String> = artifact
        .files
        .iter()
        .filter_map(|file| {
            let (status, why) = match file.status {
                FileStatus::Complete => return None,
                FileStatus::Failed => (
                    "failed",
                    file.failure_kind.map_or("unknown", failure_kind_name),
                ),
                FileStatus::Skipped => (
                    "skipped",
                    file.skip_reason.map_or("unknown", skip_reason_name),
                ),
            };
            Some(format!(
                "{} ({}): {status} — {}{}",
                code(&file.path),
                file.language.as_str(),
                code(why),
                error_suffix(file)
            ))
        })
        .collect();
    push_section(out, "Files not processed", None, &items);
}

fn push_mode_fallbacks(out: &mut String, artifact: &PlanArtifact) {
    let items: Vec<String> = artifact
        .files
        .iter()
        .filter(|file| file.effective_mode != artifact.config.mode)
        .map(|file| {
            format!(
                "{} ({}): effective mode {} (config: {})",
                code(&file.path),
                file.language.as_str(),
                code(mode_name(file.effective_mode)),
                code(mode_name(artifact.config.mode))
            )
        })
        .collect();
    push_section(out, "Mode fallbacks", None, &items);
}

fn push_warnings(out: &mut String, artifact: &PlanArtifact) {
    let items: Vec<String> = artifact
        .files
        .iter()
        .flat_map(|file| {
            file.warnings.iter().map(move |warning| {
                format!(
                    "{}: {} — {}",
                    code(&file.path),
                    code(warning_kind_name(warning.kind)),
                    prose(&warning.detail)
                )
            })
        })
        .collect();
    push_section(out, "Warnings", None, &items);
}

/// Every [`ApplyStatus`], in declaration order (the outcome tally order).
const APPLY_STATUSES: [ApplyStatus; 9] = [
    ApplyStatus::Applied,
    ApplyStatus::Unchanged,
    ApplyStatus::DryRun,
    ApplyStatus::NotSelected,
    ApplyStatus::SkippedPlanStatus,
    ApplyStatus::SkippedFirewall,
    ApplyStatus::SkippedVerify,
    ApplyStatus::FailedInvariant,
    ApplyStatus::FailedIo,
];

fn push_apply_outcome(out: &mut String, apply: &ApplyResult) {
    line!(out, "## Apply outcome");
    line!(out);
    let tally: Vec<String> = APPLY_STATUSES
        .iter()
        .filter_map(|&status| {
            let n = apply.files.iter().filter(|f| f.status == status).count();
            (n > 0).then(|| format!("{} {n}", apply_status_name(status)))
        })
        .collect();
    line!(out, "Files: {}", joined_or_none(&tally));
    line!(out);
    let (bugs, files): (Vec<&ApplyFileResult>, Vec<&ApplyFileResult>) = apply
        .files
        .iter()
        .partition(|file| file.status == ApplyStatus::FailedInvariant);
    line!(out, "### Files");
    line!(out);
    if files.is_empty() {
        line!(out, "None.");
    }
    for file in files {
        push_apply_file(out, file);
    }
    line!(out);
    line!(out, "### Failed invariant (bug reports)");
    line!(out);
    if bugs.is_empty() {
        line!(out, "None.");
    } else {
        line!(
            out,
            "Internal invariant violations: nothing was written for these files, and re-running fails the same way. Please report them as bugs."
        );
        line!(out);
        for file in bugs {
            push_apply_file(out, file);
        }
    }
    line!(out);
    if let Some(hint) = &apply.partial_apply_hint {
        line!(out, "### Partial apply");
        line!(out);
        line!(
            out,
            "Some files could not be read or written. Re-run them with:"
        );
        line!(out);
        line!(out, "{}", fenced(hint));
        line!(out);
    }
}

fn push_apply_file(out: &mut String, file: &ApplyFileResult) {
    let reason = file
        .reason
        .as_deref()
        .map_or_else(String::new, |reason| format!(" — {}", prose(reason)));
    line!(
        out,
        "- {}: {}{reason}",
        code(&file.path),
        code(apply_status_name(file.status))
    );
    for skipped in &file.skipped_edits {
        line!(
            out,
            "  - skipped edit {}: {}",
            skipped.id,
            code(skipped_edit_reason_name(skipped.reason))
        );
    }
}

// ---- text helpers ----

/// A fraction in `[0, 1]` as a percentage with one decimal.
fn percent(fraction: f64) -> String {
    format!("{:.1}%", fraction * 100.0)
}

fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

fn joined_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(" · ")
    }
}

/// Comment text as a quoted, escaped (single-line) code span.
fn quoted(text: &str) -> String {
    code(&format!("{text:?}"))
}

/// `text` as an inline code span: control characters and line/paragraph
/// separators are escaped (a span never breaks the line), and the
/// backtick fence is one longer than the longest backtick run inside.
fn code(text: &str) -> String {
    let escaped: String = text
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                c.escape_debug().to_string()
            } else {
                c.to_string()
            }
        })
        .collect();
    let fence = "`".repeat(longest_backtick_run(&escaped) + 1);
    let pad = if escaped.starts_with('`') || escaped.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{escaped}{pad}{fence}")
}

fn code_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| code(item))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `text` verbatim in a fenced code block whose fence is longer than any
/// backtick run inside (minimum three).
fn fenced(text: &str) -> String {
    let fence = "`".repeat((longest_backtick_run(text) + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// Free text as single-line inline markdown: every whitespace run that
/// contains a line break folds to one space, and the ends are trimmed.
fn prose(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for (index, line) in text
        .split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index > 0 {
            folded.push(' ');
        }
        folded.push_str(line);
    }
    folded
}

// ---- wire names (pinned to the serde names by unit tests) ----

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Redact => "redact",
        Mode::Remove => "remove",
    }
}

fn difficulty_name(difficulty: Difficulty) -> &'static str {
    match difficulty {
        Difficulty::Easy => "EASY",
        Difficulty::Medium => "MEDIUM",
        Difficulty::Difficult => "DIFFICULT",
    }
}

fn decided_by_name(decided_by: DecidedBy) -> &'static str {
    match decided_by {
        DecidedBy::Heuristic => "heuristic",
        DecidedBy::Judge => "judge",
        DecidedBy::Protected => "protected",
        DecidedBy::Engine => "engine",
        DecidedBy::Human => "human",
    }
}

fn comment_kind_name(kind: CommentKind) -> &'static str {
    match kind {
        CommentKind::Line => "line",
        CommentKind::Block => "block",
        CommentKind::Doc => "doc",
    }
}

fn position_name(position: Position) -> &'static str {
    match position {
        Position::Leading => "leading",
        Position::Trailing => "trailing",
        Position::Dangling => "dangling",
    }
}

fn classification_name(classification: Classification) -> &'static str {
    match classification {
        Classification::Prose => "prose",
        Classification::Functional => "functional",
        Classification::DocProtected => "doc-protected",
    }
}

fn failure_kind_name(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Llm => "llm",
        FailureKind::Io => "io",
        FailureKind::Invariant => "invariant",
    }
}

fn skip_reason_name(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::NonUtf8 => "non-utf8",
        SkipReason::DeclaredEncoding => "declared-encoding",
        SkipReason::SyntaxErrors => "syntax-errors",
        SkipReason::TooDeep => "too-deep",
    }
}

fn warning_kind_name(kind: FileWarningKind) -> &'static str {
    match kind {
        FileWarningKind::PlaceholderCollision => "placeholder-collision",
        FileWarningKind::UnresolvableAnchor => "unresolvable-anchor",
        FileWarningKind::UnknownId => "unknown-id",
        FileWarningKind::OversizedChunk => "oversized-chunk",
        FileWarningKind::InvalidInsertion => "invalid-insertion",
        FileWarningKind::SanitizeDroppedNew => "sanitize-dropped-new",
        FileWarningKind::VerifyDemoted => "verify-demoted",
    }
}

fn apply_status_name(status: ApplyStatus) -> &'static str {
    match status {
        ApplyStatus::Applied => "applied",
        ApplyStatus::Unchanged => "unchanged",
        ApplyStatus::DryRun => "dry-run",
        ApplyStatus::NotSelected => "not-selected",
        ApplyStatus::SkippedPlanStatus => "skipped-plan-status",
        ApplyStatus::SkippedFirewall => "skipped-firewall",
        ApplyStatus::SkippedVerify => "skipped-verify",
        ApplyStatus::FailedInvariant => "failed-invariant",
        ApplyStatus::FailedIo => "failed-io",
    }
}

fn skipped_edit_reason_name(reason: SkippedEditReason) -> &'static str {
    match reason {
        SkippedEditReason::FinalCommentInvalid => "final-comment-invalid",
        SkippedEditReason::JsxExpression => "jsx-expression",
        SkippedEditReason::AllowAddOff => "allow-add-off",
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;

    fn wire<T: Serialize>(value: T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn names_are_the_serde_wire_names() {
        for mode in [Mode::Redact, Mode::Remove] {
            assert_eq!(mode_name(mode), wire(mode));
        }
        for d in [Difficulty::Easy, Difficulty::Medium, Difficulty::Difficult] {
            assert_eq!(difficulty_name(d), wire(d));
        }
        for d in [
            DecidedBy::Heuristic,
            DecidedBy::Judge,
            DecidedBy::Protected,
            DecidedBy::Engine,
            DecidedBy::Human,
        ] {
            assert_eq!(decided_by_name(d), wire(d));
        }
        for k in [CommentKind::Line, CommentKind::Block, CommentKind::Doc] {
            assert_eq!(comment_kind_name(k), wire(k));
        }
        for p in [Position::Leading, Position::Trailing, Position::Dangling] {
            assert_eq!(position_name(p), wire(p));
        }
        for c in [
            Classification::Prose,
            Classification::Functional,
            Classification::DocProtected,
        ] {
            assert_eq!(classification_name(c), wire(c));
        }
        for k in [FailureKind::Llm, FailureKind::Io, FailureKind::Invariant] {
            assert_eq!(failure_kind_name(k), wire(k));
        }
        for r in [
            SkipReason::NonUtf8,
            SkipReason::DeclaredEncoding,
            SkipReason::SyntaxErrors,
            SkipReason::TooDeep,
        ] {
            assert_eq!(skip_reason_name(r), wire(r));
        }
        for k in [
            FileWarningKind::PlaceholderCollision,
            FileWarningKind::UnresolvableAnchor,
            FileWarningKind::UnknownId,
            FileWarningKind::OversizedChunk,
            FileWarningKind::InvalidInsertion,
            FileWarningKind::SanitizeDroppedNew,
            FileWarningKind::VerifyDemoted,
        ] {
            assert_eq!(warning_kind_name(k), wire(k));
        }
        for s in APPLY_STATUSES {
            assert_eq!(apply_status_name(s), wire(s));
        }
        for r in [
            SkippedEditReason::FinalCommentInvalid,
            SkippedEditReason::JsxExpression,
            SkippedEditReason::AllowAddOff,
        ] {
            assert_eq!(skipped_edit_reason_name(r), wire(r));
        }
    }

    #[test]
    fn code_spans_survive_backticks_and_line_breaks() {
        assert_eq!(code("a.rs"), "`a.rs`");
        assert_eq!(code("a`b"), "``a`b``");
        assert_eq!(code("`x`"), "`` `x` ``");
        assert_eq!(code("a\nb\u{2028}c"), "`a\\nb\\u{2028}c`");
        assert_eq!(quoted("// \"hi\"\n// there"), r#"`"// \"hi\"\n// there"`"#);
    }

    #[test]
    fn fences_outgrow_inner_backtick_runs() {
        assert_eq!(fenced("cmd"), "```\ncmd\n```");
        assert_eq!(fenced("a ```` b"), "`````\na ```` b\n`````");
    }

    #[test]
    fn prose_folds_line_breaks_to_spaces() {
        assert_eq!(prose("  one\n  two \r\n\nthree  "), "one two three");
        assert_eq!(prose("single"), "single");
        assert_eq!(prose("\n"), "");
    }

    #[test]
    fn percent_has_one_decimal() {
        assert_eq!(percent(0.0), "0.0%");
        assert_eq!(percent(0.902), "90.2%");
        assert_eq!(percent(1.0), "100.0%");
        assert_eq!(percent(-0.25), "-25.0%");
    }
}
