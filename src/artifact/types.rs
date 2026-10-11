//! The plan artifact schema: serde structs and enums (the single source of
//! truth for the JSON), plus the validation refinements serde cannot
//! express.
//!
//! Every struct is `camelCase` with `deny_unknown_fields`, so a typo in a
//! hand-edited key fails loudly (exit 2). `Option` fields always serialize
//! as `null`. `CommentKind`, `Position`, `Classification` and `Anchor` come
//! from `comments::types`, `Mode` and `PlanConfig` from `config`; their
//! pinned serde names keep the JSON byte-identical.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::comments::types::{Anchor, Classification, CommentKind, Position};
use crate::config::{Mode, PlanConfig};
use crate::errors::Error;
use crate::lang::extensions::language_for;
use crate::lang::ids::LanguageId;
use crate::util::paths::{is_relative_posix, normalize_posix};

/// The only artifact schema version this build reads and writes.
pub const ARTIFACT_VERSION: u32 = 1;

/// How hard a comment's information is to recover from the code alone.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[schemars(description = "How hard the comment was to write from the code alone (see the rubric).")]
pub enum Difficulty {
    /// Fully recoverable from the code.
    #[schemars(description = "Reconstructable from the immediately visible code.")]
    Easy,
    /// Partly recoverable.
    #[schemars(description = "Needed surrounding code or inferring non-obvious intent.")]
    Medium,
    /// Carries information the code does not.
    #[schemars(description = "Needs knowledge absent from the code; guesswork.")]
    Difficult,
}

impl Difficulty {
    /// Every difficulty, in declaration order.
    pub const ALL: [Difficulty; 3] = [Difficulty::Easy, Difficulty::Medium, Difficulty::Difficult];

    /// The wire (serde) name, e.g. `"MEDIUM"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Difficulty::Easy => "EASY",
            Difficulty::Medium => "MEDIUM",
            Difficulty::Difficult => "DIFFICULT",
        }
    }
}

/// The authoritative per-record decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    /// Delete the comment.
    Remove,
    /// Keep the original text.
    KeepOriginal,
    /// Replace with `finalComment` (the regenerated text).
    KeepGenerated,
    /// Replace with `finalComment` (a merge of original and regenerated).
    Hybrid,
    /// Keep the original; flagged for human review.
    NeedsReview,
    /// Keep the original; protected by policy (non-prose).
    KeepProtected,
    /// Insert `finalComment` (NEW-origin records only).
    New,
}

impl Decision {
    /// Every decision, in declaration order.
    pub const ALL: [Decision; 7] = [
        Decision::Remove,
        Decision::KeepOriginal,
        Decision::KeepGenerated,
        Decision::Hybrid,
        Decision::NeedsReview,
        Decision::KeepProtected,
        Decision::New,
    ];

    /// The wire (serde) name, e.g. `"KEEP_ORIGINAL"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Decision::Remove => "REMOVE",
            Decision::KeepOriginal => "KEEP_ORIGINAL",
            Decision::KeepGenerated => "KEEP_GENERATED",
            Decision::Hybrid => "HYBRID",
            Decision::NeedsReview => "NEEDS_REVIEW",
            Decision::KeepProtected => "KEEP_PROTECTED",
            Decision::New => "NEW",
        }
    }

    /// Whether this decision writes `finalComment` into the file (which is
    /// therefore required to be non-null).
    pub const fn writes_final_comment(self) -> bool {
        matches!(
            self,
            Decision::KeepGenerated | Decision::Hybrid | Decision::New
        )
    }

    /// The judge's wire subset, if this decision belongs to it.
    pub const fn as_judge(self) -> Option<JudgeDecision> {
        match self {
            Decision::Remove => Some(JudgeDecision::Remove),
            Decision::KeepOriginal => Some(JudgeDecision::KeepOriginal),
            Decision::KeepGenerated => Some(JudgeDecision::KeepGenerated),
            Decision::Hybrid => Some(JudgeDecision::Hybrid),
            Decision::NeedsReview => Some(JudgeDecision::NeedsReview),
            Decision::KeepProtected | Decision::New => None,
        }
    }
}

