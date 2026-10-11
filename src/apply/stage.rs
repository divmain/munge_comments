//! The in-memory edit pipeline one file's plan goes through, shared by
//! `apply`'s Phase A ([`super::run`]) and `plan`'s verification rail
//! (`engine::verify_rail`), so the edits a plan was verified with are the
//! edits apply makes:
//!
//! 1. [`check_plan_fits`]: firewall check (a) (every scanned record's
//!    range and text match a current inventory record) and the NEW-offset
//!    checks;
//! 2. [`edit_in_memory`]: [`compute_edits`], firewall check (b) (no edit
//!    intersects a currently-functional comment), then [`apply_edits`].
//!
//! The semantic verifier ([`super::verify::verify_semantic_equality`])
//! runs on the result; its caller decides what a rejection means.

use super::edits::{EditOpts, apply_edits, compute_edits};
use super::types::SkippedEdit;
use crate::artifact::types::{Decision, FilePlan};
use crate::comments::inventory::Inventory;
use crate::comments::types::{Classification, Edit};
use crate::errors::Error;
use crate::lang::adapters::adapter_for;

/// A file's text with its plan's edits applied, not yet verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditedText {
    /// The edited text.
    pub text: String,
    /// The edits that produced it, in application order.
    pub edits: Vec<Edit>,
    /// Edits [`compute_edits`] did not make.
    pub skipped: Vec<SkippedEdit>,
}

/// Whether `file` has any edit to make: a scanned record decided
/// `REMOVE`/`KEEP_GENERATED`/`HYBRID`, or (with `allow_add`) a record
/// decided `NEW`. A file without one is left alone by apply (and by the
/// plan-time verification rail): it is never read, edited or verified.
pub fn has_actionable_edit(file: &FilePlan, allow_add: bool) -> bool {
    file.comments.iter().any(|record| match record.decision {
        Decision::Remove | Decision::KeepGenerated | Decision::Hybrid => true,
        Decision::New => allow_add,
        Decision::KeepOriginal | Decision::NeedsReview | Decision::KeepProtected => false,
    })
}

/// Step 1: firewall check (a) and the NEW-offset checks of `file`'s
/// records against `text` and its current `inventory`.
///
/// # Errors
///
/// [`Error::Artifact`] on any mismatch (see the two checks).
pub fn check_plan_fits(
    file: &FilePlan,
    text: &str,
    inventory: &Inventory,
    allow_add: bool,
) -> Result<(), Error> {
    check_scanned_records(file, inventory)?;
    check_new_offsets(file, text, inventory, allow_add)
}

/// Step 2: `file`'s edits ([`compute_edits`] with `allow_add`), firewall
/// check (b) on them, and the edited text ([`apply_edits`]).
///
/// # Errors
///
/// [`compute_edits`]' and [`apply_edits`]' errors, and
/// [`Error::Firewall`] when an edit intersects a functional comment.
pub fn edit_in_memory(
    text: &str,
    file: &FilePlan,
    inventory: &Inventory,
    allow_add: bool,
) -> Result<EditedText, Error> {
    let computed = compute_edits(
        text,
        file,
        inventory,
        adapter_for(file.language),
        EditOpts { allow_add },
    )?;
    check_functional_intersections(&computed.edits, inventory)?;
    let edited = apply_edits(text, &computed.edits)?;
    Ok(EditedText {
        text: edited,
        edits: computed.edits,
        skipped: computed.skipped,
    })
}

/// Firewall check (a): every scanned record's `[start, end)` is exactly a
/// current inventory record's range, and that record's text equals the
/// record's `original`. Any mismatch is [`Error::Artifact`].
fn check_scanned_records(file: &FilePlan, inventory: &Inventory) -> Result<(), Error> {
    for record in &file.comments {
        let Some(original) = record.original.as_deref() else {
            continue;
        };
        let current = inventory
            .comments
            .binary_search_by_key(&record.start, |entry| entry.start)
            .ok()
            .map(|index| &inventory.comments[index])
            .filter(|entry| entry.end == record.end);
        match current {
            None => {
                return Err(Error::Artifact(format!(
                    "record {} range [{}, {}) matches no comment in the current file",
                    record.id, record.start, record.end
                )));
            }
            Some(entry) if entry.text != original => {
                return Err(Error::Artifact(format!(
                    "record {} text does not match the comment at [{}, {}) in the current file",
                    record.id, record.start, record.end
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The NEW-offset checks, for every NEW-origin record that will insert
/// (decided `NEW`, with `allowAdd` on; a held or disabled NEW makes no
/// edit): `start` must be in bounds, on a `char` boundary, not before or
/// inside a leading BOM, not before the end of a shebang line (its `\n`
/// included, the only terminator the kernel honors), and not strictly
/// inside a current comment's range. Any violation is [`Error::Artifact`].
fn check_new_offsets(
    file: &FilePlan,
    text: &str,
    inventory: &Inventory,
    allow_add: bool,
) -> Result<(), Error> {
    if !allow_add {
        return Ok(());
    }
    const BOM: char = '\u{feff}';
    let bom_end = if text.starts_with(BOM) {
        BOM.len_utf8()
    } else {
        0
    };
    // `None`: the whole file is the shebang line.
    let shebang_end = inventory
        .has_shebang
        .then(|| text.find('\n').map(|newline| newline + 1));
    for record in &file.comments {
        if !record.is_new_origin() || record.decision != Decision::New {
            continue;
        }
        let start = record.start;
        let bad = |detail: String| {
            Err(Error::Artifact(format!(
                "NEW record {} at offset {start} {detail}",
                record.id
            )))
        };
        if start > text.len() {
            return bad(format!("is out of bounds of the {}-byte file", text.len()));
        }
        if !text.is_char_boundary(start) {
            return bad("is not on a character boundary".to_owned());
        }
        if start < bom_end {
            return bad("lies before the end of the leading byte-order mark".to_owned());
        }
        match shebang_end {
            Some(Some(end)) if start < end => {
                return bad(format!(
                    "lies before the end of the shebang line (offset {end})"
                ));
            }
            Some(None) => return bad("lies on the shebang line".to_owned()),
            _ => {}
        }
        if let Some(entry) = inventory
            .comments
            .iter()
            .find(|entry| entry.start < start && start < entry.end)
        {
            return bad(format!(
                "lies inside the comment at [{}, {})",
                entry.start, entry.end
            ));
        }
    }
    Ok(())
}

/// Firewall check (b): no edit intersects a comment the current inventory
/// classifies `functional`. A non-empty edit intersects a comment it
/// overlaps; an insertion intersects a comment it lies strictly inside.
/// A violation is [`Error::Firewall`] (the whole file is skipped).
fn check_functional_intersections(edits: &[Edit], inventory: &Inventory) -> Result<(), Error> {
    for edit in edits {
        let start = edit.start;
        let end = edit.start.saturating_add(edit.length);
        let hit = inventory
            .comments
            .iter()
            .filter(|entry| entry.classification == Classification::Functional)
            .find(|entry| {
                if edit.length == 0 {
                    entry.start < start && start < entry.end
                } else {
                    start < entry.end && entry.start < end
                }
            });
        if let Some(entry) = hit {
            let tags: Vec<&str> = entry.tags.iter().map(String::as_str).collect();
            return Err(Error::Firewall(format!(
                "the edit at [{start}, {end}) intersects the functional comment {} at [{}, {}) (tags: {})",
                entry.id,
                entry.start,
                entry.end,
                tags.join(", ")
            )));
        }
    }
    Ok(())
}
