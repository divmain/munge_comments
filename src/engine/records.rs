//! Draft records: the in-flight form of a [`CommentRecord`] while the
//! engine decides it, the single record builder, and finalization.
//!
//! [`build_draft_records`] is the only place scanned records are built
//! from an inventory, so ranges, kinds, positions, classifications, tags,
//! originals and anchors are filled identically on every path (LLM,
//! offline, resume). It decides what needs no model: protected records
//! (row 1) and the pre-LLM rows 1b–3. Later stages fill in the model
//! fields and decide the rest; [`finalize_record`] turns a fully decided
//! draft into an artifact record.

use crate::artifact::types::{CommentRecord, DecidedBy, Decision, Difficulty};
use crate::comments::inventory::{Inventory, InventoryComment};
use crate::comments::types::{Anchor, Classification, CommentKind, Position};
use crate::errors::Error;
use crate::lang::adapter::LanguageAdapter;

use super::heuristics::pre_llm_decision;

/// A [`CommentRecord`] under construction: identical, except that the
/// decision, its confidence and its decider are not known yet.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftRecord {
    /// See [`CommentRecord::id`].
    pub id: String,
    /// See [`CommentRecord::kind`].
    pub kind: CommentKind,
    /// See [`CommentRecord::start`].
    pub start: usize,
    /// See [`CommentRecord::end`].
    pub end: usize,
    /// See [`CommentRecord::line`].
    pub line: u32,
    /// See [`CommentRecord::column`].
    pub column: u32,
    /// See [`CommentRecord::position`].
    pub position: Position,
    /// See [`CommentRecord::classification`].
    pub classification: Classification,
    /// Sorted, deduplicated tags (maintain with [`DraftRecord::add_tags`]).
    pub tags: Vec<String>,
    /// See [`CommentRecord::original`].
    pub original: Option<String>,
    /// See [`CommentRecord::anchor`].
    pub anchor: Anchor,
    /// See [`CommentRecord::regenerated`].
    pub regenerated: Option<String>,
    /// See [`CommentRecord::would_you_comment_here`].
    pub would_you_comment_here: Option<bool>,
    /// See [`CommentRecord::difficulty`].
    pub difficulty: Option<Difficulty>,
    /// See [`CommentRecord::difficulty_rationale`].
    pub difficulty_rationale: Option<String>,
    /// See [`CommentRecord::missing_context`].
    pub missing_context: Option<String>,
    /// The decision, once made.
    pub decision: Option<Decision>,
    /// See [`CommentRecord::final_comment`].
    pub final_comment: Option<String>,
    /// The decision's confidence, once made.
    pub confidence: Option<f64>,
    /// See [`CommentRecord::decision_rationale`].
    pub decision_rationale: Option<String>,
    /// Who decided, once decided.
    pub decided_by: Option<DecidedBy>,
}