/// The subset of [`Decision`] the judge may return on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[schemars(description = "One of the packet's allowed decisions.")]
pub enum JudgeDecision {
    /// See [`Decision::Remove`].
    #[schemars(description = "The comment earns no place; the code says it all.")]
    Remove,
    /// See [`Decision::KeepOriginal`].
    #[schemars(description = "The original is at least as good; leave it untouched.")]
    KeepOriginal,
    /// See [`Decision::KeepGenerated`].
    #[schemars(description = "The regeneration is strictly better and loses nothing.")]
    KeepGenerated,
    /// See [`Decision::Hybrid`].
    #[schemars(description = "Each version carries something the other lacks; merge them.")]
    Hybrid,
    /// See [`Decision::NeedsReview`].
    #[schemars(description = "Cannot decide safely; the original stays.")]
    NeedsReview,
}

impl JudgeDecision {
    /// Every judge decision, in declaration order.
    pub const ALL: [JudgeDecision; 5] = [
        JudgeDecision::Remove,
        JudgeDecision::KeepOriginal,
        JudgeDecision::KeepGenerated,
        JudgeDecision::Hybrid,
        JudgeDecision::NeedsReview,
    ];
}

impl From<JudgeDecision> for Decision {
    fn from(judge: JudgeDecision) -> Self {
        match judge {
            JudgeDecision::Remove => Decision::Remove,
            JudgeDecision::KeepOriginal => Decision::KeepOriginal,
            JudgeDecision::KeepGenerated => Decision::KeepGenerated,
            JudgeDecision::Hybrid => Decision::Hybrid,
            JudgeDecision::NeedsReview => Decision::NeedsReview,
        }
    }
}

/// Who (or what) made a record's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecidedBy {
    /// A pre-LLM or short-circuit heuristic.
    Heuristic,
    /// The LLM judge.
    Judge,
    /// Policy protection (non-prose).
    Protected,
    /// A deterministic post-model override (rails, sanitizer,
    /// alignment-derived NEW).
    Engine,
    /// A human editing the artifact.
    Human,
}

impl DecidedBy {
    /// The wire spelling (`"heuristic"`, `"judge"`, …).
    pub const fn as_str(self) -> &'static str {
        match self {
            DecidedBy::Heuristic => "heuristic",
            DecidedBy::Judge => "judge",
            DecidedBy::Protected => "protected",
            DecidedBy::Engine => "engine",
            DecidedBy::Human => "human",
        }
    }
}

/// A file's processing outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    /// Planned.
    Complete,
    /// Failed; retried by `--resume`.
    Failed,
    /// Deterministically refused; not retried unless the file changes.
    Skipped,
}

/// Why a file failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureKind {
    /// LLM/endpoint failure.
    Llm,
    /// The file could not be read.
    Io,
    /// An internal invariant tripped while planning the file.
    Invariant,
}

/// Why a file was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The bytes are not valid UTF-8.
    NonUtf8,
    /// A Python coding cookie names a non-UTF-8 encoding.
    DeclaredEncoding,
    /// The file does not parse cleanly under its grammar.
    SyntaxErrors,
    /// The syntax tree is too deep to walk safely.
    TooDeep,
}

/// The kind of a non-fatal per-file event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileWarningKind {
    /// Redact placeholders collided with file text (remove-mode fallback).
    PlaceholderCollision,
    /// A cited anchor could not be resolved.
    UnresolvableAnchor,
    /// The model referred to an unknown comment id.
    UnknownId,
    /// A chunk exceeded the size budget.
    OversizedChunk,
    /// A NEW insertion point was invalid.
    InvalidInsertion,
    /// The sanitizer dropped a NEW comment.
    SanitizeDroppedNew,
    /// The verifier demoted a decision.
    VerifyDemoted,
}

/// One non-fatal per-file event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileWarning {
    /// What happened.
    pub kind: FileWarningKind,
    /// Human-readable detail.
    pub detail: String,
}

