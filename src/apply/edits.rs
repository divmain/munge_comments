//! Edit computation and application: a file's records and decisions
//! turned into byte-offset [`Edit`]s against the original text, and those
//! edits applied in one pass.
//!
//! Per decision ([`compute_edits`]):
//!
//! - **`REMOVE`**: all of a file's removals go through one
//!   [`compute_removal_edits`] call, so line hygiene is judged on their
//!   combined result. Every comment not removed — kept, protected or
//!   replaced — counts as code there, so no removal edit touches it.
//! - **`KEEP_ORIGINAL` / `NEEDS_REVIEW` / `KEEP_PROTECTED`**: no edit.
//! - **`KEEP_GENERATED` / `HYBRID`**: `finalComment` is normalized
//!   ([`normalize_comment_text`]), validated for the record's slot
//!   ([`validate_final_comment`]) and rendered ([`render_comment`]) over
//!   exactly the record's `[start, end)`.
//! - **`NEW`** (NEW-origin records only, and only with
//!   [`EditOpts::allow_add`]): the same normalization and validation, then
//!   a zero-length insertion at `start`. A leading insertion is
//!   `{indent}{rendered}{eol}` (a line of its own before the anchor line);
//!   a trailing one is the rendered text alone, which already carries its
//!   gap.
//! - **JSX backstop**: a non-keep decision on a record whose current
//!   inventory entry is tagged `jsx-expression` is skipped.
//!
//! A validation failure, a JSX container and a NEW without `allowAdd` skip
//! that one edit (recorded in [`ComputedEdits::skipped`]); the record's
//! original text stays.
//!
//! **Edit invariants** (checked by both [`compute_edits`] and
//! [`apply_edits`]): edits are ordered by start; at a shared start,
//! insertions (by record id) precede the other edit; every endpoint is in
//! bounds and on a `char` boundary; no two edits overlap, and no insertion
//! lies strictly inside another edit's deleted range. An insertion from a
//! NEW-origin record that lands inside a deleted range is a bad
//! (hand-edited) offset, [`Error::Artifact`]; any other violation is a bug,
//! [`Error::Invariant`].

use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::render::{Slot, render_comment, slot_for};
use super::types::{SkippedEdit, SkippedEditReason};
use crate::artifact::types::{CommentRecord, Decision, FilePlan};
use crate::comments::hygiene::compute_removal_edits;
use crate::comments::inventory::{Inventory, InventoryComment, JSX_EXPRESSION};
use crate::comments::sanitize::{normalize_comment_text, validate_final_comment};
use crate::comments::types::Edit;
use crate::errors::Error;
use crate::lang::adapter::LanguageAdapter;

/// Options for [`compute_edits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditOpts {
    /// Whether NEW-origin records decided `NEW` are inserted (the
    /// artifact's `config.allowAdd`).
    pub allow_add: bool,
}

/// The edits [`compute_edits`] produced for one file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComputedEdits {
    /// In application order (see the module's edit invariants), ready for
    /// [`apply_edits`].
    pub edits: Vec<Edit>,
    /// Edits not made, in record order.
    pub skipped: Vec<SkippedEdit>,
}

/// Where an edit came from, for ordering and error attribution.
#[derive(Debug, Clone, Copy)]
enum Origin<'a> {
    /// A record's own edit (a rewrite or a NEW insertion).
    Record { id: &'a str, new_origin: bool },
    /// A removal-hygiene edit (one or more `REMOVE` records' lines).
    Removal,
}

impl Origin<'_> {
    /// Tie order among insertions at one offset: by record id. Ids match
    /// `^[CN][1-9][0-9]*$`, so (prefix, length, text) is numeric order.
    fn id_key(&self) -> Option<(Option<&str>, usize, &str)> {
        match self {
            Origin::Record { id, .. } => Some((id.get(..1), id.len(), id)),
            Origin::Removal => None,
        }
    }

    fn describe(&self) -> String {
        match self {
            Origin::Record { id, .. } => format!("record {id}"),
            Origin::Removal => "removal".to_string(),
        }
    }
}

/// An edit tagged with its origin.
struct Planned<'a> {
    edit: Edit,
    origin: Origin<'a>,
}