impl DraftRecord {
    /// An undecided scanned record built from one inventory comment, with
    /// every model field null.
    fn from_inventory(comment: &InventoryComment) -> Self {
        DraftRecord {
            id: comment.id.clone(),
            kind: comment.kind,
            start: comment.start,
            end: comment.end,
            line: comment.line,
            column: comment.column,
            position: comment.position,
            classification: comment.classification,
            // A BTreeSet iterates sorted and deduplicated.
            tags: comment.tags.iter().cloned().collect(),
            original: Some(comment.text.clone()),
            anchor: comment.anchor.clone(),
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

    /// Whether this is a NEW-origin record (no original text).
    pub fn is_new_origin(&self) -> bool {
        self.original.is_none()
    }

    /// Whether the record carries `tag`.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|own| own == tag)
    }

    /// Adds `tags`, keeping the list sorted and deduplicated.
    pub fn add_tags<I, S>(&mut self, tags: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags.extend(tags.into_iter().map(Into::into));
        self.tags.sort();
        self.tags.dedup();
    }

    /// Records a decision with its decider and confidence.
    pub fn decide(&mut self, decision: Decision, decided_by: DecidedBy, confidence: f64) {
        self.decision = Some(decision);
        self.decided_by = Some(decided_by);
        self.confidence = Some(confidence);
    }
}

/// Builds the draft records of one file — the single record builder.
///
/// Every inventory comment becomes one record, in inventory (ascending
/// offset) order, with ranges, kind, position, classification, tags,
/// original and anchor filled and every model field null. Then:
///
/// - protected records (row 1: any non-prose classification) are decided
///   `KEEP_PROTECTED`, `decidedBy: protected`, `confidence: 1`;
/// - [`pre_llm_decision`] (rows 1b–3) decides what it can, `decidedBy:
///   heuristic`, `confidence: 1`, adding the row's tags;
/// - everything else stays undecided (`decision: None`).
///
/// `adapter` must be the adapter of the language `inventory` was taken
/// under (row 3 reads the comment body through it).
pub fn build_draft_records(inventory: &Inventory, adapter: &LanguageAdapter) -> Vec<DraftRecord> {
    inventory
        .comments
        .iter()
        .map(|comment| {
            let mut record = DraftRecord::from_inventory(comment);
            if record.classification != Classification::Prose {
                record.decide(Decision::KeepProtected, DecidedBy::Protected, 1.0);
            } else if let Some((decision, tags)) = pre_llm_decision(&record, adapter) {
                record.add_tags(tags);
                record.decide(decision, DecidedBy::Heuristic, 1.0);
            }
            record
        })
        .collect()
}

/// Turns a fully decided draft into an artifact record.
///
/// # Errors
///
/// [`Error::Invariant`] when the decision, its confidence or its decider is
/// missing — every record must be decided before it is written.
pub fn finalize_record(draft: DraftRecord) -> Result<CommentRecord, Error> {
    let missing = |field: &str| {
        Error::Invariant(format!(
            "record {} reached finalization without a {field}",
            draft.id
        ))
    };
    let decision = draft.decision.ok_or_else(|| missing("decision"))?;
    let confidence = draft.confidence.ok_or_else(|| missing("confidence"))?;
    let decided_by = draft.decided_by.ok_or_else(|| missing("decider"))?;
    Ok(CommentRecord {
        id: draft.id,
        kind: draft.kind,
        start: draft.start,
        end: draft.end,
        line: draft.line,
        column: draft.column,
        position: draft.position,
        classification: draft.classification,
        tags: draft.tags,
        original: draft.original,
        anchor: draft.anchor,
        regenerated: draft.regenerated,
        would_you_comment_here: draft.would_you_comment_here,
        difficulty: draft.difficulty,
        difficulty_rationale: draft.difficulty_rationale,
        missing_context: draft.missing_context,
        decision,
        final_comment: draft.final_comment,
        confidence,
        decision_rationale: draft.decision_rationale,
        decided_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::comments::inventory::{FileRef, inventory_file};
    use crate::lang::adapters::adapter_for;
    use crate::lang::ids::LanguageId;

    fn drafts(text: &str) -> Vec<DraftRecord> {
        let inventory = inventory_file(
            text,
            FileRef {
                language: LanguageId::Javascript,
                file_name: "a.js".to_owned(),
            },
        )
        .expect("test input inventories");
        build_draft_records(&inventory, adapter_for(LanguageId::Javascript))
    }

    #[test]
    fn add_tags_keeps_the_list_sorted_and_deduplicated() {
        let mut record = drafts("// FIXME and HACK\na();\n").remove(0);
        record.add_tags(["offline", "fixme", "flagged-for-review"]);
        assert_eq!(
            record.tags,
            ["fixme", "flagged-for-review", "hack", "offline"]
        );
    }

    #[test]
    fn finalize_requires_a_complete_decision() {
        let undecided = drafts("// keeps a running total\nsum += x;\n").remove(0);
        assert_eq!(undecided.decision, None);
        let error = finalize_record(undecided.clone()).expect_err("undecided");
        assert!(
            matches!(&error, Error::Invariant(message) if message.contains("C1") && message.contains("decision")),
            "{error:?}"
        );
        for strip in [
            (|record: &mut DraftRecord| record.confidence = None) as fn(&mut DraftRecord),
            |record: &mut DraftRecord| record.decided_by = None,
        ] {
            let mut partial = undecided.clone();
            partial.decide(Decision::KeepOriginal, DecidedBy::Judge, 0.9);
            strip(&mut partial);
            assert!(matches!(finalize_record(partial), Err(Error::Invariant(_))));
        }
    }

    #[test]
    fn finalize_carries_every_field_over() {
        let mut draft = drafts("// keeps a running total\nsum += x;\n").remove(0);
        draft.regenerated = Some("Accumulate the sum.".to_owned());
        draft.would_you_comment_here = Some(true);
        draft.difficulty = Some(Difficulty::Medium);
        draft.difficulty_rationale = Some("needs context".to_owned());
        draft.missing_context = Some("why a running total".to_owned());
        draft.final_comment = Some("// Accumulate the sum.".to_owned());
        draft.decision_rationale = Some("clearer".to_owned());
        draft.decide(Decision::KeepGenerated, DecidedBy::Judge, 0.82);
        let record = finalize_record(draft.clone()).expect("decided");
        assert_eq!(record.id, draft.id);
        assert_eq!((record.start, record.end), (draft.start, draft.end));
        assert_eq!((record.line, record.column), (draft.line, draft.column));
        assert_eq!(record.kind, draft.kind);
        assert_eq!(record.position, draft.position);
        assert_eq!(record.classification, draft.classification);
        assert_eq!(record.tags, draft.tags);
        assert_eq!(record.original, draft.original);
        assert_eq!(record.anchor, draft.anchor);
        assert_eq!(record.regenerated, draft.regenerated);
        assert_eq!(record.would_you_comment_here, Some(true));
        assert_eq!(record.difficulty, Some(Difficulty::Medium));
        assert_eq!(record.difficulty_rationale, draft.difficulty_rationale);
        assert_eq!(record.missing_context, draft.missing_context);
        assert_eq!(record.final_comment, draft.final_comment);
        assert_eq!(record.decision_rationale, draft.decision_rationale);
        assert_eq!(record.decision, Decision::KeepGenerated);
        assert_eq!(record.confidence, 0.82);
        assert_eq!(record.decided_by, DecidedBy::Judge);
    }
}