/// One comment's plan: where it is, what it is, and what to do with it.
///
/// "NEW-origin" is structural: the id starts with `N` and `original` is
/// null, regardless of `decision` (a downgraded NEW is still NEW-origin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommentRecord {
    /// `C<n>` for scanned records, `N<n>` for NEW-origin records; unique
    /// within the file.
    pub id: String,
    /// Syntactic kind; for NEW records the file's prevailing NEW style
    /// (`line` or `block`, never `doc`).
    pub kind: CommentKind,
    /// UTF-8 byte offset into the original text.
    pub start: usize,
    /// Exclusive end offset; `start == end` (the insertion point) for NEW
    /// records.
    pub end: usize,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in Unicode scalar values.
    pub column: u32,
    /// Leading / trailing / dangling.
    pub position: Position,
    /// The classification at plan time (apply re-derives its own).
    pub classification: Classification,
    /// Sorted, deduplicated tags.
    pub tags: Vec<String>,
    /// Raw comment text including delimiters; null for NEW records.
    pub original: Option<String>,
    /// The code this comment annotates.
    pub anchor: Anchor,
    /// The regenerated comment, if any.
    pub regenerated: Option<String>,
    /// Redact mode only; null in remove mode.
    pub would_you_comment_here: Option<bool>,
    /// Difficulty rating, if rated.
    pub difficulty: Option<Difficulty>,
    /// Why that difficulty.
    pub difficulty_rationale: Option<String>,
    /// What the code alone does not convey, if anything.
    pub missing_context: Option<String>,
    /// The authoritative decision.
    pub decision: Decision,
    /// Exact text to write; required for KEEP_GENERATED / HYBRID / NEW,
    /// allowed and ignored otherwise.
    pub final_comment: Option<String>,
    /// In `[0, 1]`; 1 for deterministic decisions.
    pub confidence: f64,
    /// Why this decision.
    pub decision_rationale: Option<String>,
    /// Who decided.
    pub decided_by: DecidedBy,
}

impl CommentRecord {
    /// Whether this is a NEW-origin record (no original text).
    pub fn is_new_origin(&self) -> bool {
        self.original.is_none()
    }
}

/// One file's plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilePlan {
    /// POSIX path relative to the artifact's `root`.
    pub path: String,
    /// `"sha256:<lowercase hex>"` of the raw bytes at plan time; null iff
    /// the file failed with an I/O error (never read).
    pub content_hash: Option<String>,
    /// Raw byte length at plan time (0 when never read).
    pub byte_length: u64,
    /// The language the file was planned under; must equal
    /// `language_for(path)`.
    pub language: LanguageId,
    /// The mode actually used (differs from `config.mode` only on the
    /// placeholder-collision fallback).
    pub effective_mode: Mode,
    /// Processing outcome.
    pub status: FileStatus,
    /// Set iff `status == failed`.
    pub failure_kind: Option<FailureKind>,
    /// Set iff `status == skipped`.
    pub skip_reason: Option<SkipReason>,
    /// Human-readable detail for failed/skipped files.
    pub error: Option<String>,
    /// Non-fatal events.
    pub warnings: Vec<FileWarning>,
    /// Records sorted by `(start, id)`; empty for failed/skipped files.
    pub comments: Vec<CommentRecord>,
}

/// Difficulty counts over scanned prose records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DifficultyHistogram {
    /// `EASY` records.
    #[serde(rename = "EASY")]
    pub easy: u64,
    /// `MEDIUM` records.
    #[serde(rename = "MEDIUM")]
    pub medium: u64,
    /// `DIFFICULT` records.
    #[serde(rename = "DIFFICULT")]
    pub difficult: u64,
    /// Records without a difficulty.
    pub unrated: u64,
}

/// Informational run totals; report and apply always recompute them from
/// the records.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Summary {
    /// Discovered files (complete + failed + skipped).
    pub files_considered: u64,
    /// Files with status failed.
    pub files_failed: u64,
    /// Files with status skipped.
    pub files_skipped: u64,
    /// Discovered files per language.
    pub languages: BTreeMap<LanguageId, u64>,
    /// Scanned records, all classifications.
    pub comments_total: u64,
    /// Scanned prose records.
    pub prose_total: u64,
    /// Scanned records decided REMOVE.
    pub removed: u64,
    /// Scanned records decided KEEP_GENERATED or HYBRID.
    pub rewritten: u64,
    /// Scanned records decided KEEP_ORIGINAL.
    pub kept: u64,
    /// Scanned records decided NEEDS_REVIEW.
    pub needs_review: u64,
    /// Scanned records decided KEEP_PROTECTED.
    pub protected: u64,
    /// Non-prose scanned records not decided KEEP_PROTECTED.
    pub overrides: u64,
    /// NEW-origin records decided NEW.
    pub new: u64,
    /// NEW-origin records decided NEEDS_REVIEW.
    pub new_held: u64,
    /// Total per-file warnings.
    pub warnings: u64,
    /// Fraction of prose records removed, in `[0, 1]`.
    pub reduction_pct: f64,
    /// Fraction of prose comment characters removed, in `[0, 1]`.
    pub char_reduction_pct: f64,
    /// Difficulty counts over scanned prose records.
    pub difficulty_histogram: DifficultyHistogram,
}

