//! Run statistics, always recomputed from the records.
//!
//! [`compute_summary`] is the one place the [`Summary`] totals are derived
//! (the [`ArtifactWriter`](crate::artifact::write::ArtifactWriter) stores
//! its result; `report` and `apply` call it afresh because a hand-edited
//! artifact's stored summary is stale). [`convergence_pct`] is the single
//! implementation of the convergence metric.
//!
//! Everything here is computable from the artifact alone: final comment
//! lengths are measured on [`render_comment`] output for the record-only
//! [`slot_from_record`] slot (indent `""`, `"\n"` EOL), so no source file is
//! read.

use crate::apply::render::{render_comment, slot_from_record};
use crate::artifact::types::{
    CommentRecord, Decision, Difficulty, DifficultyHistogram, FilePlan, FileStatus, PlanArtifact,
    Summary,
};
use crate::comments::types::Classification;
use crate::config::PlanConfig;
use crate::errors::Error;
use crate::lang::adapters::adapter_for;

/// Computes the [`Summary`] of `files` under the `config` snapshot.
///
/// Every count follows the field definitions on [`Summary`]. Two config
/// facts matter: `char_reduction_pct` adds the rendered characters of NEW
/// insertions (records decided `NEW`) only when `config.allow_add`, since
/// only then would apply write them.
///
/// Character counts are Unicode scalar values. A record's *final* length is
/// its rendered `finalComment` for writing decisions (`KEEP_GENERATED`,
/// `HYBRID`, `NEW`), 0 for `REMOVE`, and its original length otherwise.
/// Both fractions are 0 when their denominator is 0. `char_reduction_pct`
/// is negative when rewrites lengthen the prose overall; it is reported as
/// computed, never clamped.
///
/// # Errors
///
/// [`Error::Invariant`] when the runtime partition invariant fails: every
/// scanned record must fall into exactly one of removed / rewritten / kept /
/// needs-review / protected (a scanned record decided `NEW` falls into
/// none), and every NEW-origin record into exactly one of new / new-held.
/// Also when a writing decision lacks its `finalComment`. Validated
/// artifacts never trip either check.
pub fn compute_summary(files: &[FilePlan], config: &PlanConfig) -> Result<Summary, Error> {
    let mut summary = Summary {
        files_considered: count(files.len()),
        ..Summary::default()
    };
    let mut prose_removed: u64 = 0;
    let mut original_chars: u64 = 0;
    let mut final_chars: u64 = 0;
    for file in files {
        match file.status {
            FileStatus::Complete => {}
            FileStatus::Failed => summary.files_failed += 1,
            FileStatus::Skipped => summary.files_skipped += 1,
        }
        *summary.languages.entry(file.language).or_insert(0) += 1;
        summary.warnings += count(file.warnings.len());
        for record in &file.comments {
            let partition_violation = || {
                Error::Invariant(format!(
                    "stats partition violated: {}: {}: decision {} is not allowed for a {} record",
                    file.path,
                    record.id,
                    record.decision.as_str(),
                    if record.is_new_origin() {
                        "NEW-origin"
                    } else {
                        "scanned"
                    }
                ))
            };
            if record.is_new_origin() {
                match record.decision {
                    Decision::New => {
                        summary.new += 1;
                        if config.allow_add {
                            final_chars += rendered_chars(file, record)?;
                        }
                    }
                    Decision::NeedsReview => summary.new_held += 1,
                    _ => return Err(partition_violation()),
                }
                continue;
            }
            summary.comments_total += 1;
            match record.decision {
                Decision::Remove => summary.removed += 1,
                Decision::KeepGenerated | Decision::Hybrid => summary.rewritten += 1,
                Decision::KeepOriginal => summary.kept += 1,
                Decision::NeedsReview => summary.needs_review += 1,
                Decision::KeepProtected => summary.protected += 1,
                Decision::New => return Err(partition_violation()),
            }
            if record.classification != Classification::Prose {
                if record.decision != Decision::KeepProtected {
                    summary.overrides += 1;
                }
                continue;
            }
            summary.prose_total += 1;
            tally_difficulty(&mut summary.difficulty_histogram, record.difficulty);
            let original = record.original.as_deref().map_or(0, char_count);
            original_chars += original;
            final_chars += match record.decision {
                Decision::Remove => {
                    prose_removed += 1;
                    0
                }
                decision if decision.writes_final_comment() => rendered_chars(file, record)?,
                _ => original,
            };
        }
    }
    summary.reduction_pct = fraction(prose_removed as f64, summary.prose_total);
    summary.char_reduction_pct =
        fraction(original_chars as f64 - final_chars as f64, original_chars);
    Ok(summary)
}

/// The convergence metric (the single definition):
/// `(KEEP_ORIGINAL + KEEP_PROTECTED + NEEDS_REVIEW) / scanned records`,
/// where scanned records are those with `original != null` (NEW-origin
/// records are excluded from numerator and denominator). An artifact with
/// no scanned records is converged: the metric is 1.
pub fn convergence_pct(artifact: &PlanArtifact) -> f64 {
    let scanned = artifact
        .files
        .iter()
        .flat_map(|file| &file.comments)
        .filter(|record| !record.is_new_origin());
    let (mut unchanged, mut total) = (0_u64, 0_u64);
    for record in scanned {
        total += 1;
        if matches!(
            record.decision,
            Decision::KeepOriginal | Decision::KeepProtected | Decision::NeedsReview
        ) {
            unchanged += 1;
        }
    }
    if total == 0 {
        1.0
    } else {
        unchanged as f64 / total as f64
    }
}

/// The characters `record`'s `finalComment` occupies once rendered into
/// its record-only slot.
fn rendered_chars(file: &FilePlan, record: &CommentRecord) -> Result<u64, Error> {
    let final_comment = record.final_comment.as_deref().ok_or_else(|| {
        Error::Invariant(format!(
            "{}: {}: decision {} requires a finalComment",
            file.path,
            record.id,
            record.decision.as_str()
        ))
    })?;
    let slot = slot_from_record(record, adapter_for(file.language));
    Ok(char_count(&render_comment(final_comment, &slot)))
}

fn tally_difficulty(histogram: &mut DifficultyHistogram, difficulty: Option<Difficulty>) {
    match difficulty {
        Some(Difficulty::Easy) => histogram.easy += 1,
        Some(Difficulty::Medium) => histogram.medium += 1,
        Some(Difficulty::Difficult) => histogram.difficult += 1,
        None => histogram.unrated += 1,
    }
}

/// `numerator / denominator`, 0 when the denominator is 0.
fn fraction(numerator: f64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator / denominator as f64
    }
}

fn char_count(text: &str) -> u64 {
    count(text.chars().count())
}

/// A `usize` count as `u64` (lossless on every supported target).
fn count(n: usize) -> u64 {
    n as u64
}