/// The edits `file_plan`'s decisions make to `text` (the file's current
/// text, already hash-checked against the plan), per the module rules.
/// `inventory` is `text`'s current inventory; every scanned record with an
/// edit must match one of its records' range exactly.
///
/// # Errors
///
/// - [`Error::Artifact`] when the plan does not fit the text: a scanned
///   record with an edit that matches no current inventory record (or the
///   same one as another record), a record whose range or kind does not
///   fit the text (see [`slot_for`]), a decision its origin cannot carry
///   (`NEW` on a scanned record; anything but `NEW`/`NEEDS_REVIEW` on a
///   NEW-origin record), a missing required `finalComment`, or a NEW-origin
///   insertion strictly inside another edit's deleted range.
/// - [`Error::Invariant`] for any other violated edit invariant.
pub fn compute_edits(
    text: &str,
    file_plan: &FilePlan,
    inventory: &Inventory,
    adapter: &LanguageAdapter,
    opts: EditOpts,
) -> Result<ComputedEdits, Error> {
    let path = file_plan.path.as_str();
    let mut planned: Vec<Planned> = Vec::new();
    let mut skipped = Vec::new();
    let mut to_remove: Vec<&InventoryComment> = Vec::new();
    // Inventory index → id of the record editing it.
    let mut claimed: BTreeMap<usize, &str> = BTreeMap::new();
    let mut skip = |record: &CommentRecord, reason| {
        skipped.push(SkippedEdit {
            id: record.id.clone(),
            reason,
        });
    };

    for record in &file_plan.comments {
        let new_origin = record.is_new_origin();
        match record.decision {
            Decision::KeepOriginal | Decision::NeedsReview | Decision::KeepProtected => {}
            Decision::New if new_origin => {
                if !opts.allow_add {
                    skip(record, SkippedEditReason::AllowAddOff);
                    continue;
                }
                let slot = slot_for(text, record, adapter)?;
                let Some(body) = valid_final_comment(path, record, &slot, adapter)? else {
                    skip(record, SkippedEditReason::FinalCommentInvalid);
                    continue;
                };
                let rendered = render_comment(&body, &slot);
                let replacement = if slot.trailing {
                    rendered
                } else {
                    format!("{}{rendered}{}", slot.indent, slot.eol)
                };
                planned.push(Planned {
                    edit: Edit {
                        start: record.start,
                        length: 0,
                        replacement,
                    },
                    origin: Origin::Record {
                        id: &record.id,
                        new_origin,
                    },
                });
            }
            Decision::New => return Err(wrong_origin(path, record)),
            Decision::Remove | Decision::KeepGenerated | Decision::Hybrid if new_origin => {
                return Err(wrong_origin(path, record));
            }
            Decision::Remove | Decision::KeepGenerated | Decision::Hybrid => {
                let (index, entry) = current_entry(path, record, inventory)?;
                if let Some(other) = claimed.insert(index, &record.id) {
                    return Err(Error::Artifact(format!(
                        "{path}: records {other} and {} both edit the comment at [{}, {})",
                        record.id, entry.start, entry.end
                    )));
                }
                if entry.tags.contains(JSX_EXPRESSION) {
                    skip(record, SkippedEditReason::JsxExpression);
                    continue;
                }
                if record.decision == Decision::Remove {
                    to_remove.push(entry);
                    continue;
                }
                let slot = slot_for(text, record, adapter)?;
                let Some(body) = valid_final_comment(path, record, &slot, adapter)? else {
                    skip(record, SkippedEditReason::FinalCommentInvalid);
                    continue;
                };
                planned.push(Planned {
                    edit: Edit {
                        start: record.start,
                        length: record.end - record.start,
                        replacement: render_comment(&body, &slot),
                    },
                    origin: Origin::Record {
                        id: &record.id,
                        new_origin,
                    },
                });
            }
        }
    }

    planned.extend(
        compute_removal_edits(text, &to_remove)
            .into_iter()
            .map(|edit| Planned {
                edit,
                origin: Origin::Removal,
            }),
    );
    planned.sort_by(|a, b| {
        application_order(&a.edit, &b.edit).then_with(|| a.origin.id_key().cmp(&b.origin.id_key()))
    });
    check_planned(path, text, &planned)?;
    Ok(ComputedEdits {
        edits: planned.into_iter().map(|planned| planned.edit).collect(),
        skipped,
    })
}

/// A decision the record's origin cannot carry (validation rejects these;
/// `compute_edits` does not assume a validated plan).
fn wrong_origin(path: &str, record: &CommentRecord) -> Error {
    Error::Artifact(format!(
        "{path}: {} record {} cannot be decided {}",
        if record.is_new_origin() {
            "a NEW-origin"
        } else {
            "a scanned"
        },
        record.id,
        record.decision.as_str()
    ))
}