/// The whole plan artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanArtifact {
    /// Schema version; must be [`ARTIFACT_VERSION`].
    pub version: u32,
    /// RFC 3339 UTC, seconds precision.
    pub created_at: String,
    /// POSIX path of the source root, relative to the artifact file's
    /// directory (`"."` when co-located; may contain `..`).
    pub root: String,
    /// POSIX path, relative to `root`, of the directory globs are matched
    /// against (the canonical planning cwd).
    pub glob_base: String,
    /// The config snapshot.
    pub config: PlanConfig,
    /// Per-file plans, sorted by path.
    pub files: Vec<FilePlan>,
    /// Informational totals.
    pub summary: Summary,
}

/// A [`PlanArtifact`] that has passed every refinement of
/// [`PlanArtifact::validate`]. The engine and apply consume this type, so
/// the invariants are checked once, at the boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedArtifact(PlanArtifact);

impl ValidatedArtifact {
    /// Unwraps the validated artifact.
    pub fn into_inner(self) -> PlanArtifact {
        self.0
    }
}

impl Deref for ValidatedArtifact {
    type Target = PlanArtifact;

    fn deref(&self) -> &PlanArtifact {
        &self.0
    }
}

impl PlanArtifact {
    /// Checks every structural refinement serde cannot express; any
    /// violation is `Error::Artifact` (exit 2) naming the offending file
    /// path and record id.
    ///
    /// - `version == 1`; `globBase` is a relative POSIX path;
    /// - `FilePlan.path` values are unique after POSIX normalization, and
    ///   each file's `language == language_for(path)`;
    /// - `failureKind` set iff failed, `skipReason` set iff skipped;
    ///   `contentHash` null iff failed with `failureKind: io`, and
    ///   otherwise `sha256:` + 64 lowercase hex digits;
    /// - ids match `^[CN][1-9][0-9]*$`, are unique within a file, and start
    ///   with `N` iff `original` is null;
    /// - scanned records are never `NEW`; NEW-origin records are only `NEW`
    ///   or `NEEDS_REVIEW`, only in `effectiveMode: remove` files, have
    ///   `start == end`, and `start <= byteLength`;
    /// - `start <= end`; `confidence` in `[0, 1]`; `finalComment` non-null
    ///   for KEEP_GENERATED / HYBRID / NEW;
    /// - scanned records' `[start, end)` ranges are pairwise
    ///   non-overlapping within a file.
    pub fn validate(self) -> Result<ValidatedArtifact, Error> {
        if self.version != ARTIFACT_VERSION {
            return Err(Error::Artifact(format!(
                "unsupported artifact version {} (this build reads version {ARTIFACT_VERSION})",
                self.version
            )));
        }
        if !is_relative_posix(&self.glob_base) {
            return Err(Error::Artifact(format!(
                "globBase {:?} must be a relative POSIX path",
                self.glob_base
            )));
        }
        let mut seen: HashMap<String, &str> = HashMap::new();
        for file in &self.files {
            let normalized = normalize_posix(&file.path);
            if let Some(previous) = seen.insert(normalized, &file.path) {
                return Err(Error::Artifact(format!(
                    "{}: duplicate file entry (same file as {previous:?} after path normalization)",
                    file.path
                )));
            }
            validate_file(file)?;
        }
        Ok(ValidatedArtifact(self))
    }
}

