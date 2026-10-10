//! The outcome of an `apply` run. Apply never mutates the artifact; this
//! is the only record of what it did, rendered by the post-apply report.

use serde::{Deserialize, Serialize};

/// The outcome of a whole `apply` run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    /// One entry per artifact file (selected or not), in artifact order.
    pub files: Vec<ApplyFileResult>,
    /// A ready-to-run `munge-comments apply <artifact> --only <p> …` line
    /// re-selecting the `failed-io` files of a partially applied run. Built
    /// by `run_apply`; rendered verbatim by the report.
    pub partial_apply_hint: Option<String>,
}

/// The outcome for one artifact file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyFileResult {
    /// The artifact-relative path, as recorded in the artifact.
    pub path: String,
    /// What happened to the file.
    pub status: ApplyStatus,
    /// Detail for every non-applied status.
    pub reason: Option<String>,
    /// Individual edits that were not made (the rest of the file's edits
    /// were).
    pub skipped_edits: Vec<SkippedEdit>,
}

/// What happened to one artifact file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyStatus {
    /// Edited and written.
    Applied,
    /// No actionable edit: nothing to write.
    Unchanged,
    /// Would be written; a dry run writes nothing (in memory only).
    DryRun,
    /// Not matched by `--only` (exit-code-neutral).
    NotSelected,
    /// The artifact records the file as `failed` or `skipped`.
    SkippedPlanStatus,
    /// An edit range intersects a currently-functional comment.
    SkippedFirewall,
    /// The semantic verifier rejected the edited text.
    SkippedVerify,
    /// An internal invariant was violated (a bug canary; re-running fails
    /// the same way).
    FailedInvariant,
    /// Reading or writing the file failed (transient; see
    /// [`ApplyResult::partial_apply_hint`]).
    FailedIo,
}

/// One record's edit that `compute_edits` did not make; the record's
/// original text stays in place (`KEEP_ORIGINAL` semantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedEdit {
    /// The record id (`C<n>` / `N<n>`).
    pub id: String,
    /// Why the edit was not made.
    pub reason: SkippedEditReason,
}

/// Why an individual edit was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkippedEditReason {
    /// `finalComment` failed normalization + `validate_final_comment` for
    /// the target slot (e.g. `*/` bound for a block slot).
    FinalCommentInvalid,
    /// A non-keep decision on the sole content of a JSX `{/* … */}`
    /// container (tag `jsx-expression`), which cannot be edited safely.
    JsxExpression,
    /// A `NEW` insertion while `allowAdd` is off.
    AllowAddOff,
}
