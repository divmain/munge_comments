//! Judge orchestration: call #2 for one file, and the post-judge rails.
//!
//! [`judge_file`] turns a file's draft records into final artifact
//! records:
//!
//! 1. **Pass-through.** Drafts that already carry a decision (protected,
//!    rows 1b–3, NEW-origin records) are left untouched.
//! 2. **Routing.** [`route`] runs over the undecided drafts: a
//!    short-circuit (rows 1–5) is decided `decidedBy: heuristic` at
//!    confidence 1; everything else becomes a judge [`Packet`] carrying its
//!    allowed decision set (reduced, restricted or full).
//! 3. **Batching.** Packets are packed greedily, in record order, into
//!    batches of at most `max_file_chars / 4` estimated tokens (a token is
//!    `ceil(chars / 4)` of the packet's rendered prompt section); batches
//!    are never split by decision set. One [`complete_typed`] call per
//!    batch. A truncated reply is retried as two halves of the batch's
//!    packets, at most [`MAX_SPLIT_DEPTH`] levels deep.
//! 4. **Rails.** Each verdict passes the post-judge rails in their fixed
//!    order, first match wins: (1) [`validate_judge_response`]; (2) a
//!    rewrite failing sanitization for its slot kind → `KEEP_ORIGINAL` +
//!    [`SANITIZE_FAILED`]; (3) a tie → `KEEP_ORIGINAL`; (4) the DIFFICULT
//!    downgrade → `NEEDS_REVIEW`; (5) confidence below
//!    [`CONFIDENCE_FLOOR`] → `NEEDS_REVIEW`. A rail decides as the engine
//!    at confidence 1, recording the judge's decision and confidence in
//!    `decisionRationale` and keeping its `finalComment`.
//! 5. **Finalize.** Every record goes through [`finalize_record`].

use std::collections::HashMap;

use crate::apply::render::scanned_slot_kind;
use crate::artifact::types::{CommentRecord, DecidedBy, Decision, Difficulty, FileWarning};
use crate::comments::sanitize::{match_key, normalize_comment_text, validate_final_comment};
use crate::config::{Mode, PlanConfig};
use crate::errors::{Error, LlmErrorReason};
use crate::lang::adapter::LanguageAdapter;
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::llm::call::{JudgeOutcome, Packet, complete_typed, validate_judge_response};
use crate::llm::client::{CallKind, ChatRequest, LlmClient, LlmMeta, MetaItem};
use crate::llm::prompts::{JudgePromptInput, build_judge_messages, render_packet};
use crate::llm::schemas::JudgeResponse;
use crate::log::Log;

use super::heuristics::{Route, route, similarity};
use super::records::{DraftRecord, finalize_record};
use super::regenerate::{FileInput, MAX_SPLIT_DEPTH};

/// The record tag rail 2 adds when the judge's text fails sanitization.
pub const SANITIZE_FAILED: &str = "sanitize-failed";

/// Rail 3 (row 11): a rewrite whose similarity to the original reaches
/// this is a tie.
pub const TIE_SIMILARITY: f64 = 0.8;

/// Rail 5 (row 10): a judge verdict below this confidence goes to review.
pub const CONFIDENCE_FLOOR: f64 = 0.6;

/// The result of call #2 for one file.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeOutput {
    /// Every record of the file, finalized, in the order of the drafts.
    pub records: Vec<CommentRecord>,
    /// The judge's non-fatal events (`unknown-id`), in call order.
    pub warnings: Vec<FileWarning>,
}