/// The per-file refinements.
fn validate_file(file: &FilePlan) -> Result<(), Error> {
    let fail = |detail: String| Err(Error::Artifact(format!("{}: {detail}", file.path)));
    match language_for(&file.path) {
        Some(expected) if expected == file.language => {}
        Some(expected) => {
            return fail(format!(
                "language {:?} disagrees with the file extension, which maps to {:?}",
                file.language.as_str(),
                expected.as_str()
            ));
        }
        None => return fail("the file extension maps to no supported language".to_owned()),
    }
    let failed = file.status == FileStatus::Failed;
    if failed != file.failure_kind.is_some() {
        return fail("failureKind must be set iff status is \"failed\"".to_owned());
    }
    if (file.status == FileStatus::Skipped) != file.skip_reason.is_some() {
        return fail("skipReason must be set iff status is \"skipped\"".to_owned());
    }
    let never_read = failed && file.failure_kind == Some(FailureKind::Io);
    match (&file.content_hash, never_read) {
        (None, true) => {}
        (None, false) => {
            return fail(
                "contentHash may be null only for a failed file with failureKind \"io\"".to_owned(),
            );
        }
        (Some(_), true) => {
            return fail(
                "contentHash must be null for a failed file with failureKind \"io\"".to_owned(),
            );
        }
        (Some(hash), false) if !is_content_hash(hash) => {
            return fail(format!(
                "contentHash {hash:?} must be \"sha256:\" followed by 64 lowercase hex digits"
            ));
        }
        (Some(_), false) => {}
    }
    let mut ids: HashSet<&str> = HashSet::new();
    for record in &file.comments {
        if !ids.insert(&record.id) {
            return fail(format!("duplicate comment id {}", record.id));
        }
        validate_record(file, record).or_else(|detail| fail(format!("{}: {detail}", record.id)))?;
    }
    validate_no_overlap(file)
}

/// The per-record refinements; `Err` carries the detail only.
fn validate_record(file: &FilePlan, record: &CommentRecord) -> Result<(), String> {
    let id = record.id.as_bytes();
    let well_formed = id.len() >= 2
        && matches!(id[0], b'C' | b'N')
        && id[1] != b'0'
        && id[1..].iter().all(u8::is_ascii_digit);
    if !well_formed {
        return Err("ids must match ^[CN][1-9][0-9]*$".to_owned());
    }
    let new_origin = id[0] == b'N';
    if new_origin != record.original.is_none() {
        return Err("ids start with N exactly when original is null".to_owned());
    }
    if record.start > record.end {
        return Err(format!("start {} exceeds end {}", record.start, record.end));
    }
    if !(0.0..=1.0).contains(&record.confidence) {
        return Err(format!(
            "confidence {} is outside [0, 1]",
            record.confidence
        ));
    }
    if record.decision.writes_final_comment() && record.final_comment.is_none() {
        return Err(format!(
            "decision {} requires a non-null finalComment",
            record.decision.as_str()
        ));
    }
    if new_origin {
        if !matches!(record.decision, Decision::New | Decision::NeedsReview) {
            return Err(format!(
                "a NEW-origin record's decision must be NEW or NEEDS_REVIEW, not {}",
                record.decision.as_str()
            ));
        }
        if file.effective_mode != Mode::Remove {
            return Err(
                "NEW-origin records may occur only in effectiveMode \"remove\" files".to_owned(),
            );
        }
        if record.start != record.end {
            return Err("a NEW-origin record must have start == end".to_owned());
        }
        let in_bounds = u64::try_from(record.start).is_ok_and(|start| start <= file.byte_length);
        if !in_bounds {
            return Err(format!(
                "insertion offset {} lies beyond the file length {}",
                record.start, file.byte_length
            ));
        }
    } else if record.decision == Decision::New {
        return Err("a scanned record's decision may not be NEW".to_owned());
    }
    Ok(())
}

/// Scanned records' `[start, end)` ranges must be pairwise disjoint.
fn validate_no_overlap(file: &FilePlan) -> Result<(), Error> {
    let mut scanned: Vec<&CommentRecord> = file
        .comments
        .iter()
        .filter(|r| !r.is_new_origin())
        .collect();
    scanned.sort_by_key(|r| (r.start, r.end));
    let mut widest: Option<&CommentRecord> = None;
    for record in scanned {
        if let Some(prior) = widest
            && record.start < prior.end
        {
            return Err(Error::Artifact(format!(
                "{}: comment ranges overlap: {} [{}, {}) and {} [{}, {})",
                file.path, prior.id, prior.start, prior.end, record.id, record.start, record.end
            )));
        }
        if widest.is_none_or(|prior| record.end > prior.end) {
            widest = Some(record);
        }
    }
    Ok(())
}

