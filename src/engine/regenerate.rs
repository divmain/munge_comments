//! Regeneration orchestration: call #1 (blind regeneration) for one file.
//!
//! [`regenerate_file`] owns everything between a file's inventory and its
//! regenerated draft records:
//!
//! 0. **Mode.** A redact-mode file already holding placeholder-like text
//!    (`⟦C<digits>⟧`) falls back to remove mode, with a
//!    `placeholder-collision` warning; the effective mode is recorded even
//!    for a file that then makes no call.
//! 1. **Start.** The records come from [`build_draft_records`], which
//!    decides what needs no model (rows 1–3). The undecided records are the
//!    call's *targets*; a file without any makes **zero** calls.
//! 2. **Chunking.** [`chunk_file`] splits the file to the
//!    `--max-file-chars` budget (its `oversized-chunk` warnings are kept)
//!    and [`build_chunk_texts`] hides exactly the targets in each chunk's
//!    prompt text. A chunk holding no target makes no call.
//! 3. **Calls.** One [`complete_typed`] call per chunk, its [`LlmMeta`]
//!    filled from the inventory, so test doubles can answer without
//!    parsing prompts. Calls run sequentially; files are what run
//!    concurrently.
//! 4. **Join.** Redact mode joins answers to records by placeholder id;
//!    remove mode aligns the model's citations with the targets
//!    ([`align_remove_mode`], once, over every answered chunk) and turns
//!    the regenerations no target claims into NEW-origin records. See
//!    [`regenerate_file`] for the rules.
//!
//! Truncated output (`finish_reason: "length"`) is never repaired: the
//! request is split and retried, at most [`MAX_SPLIT_DEPTH`] levels deep —
//! redact mode halves its requested ids over the same chunk text, remove
//! mode re-cuts the chunk at half its size. A truncation that can no
//! longer be split fails the file.

use std::collections::{BTreeSet, HashMap};

use crate::artifact::types::{DecidedBy, Decision, Difficulty, FileWarning, FileWarningKind};
use crate::comments::inventory::{Inventory, InventoryComment};
use crate::comments::redact::has_placeholder_collision;
use crate::comments::sanitize::validate_final_comment;
use crate::comments::scan::line_start_of;
use crate::comments::types::{Classification, CommentKind, Position};
use crate::config::{Mode, PlanConfig};
use crate::errors::{Error, LlmErrorReason};
use crate::lang::adapter::{LanguageAdapter, SlotKind};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::llm::call::{Regeneration, complete_typed, postprocess_regeneration};
use crate::llm::chunk::{ChunkText, build_chunk_texts, chunk_file};
use crate::llm::client::{CallKind, ChatRequest, LlmClient, LlmMeta, Message, MetaItem};
use crate::llm::prompts::{RegenPromptInput, build_regeneration_messages};
use crate::llm::schemas::{
    RegenerationResponse, RemoveModeComment, RemoveModeRegenerationResponse,
};
use crate::log::Log;

use super::align::{NewCandidate, align_remove_mode};
use super::records::{DraftRecord, build_draft_records};

/// How many times one request may be split after truncated output. The
/// first request is at depth 0; a truncation at this depth (or of a request
/// that cannot be split) fails the file.
pub const MAX_SPLIT_DEPTH: u32 = 3;

/// One file, as regeneration needs it.
#[derive(Debug, Clone, Copy)]
pub struct FileInput<'a> {
    /// Root-relative POSIX path.
    pub path: &'a str,
    /// The file's language.
    pub language: LanguageId,
    /// The file's original text.
    pub text: &'a str,
    /// The inventory of `text` under `language`.
    pub inventory: &'a Inventory,
}

/// The result of call #1 for one file.
#[derive(Debug, Clone, PartialEq)]
pub struct RegenOutput {
    /// Every record of the file sorted by `(start, id)`: the scanned
    /// records (in inventory order), decided by rows 1–3 or undecided with
    /// their model fields filled from call #1, and in remove mode the
    /// NEW-origin records (`N…`), already decided `NEW` or `NEEDS_REVIEW`.
    pub records: Vec<DraftRecord>,
    /// The mode the file was processed in.
    pub effective_mode: Mode,
    /// The file's non-fatal events, in the order they occurred.
    pub warnings: Vec<FileWarning>,
}