/// Decides every undecided draft of one file — by a heuristic
/// short-circuit or by the judge — and finalizes all of them.
///
/// `drafts` are the file's records (usually [`RegenOutput::records`]),
/// `mode` the mode the file was processed in (its effective mode), carried
/// into each request's [`LlmMeta`]. Decided drafts pass through untouched;
/// a file whose undecided drafts all short-circuit makes **zero** calls.
/// Each packet presents the draft's anchor snippet, original,
/// regeneration, difficulty (with rationale) and `wouldYouCommentHere`,
/// with the allowed set [`route`] chose: reduced without a regeneration,
/// restricted for EASY, full for MEDIUM/DIFFICULT.
///
/// [`RegenOutput::records`]: super::regenerate::RegenOutput::records
///
/// # Errors
///
/// - [`Error::Llm`] from any call, including a truncation that can no
///   longer be split (a single packet, or depth [`MAX_SPLIT_DEPTH`]); the
///   file fails with `failureKind: llm`.
/// - [`Error::Invariant`] for an undecided NEW-origin draft, a scanned
///   draft whose original is not a comment of the language, or a draft
///   left undecided at finalization (caller bugs).
pub async fn judge_file(
    input: FileInput<'_>,
    mode: Mode,
    mut drafts: Vec<DraftRecord>,
    config: &PlanConfig,
    llm: &dyn LlmClient,
    log: &dyn Log,
) -> Result<JudgeOutput, Error> {
    let adapter = adapter_for(input.language);
    let mut packets = Vec::new();
    for draft in drafts.iter_mut().filter(|draft| draft.decision.is_none()) {
        let Some(original) = draft.original.clone() else {
            return Err(Error::Invariant(format!(
                "{}: NEW-origin record {} reached the judge undecided",
                input.path, draft.id
            )));
        };
        match route(draft, config.similarity_threshold, adapter) {
            Route::Decided { decision, tags } => {
                draft.add_tags(tags);
                draft.decide(decision, DecidedBy::Heuristic, 1.0);
            }
            Route::Judge { allowed } => packets.push(Packet {
                id: draft.id.clone(),
                anchor_snippet: draft.anchor.snippet.clone(),
                original,
                regenerated: draft.regenerated.clone(),
                difficulty: draft.difficulty,
                difficulty_rationale: draft.difficulty_rationale.clone(),
                would_you_comment_here: draft.would_you_comment_here,
                allowed,
            }),
        }
    }

    let mut warnings = Vec::new();
    if !packets.is_empty() {
        let budget = usize::try_from(config.max_file_chars / 4).unwrap_or(usize::MAX);
        let batches = batch_packets(packets, budget, input.language);
        let index: HashMap<String, usize> = drafts
            .iter()
            .enumerate()
            .map(|(position, draft)| (draft.id.clone(), position))
            .collect();
        let ctx = JudgeCtx {
            input,
            mode,
            model: &config.model,
            llm,
            log,
            drafts: &drafts,
            index: &index,
        };
        let mut verdicts = Vec::new();
        for (batch_index, batch) in batches.into_iter().enumerate() {
            let (outcomes, batch_warnings) = ctx.judge_batch(batch_index, batch).await?;
            verdicts.extend(outcomes);
            warnings.extend(batch_warnings);
        }
        for (id, outcome) in verdicts {
            let draft = index
                .get(&id)
                .map(|&position| &mut drafts[position])
                .ok_or_else(|| {
                    Error::Invariant(format!(
                        "{}: the judge's verdict for {id} matches no record",
                        input.path
                    ))
                })?;
            apply_verdict(draft, outcome, adapter)?;
        }
    }

    let records = drafts
        .into_iter()
        .map(finalize_record)
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(JudgeOutput { records, warnings })
}