/// Whether `hash` is `"sha256:"` + 64 lowercase hex digits.
fn is_content_hash(hash: &str) -> bool {
    hash.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Serialize>(value: T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn judge_decisions_map_to_decisions_of_the_same_name() {
        for judge in JudgeDecision::ALL {
            let decision = Decision::from(judge);
            assert_eq!(wire(judge), wire(decision));
            assert_eq!(decision.as_judge(), Some(judge));
        }
        let outside: Vec<Decision> = Decision::ALL
            .into_iter()
            .filter(|d| d.as_judge().is_none())
            .collect();
        assert_eq!(outside, [Decision::KeepProtected, Decision::New]);
    }

    #[test]
    fn decision_as_str_is_the_wire_name() {
        for decision in Decision::ALL {
            assert_eq!(decision.as_str(), wire(decision));
        }
        assert_eq!(wire(Decision::KeepGenerated), "KEEP_GENERATED");
    }

    #[test]
    fn difficulty_as_str_is_the_wire_name() {
        for difficulty in Difficulty::ALL {
            assert_eq!(difficulty.as_str(), wire(difficulty));
        }
    }

    #[test]
    fn decided_by_as_str_is_the_wire_name() {
        for decided_by in [
            DecidedBy::Heuristic,
            DecidedBy::Judge,
            DecidedBy::Protected,
            DecidedBy::Engine,
            DecidedBy::Human,
        ] {
            assert_eq!(decided_by.as_str(), wire(decided_by));
        }
    }

    #[test]
    fn enum_wire_names_are_pinned() {
        assert_eq!(wire(Difficulty::Difficult), "DIFFICULT");
        assert_eq!(wire(DecidedBy::Human), "human");
        assert_eq!(wire(FileStatus::Skipped), "skipped");
        assert_eq!(wire(FailureKind::Io), "io");
        assert_eq!(wire(SkipReason::NonUtf8), "non-utf8");
        assert_eq!(wire(SkipReason::DeclaredEncoding), "declared-encoding");
        assert_eq!(wire(SkipReason::SyntaxErrors), "syntax-errors");
        assert_eq!(wire(SkipReason::TooDeep), "too-deep");
        assert_eq!(
            wire(FileWarningKind::SanitizeDroppedNew),
            "sanitize-dropped-new"
        );
        assert_eq!(wire(FileWarningKind::VerifyDemoted), "verify-demoted");
    }

    #[test]
    fn summary_keys_are_camel_case_with_a_pinned_histogram() {
        let mut summary = Summary::default();
        summary.languages.insert(LanguageId::Rust, 2);
        let json = serde_json::to_value(&summary).unwrap();
        let object = json.as_object().unwrap();
        assert!(object.contains_key("filesConsidered"));
        assert!(object.contains_key("charReductionPct"));
        assert_eq!(json["languages"]["rust"], 2);
        let histogram = json["difficultyHistogram"].as_object().unwrap();
        let keys: Vec<&str> = histogram.keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 4);
        for key in ["EASY", "MEDIUM", "DIFFICULT", "unrated"] {
            assert!(keys.contains(&key), "{key}");
        }
        let back: Summary = serde_json::from_value(json).unwrap();
        assert_eq!(back, summary);
    }

    #[test]
    fn content_hash_shape_is_checked_exactly() {
        let hex = "0123456789abcdef".repeat(4);
        assert!(is_content_hash(&format!("sha256:{hex}")));
        assert!(!is_content_hash(&format!("sha256:{}", hex.to_uppercase())));
        assert!(!is_content_hash(&format!("sha256:{}", &hex[1..])));
        assert!(!is_content_hash(&format!("sha1:{hex}")));
    }

    #[test]
    fn floats_round_trip_exactly() {
        for value in [
            0.1_f64 + 0.2,
            1.0 / 3.0,
            0.82,
            5e-324,
            0.999_999_999_999_999_9,
        ] {
            let json = serde_json::to_string(&value).unwrap();
            let back: f64 = serde_json::from_str(&json).unwrap();
            assert_eq!(back.to_bits(), value.to_bits(), "{json}");
        }
    }
}
