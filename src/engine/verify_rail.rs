//! The plan-time verification rail: the last per-file step of `plan`.
//!
//! Some individually legal edits are rejected by the semantic verifier
//! only because of where they leave *other* comments (removing line 1 can
//! move a dormant line-3 PEP 263 cookie onto line 2; a rewrite inside the
//! first comment run can add "license" and turn the whole run
//! functional). At apply time such a file would be rejected as a whole,
//! deterministically, on every re-run. So [`verify_rail`] runs apply's
//! in-memory pipeline on the planned records — [`check_plan_fits`], then
//! [`edit_in_memory`] (the very edit computation apply uses, with the
//! run's `allowAdd`), then the verifier — and:
//!
//! - a file with no actionable edit is not verified at all (apply leaves
//!   it `Unchanged` without verifying it either);
//! - if the full plan verifies, the file is done (one verification);
//! - on [`Error::Verify`], the edit-producing records (`REMOVE`,
//!   `KEEP_GENERATED`, `HYBRID`, and `NEW` when `allowAdd`) are re-admitted
//!   greedily: starting from no edits, in ascending `(start, id)` order,
//!   each one is tentatively added and the result re-verified; it is kept
//!   if verification passes and demoted otherwise. A demoted record becomes
//!   `KEEP_ORIGINAL` (`NEEDS_REVIEW` for a NEW-origin record), tagged
//!   [`VERIFY_RAIL_TAG`], `decidedBy: engine`, `confidence: 1`, with its
//!   prior decision recorded in `decisionRationale`, and the file gets one
//!   `verify-demoted` warning per demotion. A tentative edit set whose
//!   edited text equals the last verified one is kept without re-verifying
//!   (verification is a function of the edited text);
//! - any other error propagates: the caller fails the file, classified by
//!   variant.
//!
//! The kept set after the walk is exactly the last edit set that verified,
//! so every `complete` file of a fresh artifact verifies clean when
//! applied.

use crate::apply::stage::{check_plan_fits, edit_in_memory, has_actionable_edit};
use crate::apply::verify::{FunctionalComment, functional_sequence};
use crate::artifact::types::{
    CommentRecord, DecidedBy, Decision, FilePlan, FileWarning, FileWarningKind,
};
use crate::comments::inventory::Inventory;
use crate::comments::types::FileRef;
use crate::errors::Error;

/// The tag on every record the rail demoted.
pub const VERIFY_RAIL_TAG: &str = "verify-rail";

/// The verifier the rail calls: `(original, edited, file, original
/// functional sequence) → verdict`, exactly the shape of
/// [`verify_semantic_equality`](crate::apply::verify::verify_semantic_equality).
pub type VerifyFn =
    dyn Fn(&str, &str, FileRef, &[FunctionalComment]) -> Result<(), Error> + Send + Sync;

/// What the rail did to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RailReport {
    /// How many times the verifier ran.
    pub verifications: usize,
    /// How many records were demoted.
    pub demoted: usize,
}