/// Runs call #1 for one file and returns its draft records with the model
/// fields filled.
///
/// Redact mode, per chunk: the requested ids are the chunk's targets (the
/// keys of its id map). The answers are post-processed by
/// [`postprocess_regeneration`], whose `unknown-id` warnings (one per
/// unrequested id) are collected as-is, and joined to the records by id,
/// filling `regenerated`, `would_you_comment_here`, `difficulty`,
/// `difficulty_rationale` and `missing_context`. A comment the model left
/// empty is stored as `regenerated: None`, keeping its other fields (so an
/// EASY "would not comment here" still routes to row 4). A requested id the
/// model did not answer keeps every model field `None`; it reaches the judge
/// with the reduced decision set. A truncated reply is retried as two
/// halves of its requested ids over the same chunk text.
///
/// Remove mode (configured, or by the collision fallback), per chunk: the
/// request's items are the targets the chunk hides, and the model cites
/// the code each comment it would write annotates. A truncated reply is
/// retried over the chunk's range re-cut at half its size. All answered
/// (sub-)chunks are then aligned at once against the whole-file clean text
/// ([`align_remove_mode`]):
///
/// - a paired target gets `regenerated` (`None` when normalization left
///   it blank), `difficulty`, `difficulty_rationale` and
///   `missing_context`; `would_you_comment_here` stays `None`;
/// - an unmatched target keeps every model field `None` (the judge's
///   reduced set);
/// - each NEW candidate becomes a NEW-origin record: ids `N1`, `N2`, … in
///   insertion-offset order; `start == end ==` the insertion offset;
///   `kind` the file's prevailing NEW style (line or block); `anchor` and
///   `position` as cited; `regenerated` and `final_comment` the
///   normalized text, which must pass `validate_final_comment` for the
///   slot kind (else it is dropped with a `sanitize-dropped-new`
///   warning); decided `NEW` by the engine at confidence 1, or held as
///   `NEEDS_REVIEW` when DIFFICULT;
/// - alignment's `unresolvable-anchor`/`invalid-insertion` warnings and
///   the `sanitize-dropped-new` warnings are collected.
///
/// Records are returned sorted by `(start, id)`. Scanned records stay
/// undecided past rows 1–3 (routing and the judge are later stages);
/// NEW-origin records are decided here.
///
/// # Errors
///
/// - [`Error::Llm`] from any call, including a truncation that can no
///   longer be split (a single id or a chunk that re-cuts into one chunk,
///   or depth [`MAX_SPLIT_DEPTH`]); the file fails with `failureKind:
///   llm`.
/// - [`Error::Invariant`] for inconsistent chunk or inventory data
///   (caller bugs).
/// - Any error of [`chunk_file`], [`build_chunk_texts`] or
///   [`align_remove_mode`].
pub async fn regenerate_file(
    input: FileInput<'_>,
    config: &PlanConfig,
    llm: &dyn LlmClient,
    log: &dyn Log,
) -> Result<RegenOutput, Error> {
    let adapter = adapter_for(input.language);
    let mut warnings = Vec::new();
    // The collision probe comes first, so even a zero-call file records
    // the mode it would have been processed in.
    let effective_mode = match config.mode {
        Mode::Redact if has_placeholder_collision(input.text) => {
            warnings.push(FileWarning {
                kind: FileWarningKind::PlaceholderCollision,
                detail: "the file already contains placeholder-like text (⟦C<digits>⟧); \
                         processed in remove mode instead"
                    .to_owned(),
            });
            Mode::Remove
        }
        mode => mode,
    };
    let mut records = build_draft_records(input.inventory, adapter);
    let targets: BTreeSet<String> = records
        .iter()
        .filter(|record| record.decision.is_none())
        .map(|record| record.id.clone())
        .collect();
    if targets.is_empty() {
        return Ok(RegenOutput {
            records,
            effective_mode,
            warnings,
        });
    }

    let max_chars = usize::try_from(config.max_file_chars).unwrap_or(usize::MAX);
    let (chunks, chunk_warnings) =
        chunk_file(input.text, input.language, input.inventory, max_chars, None)?;
    warnings.extend(chunk_warnings);
    let chunk_texts = build_chunk_texts(
        input.text,
        input.inventory,
        adapter,
        &chunks,
        effective_mode,
        Some(&targets),
    )?;

    let ctx = CallCtx::new(input, adapter, &config.model, effective_mode, llm, log);
    match effective_mode {
        Mode::Redact => {
            let mut regenerations = Vec::new();
            for chunk_text in &chunk_texts {
                let (answered, chunk_warnings) = ctx.redact_chunk(chunk_text).await?;
                regenerations.extend(answered);
                warnings.extend(chunk_warnings);
            }
            join_regenerations(&mut records, regenerations)?;
        }
        Mode::Remove => {
            // The answered (sub-)chunk texts in file order, and each
            // citation with the index of its leaf in that list.
            let mut leaves = Vec::new();
            let mut cited = Vec::new();
            for chunk_text in chunk_texts {
                for (leaf, comments) in ctx.remove_chunk(chunk_text, &targets).await? {
                    let leaf_index = leaves.len();
                    leaves.push(leaf);
                    cited.extend(comments.into_iter().map(|comment| (leaf_index, comment)));
                }
            }
            let alignment = align_remove_mode(
                input.text,
                adapter,
                input.inventory,
                &targets,
                &leaves,
                cited,
            )?;
            warnings.extend(alignment.warnings);
            join_pairs(&mut records, alignment.pairs)?;
            let (new_records, new_warnings) = new_records(input, adapter, alignment.new_candidates);
            records.extend(new_records);
            warnings.extend(new_warnings);
            records.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
        }
    }
    Ok(RegenOutput {
        records,
        effective_mode,
        warnings,
    })
}