/// The estimated token cost of `text`: `ceil(chars / 4)`.
fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Packs `packets` greedily, in order, into batches of at most `budget`
/// estimated tokens each (measured on each packet's rendered prompt
/// section). A packet that alone exceeds the budget forms its own batch.
/// Allowed sets play no part.
fn batch_packets(packets: Vec<Packet>, budget: usize, language: LanguageId) -> Vec<Vec<Packet>> {
    let mut batches: Vec<Vec<Packet>> = Vec::new();
    let mut current = Vec::new();
    let mut used = 0_usize;
    for packet in packets {
        let cost = estimate_tokens(&render_packet(&packet, language));
        if !current.is_empty() && used.saturating_add(cost) > budget {
            batches.push(std::mem::take(&mut current));
            used = 0;
        }
        used = used.saturating_add(cost);
        current.push(packet);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// The per-file context shared by every judge request of one file.
struct JudgeCtx<'a> {
    input: FileInput<'a>,
    mode: Mode,
    model: &'a str,
    llm: &'a dyn LlmClient,
    log: &'a dyn Log,
    /// The file's drafts (the packets' records are among them) …
    drafts: &'a [DraftRecord],
    /// … and each draft's index there, by id.
    index: &'a HashMap<String, usize>,
}

impl JudgeCtx<'_> {
    /// The routing data of a judge request over `packets` of batch
    /// `batch_index`.
    fn meta(&self, batch_index: usize, packets: &[Packet]) -> Result<LlmMeta, Error> {
        let items = packets
            .iter()
            .map(|packet| {
                let draft = self
                    .index
                    .get(&packet.id)
                    .map(|&position| &self.drafts[position])
                    .ok_or_else(|| {
                        Error::Invariant(format!(
                            "{}: judge packet {} matches no record",
                            self.input.path, packet.id
                        ))
                    })?;
                Ok(MetaItem {
                    id: packet.id.clone(),
                    original: Some(packet.original.clone()),
                    regenerated: packet.regenerated.clone(),
                    anchor: draft.anchor.clone(),
                    position: draft.position,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(LlmMeta {
            call: CallKind::Judge,
            mode: self.mode,
            path: self.input.path.to_owned(),
            language: self.input.language,
            chunk_index: batch_index,
            ids: packets.iter().map(|packet| packet.id.clone()).collect(),
            items,
        })
    }

    /// Judges one batch, splitting on truncation, and returns every
    /// packet's rail-1 outcome (see [`validate_judge_response`]) with the
    /// `unknown-id` warnings.
    ///
    /// Each truncated request is replaced by its two halves (first half
    /// first), one level deeper; a truncation of a single packet or at
    /// [`MAX_SPLIT_DEPTH`] fails the file.
    async fn judge_batch(
        &self,
        batch_index: usize,
        batch: Vec<Packet>,
    ) -> Result<(Vec<(String, JudgeOutcome)>, Vec<FileWarning>), Error> {
        let mut outcomes = Vec::new();
        let mut warnings = Vec::new();
        // A stack of pending requests: the last pushed is sent next.
        let mut pending = vec![(batch, 0_u32)];
        while let Some((packets, depth)) = pending.pop() {
            let messages = build_judge_messages(&JudgePromptInput {
                language: self.input.language,
                path: self.input.path,
                packets: &packets,
            });
            let request = ChatRequest::new(self.model, messages, self.meta(batch_index, &packets)?);
            match complete_typed::<JudgeResponse>(self.llm, request).await {
                Ok(response) => {
                    let (answered, unknown) = validate_judge_response(response, &packets);
                    outcomes.extend(answered);
                    warnings.extend(unknown);
                }
                Err(Error::Llm {
                    reason: LlmErrorReason::Truncated,
                    message,
                }) => {
                    if packets.len() == 1 || depth >= MAX_SPLIT_DEPTH {
                        return Err(Error::Llm {
                            reason: LlmErrorReason::Truncated,
                            message: format!(
                                "{}: judge batch {batch_index} of {} packet(s) at split depth \
                                 {depth} cannot be split further: {message}",
                                self.input.path,
                                packets.len()
                            ),
                        });
                    }
                    let mut first = packets;
                    let second = first.split_off(first.len().div_ceil(2));
                    self.log.info(&format!(
                        "{}: judge batch {batch_index} truncated; retrying its {} packets as \
                         {} + {}",
                        self.input.path,
                        first.len() + second.len(),
                        first.len(),
                        second.len()
                    ));
                    pending.push((second, depth + 1));
                    pending.push((first, depth + 1));
                }
                Err(error) => return Err(error),
            }
        }
        Ok((outcomes, warnings))
    }
}

/// Records the judge's verdict on `draft`, enforcing the post-judge rails
/// in their fixed order — first match wins:
///
/// 1. [`validate_judge_response`] (already applied: an outcome decided by
///    [`DecidedBy::Engine`] is a rail-1 `NEEDS_REVIEW` and is copied as
///    is, skipping rails 2–5);
/// 2. a writing decision (`KEEP_GENERATED`/`HYBRID`) whose `finalComment`
///    fails [`validate_final_comment`] for the record's slot kind →
///    `KEEP_ORIGINAL` + tag [`SANITIZE_FAILED`];
/// 3. a tie — a writing decision whose `finalComment` has similarity ≥
///    [`TIE_SIMILARITY`] to the original, or the same `match_key` →
///    `KEEP_ORIGINAL`;
/// 4. the DIFFICULT downgrade: on a DIFFICULT record, any decision but
///    `KEEP_ORIGINAL` → `NEEDS_REVIEW`;
/// 5. the confidence floor: confidence < [`CONFIDENCE_FLOOR`] →
///    `NEEDS_REVIEW`.
///
/// Rails 4–5 route to review, so they do not fire on a verdict that
/// already is `NEEDS_REVIEW`. A railed verdict is decided by the engine
/// at confidence 1, with the judge's decision, confidence and rationale
/// recorded in `decisionRationale`; an unrailed one is the judge's, with
/// its confidence and rationale. Either way the judge's `finalComment`
/// is kept (normalized with [`normalize_comment_text`]), so a reviewer can
/// approve a held rewrite by flipping only `decision`.
///
/// # Errors
///
/// [`Error::Invariant`] when `draft` has no original, or its original is
/// not a comment of the language (so it has no slot kind).
fn apply_verdict(
    draft: &mut DraftRecord,
    outcome: JudgeOutcome,
    adapter: &LanguageAdapter,
) -> Result<(), Error> {
    let original = draft.original.clone().ok_or_else(|| {
        Error::Invariant(format!(
            "NEW-origin record {} received a judge verdict",
            draft.id
        ))
    })?;
    let final_comment = outcome
        .final_comment
        .as_deref()
        .map(|text| normalize_comment_text(text, adapter));
    draft.final_comment = final_comment.clone();
    // Rail 1.
    if outcome.decided_by == DecidedBy::Engine {
        draft.decide(
            Decision::from(outcome.decision),
            DecidedBy::Engine,
            outcome.confidence,
        );
        draft.decision_rationale = Some(outcome.rationale);
        return Ok(());
    }
    let decision = Decision::from(outcome.decision);
    let rail = match final_comment.as_deref() {
        // Rails 2–3 judge only text that would be written.
        Some(text) if decision.writes_final_comment() => {
            sanitize_rail(draft, &original, text, adapter)?
                .or_else(|| tie_rail(&original, text, adapter))
        }
        _ => None,
    };
    let rail = rail.or_else(|| {
        if decision == Decision::NeedsReview {
            None
        } else if draft.difficulty == Some(Difficulty::Difficult)
            && decision != Decision::KeepOriginal
        {
            // Rail 4.
            Some(Rail {
                decision: Decision::NeedsReview,
                tag: None,
                reason: "a DIFFICULT comment keeps its original or goes to review".to_owned(),
            })
        } else if outcome.confidence < CONFIDENCE_FLOOR {
            // Rail 5.
            Some(Rail {
                decision: Decision::NeedsReview,
                tag: None,
                reason: format!(
                    "confidence {} is below the {CONFIDENCE_FLOOR} floor",
                    outcome.confidence
                ),
            })
        } else {
            None
        }
    });
    match rail {
        Some(rail) => {
            draft.add_tags(rail.tag);
            draft.decide(rail.decision, DecidedBy::Engine, 1.0);
            draft.decision_rationale = Some(format!(
                "rail: {}; the judge decided {} at confidence {}: {}",
                rail.reason,
                decision.as_str(),
                outcome.confidence,
                outcome.rationale
            ));
        }
        None => {
            draft.decide(decision, DecidedBy::Judge, outcome.confidence);
            draft.decision_rationale = Some(outcome.rationale);
        }
    }
    Ok(())
}

/// A post-judge rail that fired: the decision it imposes, the tag it adds
/// and why.
struct Rail {
    decision: Decision,
    tag: Option<&'static str>,
    reason: String,
}

/// Rail 2: `text` (normalized) must be writable into `draft`'s slot.
fn sanitize_rail(
    draft: &DraftRecord,
    original: &str,
    text: &str,
    adapter: &LanguageAdapter,
) -> Result<Option<Rail>, Error> {
    let slot_kind = scanned_slot_kind(original, draft.kind, adapter).ok_or_else(|| {
        Error::Invariant(format!(
            "record {}'s original is not a {} comment",
            draft.id,
            adapter.id.as_str()
        ))
    })?;
    Ok(validate_final_comment(text, slot_kind, adapter)
        .err()
        .map(|reason| Rail {
            decision: Decision::KeepOriginal,
            tag: Some(SANITIZE_FAILED),
            reason: format!("the finalComment {reason}"),
        }))
}

/// Rail 3 (row 11): a rewrite comparable to the original is a tie.
fn tie_rail(original: &str, text: &str, adapter: &LanguageAdapter) -> Option<Rail> {
    let reason = if match_key(text, adapter) == match_key(original, adapter) {
        "tie: the finalComment has the original's match key".to_owned()
    } else {
        let score = similarity(text, original, adapter);
        if score < TIE_SIMILARITY {
            return None;
        }
        format!(
            "tie: the finalComment's similarity to the original is {score:.2} (≥ {TIE_SIMILARITY})"
        )
    };
    Some(Rail {
        decision: Decision::KeepOriginal,
        tag: None,
        reason,
    })
}

#[cfg(test)]
#[path = "judge_tests.rs"]
mod tests;