/// Runs the rail on `file` (a `complete` plan whose records are sorted by
/// `(start, id)`), planned from `text` and its `inventory`, demoting in
/// place whatever would fail verification under `allow_add` (see the
/// module docs).
///
/// # Errors
///
/// Any error other than [`Error::Verify`] from the pipeline or the
/// verifier: a plan that does not fit its own text ([`Error::Artifact`]),
/// an edit touching a functional comment ([`Error::Firewall`]), an
/// [`Error::Invariant`], or a registry/inventory error.
pub fn verify_rail(
    file: &mut FilePlan,
    text: &str,
    inventory: &Inventory,
    allow_add: bool,
    verify: &VerifyFn,
) -> Result<RailReport, Error> {
    let mut report = RailReport::default();
    if !has_actionable_edit(file, allow_add) {
        return Ok(report);
    }
    // Holding records back only removes NEW insertions, so the full plan passing these checks covers every tentative subset. The plan must fit its own text before anything is edited: a mismatch here is an artifact error, not something the greedy walk below could demote away.
    check_plan_fits(file, text, inventory, allow_add)?;
    let functional = functional_sequence(inventory);
    let file_ref = FileRef {
        language: file.language,
        file_name: file.path.clone(),
    };
    // `Ok(None)` when `edited` verifies; `Ok(Some(reason))` when the verifier rejects it. Counts every verifier run for the report and separates a rejection (which the walk can act on) from any other error, which must propagate to the caller.
    let mut rejection = |edited: &str| -> Result<Option<String>, Error> {
        report.verifications += 1;
        match verify(text, edited, file_ref.clone(), &functional) {
            Ok(()) => Ok(None),
            Err(Error::Verify(reason)) => Ok(Some(reason)),
            Err(err) => Err(err),
        }
    };
    let edited = edit_in_memory(text, file, inventory, allow_add)?.text;
    if rejection(&edited)?.is_none() {
        return Ok(report);
    }

    let candidates: Vec<usize> = file
        .comments
        .iter()
        .enumerate()
        .filter(|(_, record)| produces_edit(record, allow_add))
        .map(|(index, _)| index)
        .collect();
    let mut trial = file.clone();
    for &index in &candidates {
        trial.comments[index].decision = held_decision(&file.comments[index]);
    }
    let mut verified = text.to_owned();
    let mut demotions = Vec::new();
    for &index in &candidates {
        trial.comments[index].decision = file.comments[index].decision;
        let edited = edit_in_memory(text, &trial, inventory, allow_add)?.text;
        if edited == verified {
            // No new edit text (e.g. a rewrite the sanitizer skips): the verdict is the last one's. Verification depends only on the edited text, so an edit set that reproduces the last verified text is already known to pass and is kept without re-running the verifier.
            continue;
        }
        match rejection(&edited)? {
            None => verified = edited,
            Some(reason) => {
                trial.comments[index].decision = held_decision(&file.comments[index]);
                demotions.push((index, reason));
            }
        }
    }
    report.demoted = demotions.len();
    for (index, reason) in demotions {
        let warning = demote(&mut file.comments[index], &reason);
        file.warnings.push(warning);
    }
    Ok(report)
}

/// Whether `record` makes an edit when applied under `allow_add`.
fn produces_edit(record: &CommentRecord, allow_add: bool) -> bool {
    match record.decision {
        Decision::Remove | Decision::KeepGenerated | Decision::Hybrid => !record.is_new_origin(),
        Decision::New => allow_add && record.is_new_origin(),
        Decision::KeepOriginal | Decision::NeedsReview | Decision::KeepProtected => false,
    }
}

/// The decision a held-back (or demoted) record takes: `NEEDS_REVIEW` for
/// a NEW-origin record (the only other decision it can carry),
/// `KEEP_ORIGINAL` for a scanned one.
fn held_decision(record: &CommentRecord) -> Decision {
    if record.is_new_origin() {
        Decision::NeedsReview
    } else {
        Decision::KeepOriginal
    }
}

/// Demotes `record` (whose edit failed verification for `reason`) and
/// returns its `verify-demoted` warning. The prior decision, decider,
/// confidence and rationale are kept in `decisionRationale`; the proposed
/// `finalComment` stays, so a reviewer can still see (and approve) it.
fn demote(record: &mut CommentRecord, reason: &str) -> FileWarning {
    let prior = record.decision;
    let held = held_decision(record);
    let mut rationale = format!(
        "verify rail: applying this edit fails verification ({reason}); previously decided {} \
         by {} at confidence {}",
        prior.as_str(),
        record.decided_by.as_str(),
        record.confidence
    );
    if let Some(previous) = record.decision_rationale.as_deref() {
        rationale.push_str(": ");
        rationale.push_str(previous);
    }
    record.decision = held;
    record.decided_by = DecidedBy::Engine;
    record.confidence = 1.0;
    record.decision_rationale = Some(rationale);
    if let Err(at) = record
        .tags
        .binary_search_by(|tag| tag.as_str().cmp(VERIFY_RAIL_TAG))
    {
        record.tags.insert(at, VERIFY_RAIL_TAG.to_owned());
    }
    FileWarning {
        kind: FileWarningKind::VerifyDemoted,
        detail: format!(
            "record {} (line {}) was demoted from {} to {}: applying it fails verification \
             ({reason})",
            record.id,
            record.line,
            prior.as_str(),
            held.as_str()
        ),
    }
}