/// The per-file context shared by every call #1 request of one file.
struct CallCtx<'a> {
    input: FileInput<'a>,
    adapter: &'static LanguageAdapter,
    model: &'a str,
    mode: Mode,
    llm: &'a dyn LlmClient,
    log: &'a dyn Log,
    /// The inventory's comments by id.
    comments: HashMap<&'a str, &'a InventoryComment>,
}

impl<'a> CallCtx<'a> {
    fn new(
        input: FileInput<'a>,
        adapter: &'static LanguageAdapter,
        model: &'a str,
        mode: Mode,
        llm: &'a dyn LlmClient,
        log: &'a dyn Log,
    ) -> CallCtx<'a> {
        let comments = input
            .inventory
            .comments
            .iter()
            .map(|comment| (comment.id.as_str(), comment))
            .collect();
        CallCtx {
            input,
            adapter,
            model,
            mode,
            llm,
            log,
            comments,
        }
    }

    /// The inventory comment `id`.
    fn comment(&self, id: &str) -> Result<&'a InventoryComment, Error> {
        self.comments.get(id).copied().ok_or_else(|| {
            Error::Invariant(format!(
                "{}: comment id {id} is not in the file's inventory",
                self.input.path
            ))
        })
    }

    /// The raw text of each functional comment of `chunk_text`, in offset
    /// order: the model sees them verbatim and must leave them alone.
    fn functional_texts(&self, chunk_text: &ChunkText) -> Result<Vec<String>, Error> {
        let mut texts = Vec::new();
        for id in &chunk_text.chunk.comment_ids {
            let comment = self.comment(id)?;
            if comment.classification == Classification::Functional {
                texts.push(comment.text.clone());
            }
        }
        Ok(texts)
    }

    /// The routing data of a call #1 request for chunk `chunk_index`: `ids`
    /// are the requested placeholder ids (redact mode; empty in remove
    /// mode), `item_ids` the comments whose originals the request is about.
    fn meta(
        &self,
        chunk_index: usize,
        ids: &[String],
        item_ids: &[String],
    ) -> Result<LlmMeta, Error> {
        let items = item_ids
            .iter()
            .map(|id| {
                let comment = self.comment(id)?;
                Ok(MetaItem {
                    id: comment.id.clone(),
                    original: Some(comment.text.clone()),
                    regenerated: None,
                    anchor: comment.anchor.clone(),
                    position: comment.position,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(LlmMeta {
            call: CallKind::Regenerate,
            mode: self.mode,
            path: self.input.path.to_owned(),
            language: self.input.language,
            chunk_index,
            ids: ids.to_vec(),
            items,
        })
    }

    /// The messages of a call #1 request over `chunk_text`.
    fn messages(
        &self,
        chunk_text: &ChunkText,
        functional_texts: &[String],
        requested_ids: &[String],
    ) -> Vec<Message> {
        build_regeneration_messages(&RegenPromptInput {
            mode: self.mode,
            language: self.input.language,
            path: self.input.path,
            prompt_text: &chunk_text.prompt_text,
            outline: &chunk_text.chunk.outline,
            style_stats: self.input.inventory.style_stats,
            functional_texts,
            declaration_kinds: self.adapter.declaration_kinds,
            requested_ids,
        })
    }

    /// Redact mode: call #1 over one chunk, splitting on truncation.
    ///
    /// The requested ids are the chunk's id-map keys, in offset order. A
    /// chunk without any makes no call. Each truncated request is replaced
    /// by its two halves (first half first), over the same chunk text, one
    /// level deeper; a truncation of a single id or at [`MAX_SPLIT_DEPTH`]
    /// fails the file.
    async fn redact_chunk(
        &self,
        chunk_text: &ChunkText,
    ) -> Result<(Vec<Regeneration>, Vec<FileWarning>), Error> {
        let chunk = &chunk_text.chunk;
        let id_map = chunk_text.id_map.as_ref().ok_or_else(|| {
            Error::Invariant(format!(
                "{}: redact-mode chunk {} has no id map",
                self.input.path, chunk.index
            ))
        })?;
        let requested: Vec<String> = chunk
            .comment_ids
            .iter()
            .filter(|id| id_map.contains_key(id.as_str()))
            .cloned()
            .collect();
        if requested.len() != id_map.len() {
            return Err(Error::Invariant(format!(
                "{}: chunk {} hides placeholder ids outside its own records",
                self.input.path, chunk.index
            )));
        }
        let mut regenerations = Vec::new();
        let mut warnings = Vec::new();
        if requested.is_empty() {
            return Ok((regenerations, warnings));
        }
        let functional_texts = self.functional_texts(chunk_text)?;

        // A stack of pending requests: the last pushed is sent next.
        let mut pending = vec![(requested, 0_u32)];
        while let Some((ids, depth)) = pending.pop() {
            let request = ChatRequest::new(
                self.model,
                self.messages(chunk_text, &functional_texts, &ids),
                self.meta(chunk.index, &ids, &ids)?,
            );
            match complete_typed::<RegenerationResponse>(self.llm, request).await {
                Ok(response) => {
                    let (answered, unknown) =
                        postprocess_regeneration(response, &ids, self.adapter);
                    regenerations.extend(answered);
                    warnings.extend(unknown);
                }
                Err(Error::Llm {
                    reason: LlmErrorReason::Truncated,
                    message,
                }) => {
                    if ids.len() == 1 || depth >= MAX_SPLIT_DEPTH {
                        return Err(Error::Llm {
                            reason: LlmErrorReason::Truncated,
                            message: format!(
                                "{}: chunk {} regeneration of {} id(s) at split depth {depth} \
                                 cannot be split further: {message}",
                                self.input.path,
                                chunk.index,
                                ids.len()
                            ),
                        });
                    }
                    let (first, second) = ids.split_at(ids.len().div_ceil(2));
                    self.log.info(&format!(
                        "{}: chunk {} regeneration truncated; retrying its {} ids as {} + {}",
                        self.input.path,
                        chunk.index,
                        ids.len(),
                        first.len(),
                        second.len()
                    ));
                    pending.push((second.to_vec(), depth + 1));
                    pending.push((first.to_vec(), depth + 1));
                }
                Err(error) => return Err(error),
            }
        }
        Ok((regenerations, warnings))
    }

    /// The ids of the targets `chunk_text` hides (targets that are
    /// editable), in offset order: a remove-mode request's items.
    fn hidden_targets(
        &self,
        chunk_text: &ChunkText,
        targets: &BTreeSet<String>,
    ) -> Result<Vec<String>, Error> {
        let mut hidden = Vec::new();
        for id in &chunk_text.chunk.comment_ids {
            if targets.contains(id) && self.comment(id)?.editable {
                hidden.push(id.clone());
            }
        }
        Ok(hidden)
    }

    /// Remove mode: call #1 over one top-level chunk, re-chunking on
    /// truncation.
    ///
    /// Returns each answered (sub-)chunk text, in file order, with the
    /// comments the model cited in it. A (sub-)chunk hiding no target makes
    /// no call. A truncated request is replaced by the sub-chunks of its
    /// chunk re-cut at half the chunk's size (see [`CallCtx::rechunk`]),
    /// first sub-chunk first, one level deeper; a truncation at
    /// [`MAX_SPLIT_DEPTH`], or of a chunk that re-cuts into a single
    /// chunk, fails the file. Every request carries the top-level chunk's
    /// index in its meta.
    async fn remove_chunk(
        &self,
        chunk_text: ChunkText,
        targets: &BTreeSet<String>,
    ) -> Result<Vec<(ChunkText, Vec<RemoveModeComment>)>, Error> {
        let top_index = chunk_text.chunk.index;
        let mut answered = Vec::new();
        // A stack of pending chunk texts: the last pushed is sent next.
        let mut pending = vec![(chunk_text, 0_u32)];
        while let Some((chunk_text, depth)) = pending.pop() {
            let items = self.hidden_targets(&chunk_text, targets)?;
            if items.is_empty() {
                continue;
            }
            let functional_texts = self.functional_texts(&chunk_text)?;
            let request = ChatRequest::new(
                self.model,
                self.messages(&chunk_text, &functional_texts, &[]),
                self.meta(top_index, &[], &items)?,
            );
            match complete_typed::<RemoveModeRegenerationResponse>(self.llm, request).await {
                Ok(response) => answered.push((chunk_text, response.comments)),
                Err(Error::Llm {
                    reason: LlmErrorReason::Truncated,
                    message,
                }) => {
                    let range = chunk_text.chunk.original_start..chunk_text.chunk.original_end;
                    let parts = if depth < MAX_SPLIT_DEPTH {
                        self.rechunk(&chunk_text, targets)?
                    } else {
                        Vec::new()
                    };
                    if parts.len() < 2 {
                        return Err(Error::Llm {
                            reason: LlmErrorReason::Truncated,
                            message: format!(
                                "{}: chunk {top_index} remove-mode regeneration of bytes \
                                 {range:?} at split depth {depth} cannot be split further: \
                                 {message}",
                                self.input.path
                            ),
                        });
                    }
                    self.log.info(&format!(
                        "{}: chunk {top_index} remove-mode regeneration of bytes {range:?} \
                         truncated; retrying it as {} sub-chunks",
                        self.input.path,
                        parts.len()
                    ));
                    pending.extend(parts.into_iter().rev().map(|part| (part, depth + 1)));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(answered)
    }

    /// Re-cuts `chunk_text`'s range at half its size (in Unicode scalar
    /// values, at least 1), along declaration boundaries per
    /// [`chunk_file`], into remove-mode chunk texts hiding `targets`. The
    /// re-cut's `oversized-chunk` warnings are discarded: they measure the
    /// internal half budget, not `--max-file-chars`.
    fn rechunk(
        &self,
        chunk_text: &ChunkText,
        targets: &BTreeSet<String>,
    ) -> Result<Vec<ChunkText>, Error> {
        let range = chunk_text.chunk.original_start..chunk_text.chunk.original_end;
        let size = self.input.text[range.clone()].chars().count();
        let (chunks, _) = chunk_file(
            self.input.text,
            self.input.language,
            self.input.inventory,
            (size / 2).max(1),
            Some(range),
        )?;
        build_chunk_texts(
            self.input.text,
            self.input.inventory,
            self.adapter,
            &chunks,
            Mode::Remove,
            Some(targets),
        )
    }
}

/// Joins remove-mode pairs (target id, aligned regeneration, its text
/// already normalized) to their records, filling `regenerated` (`None`
/// when normalization left it blank), `difficulty`,
/// `difficulty_rationale` and `missing_context`;
/// `would_you_comment_here` stays `None` in remove mode.
///
/// # Errors
///
/// [`Error::Invariant`] when a pair's id is not an undecided, unanswered
/// record: alignment pairs only targets, one-to-one, so this is a bug.
fn join_pairs(
    records: &mut [DraftRecord],
    pairs: Vec<(String, RemoveModeComment)>,
) -> Result<(), Error> {
    let index: HashMap<String, usize> = records
        .iter()
        .enumerate()
        .map(|(position, record)| (record.id.clone(), position))
        .collect();
    for (id, comment) in pairs {
        let record = index
            .get(&id)
            .map(|&position| &mut records[position])
            .filter(|record| record.decision.is_none() && record.difficulty.is_none())
            .ok_or_else(|| {
                Error::Invariant(format!(
                    "an aligned regeneration for {id} matches no undecided, unanswered record"
                ))
            })?;
        record.regenerated = (!comment.comment.trim().is_empty()).then_some(comment.comment);
        record.difficulty = Some(comment.difficulty);
        record.difficulty_rationale = Some(comment.rationale);
        record.missing_context = comment.missing_context;
    }
    Ok(())
}

/// Joins redact-mode answers to their records by id, filling the model
/// fields.
///
/// # Errors
///
/// [`Error::Invariant`] when an answer's id is not an undecided,
/// not-yet-answered record: requests are built from the undecided records
/// and [`postprocess_regeneration`] drops every unrequested id, so this is
/// a bug.
fn join_regenerations(
    records: &mut [DraftRecord],
    regenerations: Vec<Regeneration>,
) -> Result<(), Error> {
    let index: HashMap<String, usize> = records
        .iter()
        .enumerate()
        .map(|(position, record)| (record.id.clone(), position))
        .collect();
    for regeneration in regenerations {
        let record = index
            .get(&regeneration.id)
            .map(|&position| &mut records[position])
            .filter(|record| record.decision.is_none() && record.difficulty.is_none())
            .ok_or_else(|| {
                Error::Invariant(format!(
                    "regeneration for {} matches no undecided, unanswered record",
                    regeneration.id
                ))
            })?;
        record.regenerated = regeneration.regenerated;
        record.would_you_comment_here = Some(regeneration.would_you_comment_here);
        record.difficulty = Some(regeneration.difficulty);
        record.difficulty_rationale = Some(regeneration.rationale);
        record.missing_context = regeneration.missing_context;
    }
    Ok(())
}

/// Turns the surviving NEW candidates into NEW-origin draft records.
///
/// Candidates are ordered by insertion offset (citation order breaking
/// ties). Each one's text (already normalized by alignment) is validated
/// with [`validate_final_comment`] for the file's prevailing NEW slot kind
/// (`line` or `block`, never `doc`); a failure drops it with a
/// `sanitize-dropped-new` warning. The survivors are numbered `N1`, `N2`,
/// … in that order and decided `NEW` (`decidedBy: engine`, confidence 1),
/// except that a DIFFICULT one is held as `NEEDS_REVIEW`: it is never
/// auto-applied, and stays NEW-origin.
fn new_records(
    input: FileInput<'_>,
    adapter: &LanguageAdapter,
    mut candidates: Vec<NewCandidate>,
) -> (Vec<DraftRecord>, Vec<FileWarning>) {
    let kind = input.inventory.style_stats.prevailing_new();
    let slot_kind = match kind {
        CommentKind::Line => SlotKind::Line,
        CommentKind::Block | CommentKind::Doc => SlotKind::Block,
    };
    candidates.sort_by_key(|candidate| candidate.offset);
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    for candidate in candidates {
        let (line, column) = line_and_column(input.text, candidate.offset);
        let position = Position::from(candidate.position);
        let comment = candidate.comment;
        if let Err(reason) = validate_final_comment(&comment.comment, slot_kind, adapter) {
            warnings.push(FileWarning {
                kind: FileWarningKind::SanitizeDroppedNew,
                detail: format!(
                    "chunk {}: the NEW {} comment at line {line}, column {column} was dropped: \
                     its text {reason}",
                    candidate.chunk,
                    position_name(position)
                ),
            });
            continue;
        }
        let mut record = DraftRecord {
            id: format!("N{}", records.len() + 1),
            kind,
            start: candidate.offset,
            end: candidate.offset,
            line,
            column,
            position,
            classification: Classification::Prose,
            tags: Vec::new(),
            original: None,
            anchor: comment.anchor,
            regenerated: Some(comment.comment.clone()),
            would_you_comment_here: None,
            difficulty: Some(comment.difficulty),
            difficulty_rationale: Some(comment.rationale),
            missing_context: comment.missing_context,
            decision: None,
            final_comment: Some(comment.comment),
            confidence: None,
            decision_rationale: None,
            decided_by: None,
        };
        if comment.difficulty == Difficulty::Difficult {
            record.decide(Decision::NeedsReview, DecidedBy::Engine, 1.0);
            record.decision_rationale = Some(
                "held for review: a DIFFICULT NEW comment is never auto-applied \
                 (proposed decision: NEW)"
                    .to_owned(),
            );
        } else {
            record.decide(Decision::New, DecidedBy::Engine, 1.0);
        }
        records.push(record);
    }
    (records, warnings)
}

/// The 1-based line and 1-based column (in Unicode scalar values; a
/// leading BOM is no column) of `offset` in `text`, as the scanner computes
/// them for comments.
fn line_and_column(text: &str, offset: usize) -> (u32, u32) {
    let line = 1 + text[..offset].matches('\n').count();
    let column = 1 + text[line_start_of(text, offset)..offset].chars().count();
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    )
}

/// The wire name of a NEW record's position, for warnings.
fn position_name(position: Position) -> &'static str {
    match position {
        Position::Leading => "leading",
        Position::Trailing => "trailing",
        Position::Dangling => "dangling",
    }
}