/// The current inventory record (and its index) whose range is exactly
/// the scanned `record`'s.
fn current_entry<'a>(
    path: &str,
    record: &CommentRecord,
    inventory: &'a Inventory,
) -> Result<(usize, &'a InventoryComment), Error> {
    inventory
        .comments
        .binary_search_by_key(&record.start, |entry| entry.start)
        .ok()
        .map(|index| (index, &inventory.comments[index]))
        .filter(|(_, entry)| entry.end == record.end)
        .ok_or_else(|| {
            Error::Artifact(format!(
                "{path}: record {} range [{}, {}) matches no comment in the current file",
                record.id, record.start, record.end
            ))
        })
}

/// `record`'s `finalComment`, normalized, if it may be written into
/// `slot`; `None` when validation rejects it.
fn valid_final_comment(
    path: &str,
    record: &CommentRecord,
    slot: &Slot,
    adapter: &LanguageAdapter,
) -> Result<Option<String>, Error> {
    let final_comment = record.final_comment.as_deref().ok_or_else(|| {
        Error::Artifact(format!(
            "{path}: record {} is decided {} but has no finalComment",
            record.id,
            record.decision.as_str()
        ))
    })?;
    let normalized = normalize_comment_text(final_comment, adapter);
    Ok(validate_final_comment(&normalized, slot.kind, adapter)
        .is_ok()
        .then_some(normalized))
}

/// Application order: by start; at a shared start, a zero-length
/// insertion before a non-empty edit. (Ties among insertions are broken by
/// record id in [`compute_edits`] and kept stable by [`apply_edits`].)
pub(super) fn application_order(a: &Edit, b: &Edit) -> Ordering {
    a.start
        .cmp(&b.start)
        .then_with(|| (a.length != 0).cmp(&(b.length != 0)))
}

/// `edit`'s end offset, or a description of why its range does not fit
/// `text` (out of bounds, or an endpoint off a `char` boundary).
fn checked_end(text: &str, edit: &Edit) -> Result<usize, String> {
    let end = edit
        .start
        .checked_add(edit.length)
        .filter(|&end| end <= text.len())
        .ok_or_else(|| {
            format!(
                "edit [{}, +{}) is out of bounds of the {}-byte text",
                edit.start,
                edit.length,
                text.len()
            )
        })?;
    if !text.is_char_boundary(edit.start) || !text.is_char_boundary(end) {
        return Err(format!(
            "edit [{}, {end}) has an endpoint off a char boundary",
            edit.start
        ));
    }
    Ok(end)
}

/// Checks the edit invariants on `planned` (already in application
/// order), attributing a NEW-origin insertion inside a deleted range to
/// the artifact.
fn check_planned(path: &str, text: &str, planned: &[Planned]) -> Result<(), Error> {
    let mut previous: Option<(&Planned, usize)> = None;
    for current in planned {
        let end = checked_end(text, &current.edit).map_err(|reason| {
            Error::Invariant(format!("{path}: {}: {reason}", current.origin.describe()))
        })?;
        if let Some((before, before_end)) = previous
            && current.edit.start < before_end
        {
            let inside = format!(
                "{} at [{}, {end}) lies inside the range [{}, {before_end}) edited by {}",
                current.origin.describe(),
                current.edit.start,
                before.edit.start,
                before.origin.describe()
            );
            return Err(match current.origin {
                Origin::Record {
                    new_origin: true, ..
                } if current.edit.length == 0 => Error::Artifact(format!(
                    "{path}: NEW {inside}; a NEW record must not be placed in text another edit deletes"
                )),
                _ => Error::Invariant(format!("{path}: overlapping edits: {inside}")),
            });
        }
        previous = Some((current, end));
    }
    Ok(())
}

/// Applies `edits` (byte offsets into `text`, the original) in one pass.
/// Edits may come in any order: they are put in application order (a
/// stable sort, so insertions sharing an offset keep their given order;
/// [`compute_edits`] already orders them by record id).
///
/// # Errors
///
/// [`Error::Invariant`] when an edit is out of bounds or off a `char`
/// boundary, or two edits overlap (including an insertion strictly inside
/// another edit's deleted range).
pub fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, Error> {
    let mut ordered: Vec<&Edit> = edits.iter().collect();
    ordered.sort_by(|a, b| application_order(a, b));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for edit in ordered {
        let end = checked_end(text, edit).map_err(Error::Invariant)?;
        if edit.start < cursor {
            return Err(Error::Invariant(format!(
                "overlapping edits: edit [{}, {end}) starts inside the previous edit, which ends at {cursor}",
                edit.start
            )));
        }
        out.push_str(&text[cursor..edit.start]);
        out.push_str(&edit.replacement);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}
