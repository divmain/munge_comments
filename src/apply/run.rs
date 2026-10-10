//! The apply driver: [`run_apply`] turns a validated plan artifact into
//! written files, in two phases.
//!
//! **Phase A (no writes).** Every artifact path is validated (exit 2 on
//! any violation, selected or not). Each selected file is triaged from the
//! artifact alone — `failed`/`skipped` → [`ApplyStatus::SkippedPlanStatus`],
//! no actionable edit → [`ApplyStatus::Unchanged`] — and every remaining
//! file is staged:
//!
//! 1. link checks, read, and the disk hash against `contentHash`;
//! 2. [`inventory_file`] + firewall check (a) (every scanned record's range
//!    and text match a current inventory record), plus the NEW-offset
//!    checks;
//! 3. [`compute_edits`];
//! 4. firewall check (b) (no edit intersects a currently-functional
//!    comment);
//! 5. [`apply_edits`] in memory.
//!
//! Steps 3–5 are replaceable through [`ApplyHooks::apply_file`]. Drift
//! (exit 3) and artifact/codebase disagreement (exit 2) in any selected
//! file abort the run before a single byte is written, listing every
//! offending file. Only then is each staged file verified
//! ([`verify_semantic_equality`]), so a run that is going to abort never
//! pays for verification.
//!
//! **Phase B** writes every verified file atomically, re-hashing the target
//! immediately before its rename; a file that changed since Phase A is
//! `failed-io` ("changed during apply"), as is any write failure. The rest
//! proceed. When some files were written and some are `failed-io`, the
//! result carries a ready-to-run recovery command re-selecting the failed
//! ones ([`ApplyResult::partial_apply_hint`]).
//!
//! **Dry runs** execute all of Phase A, verification included, then mark
//! every file that would be written [`ApplyStatus::DryRun`] and print its
//! unified diff to [`ApplyDeps::stdout`] instead of entering Phase B. They
//! write nothing — no source file, no report — and return the exit code a
//! real run would.
//!
//! Run-level aborts are returned as `Err`; per-file outcomes live in the
//! [`ApplyResult`] and are folded into the exit code by [`exit_code_for`].

use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use super::diff::render_unified_diff;
use super::edits::{EditOpts, apply_edits, compute_edits};
use super::types::{ApplyFileResult, ApplyResult, ApplyStatus, SkippedEdit};
use super::verify::{FunctionalComment, functional_sequence, verify_semantic_equality};
use crate::artifact::read::read_artifact;
use crate::artifact::types::{Decision, FilePlan, FileStatus, PlanArtifact};
use crate::artifact::validate::{check_file_path, content_hash, decode_utf8, resolve_root};
use crate::comments::inventory::{Inventory, inventory_file};
use crate::comments::types::{Classification, Edit, FileRef};
use crate::errors::{Error, pick_exit_code};
use crate::lang::adapters::adapter_for;
use crate::log::Log;
use crate::report::markdown::render_markdown;
use crate::util::fs::{atomic_write, ensure_parent_dir};
use crate::util::paths::{lexical_normalize, relative_posix, shell_quote, to_posix};

/// What to apply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyRequest {
    /// The plan artifact, relative to [`ApplyDeps::cwd`] (or absolute).
    /// The recovery hint repeats it exactly as given.
    pub artifact: PathBuf,
    /// `--only` file paths, relative to the cwd; empty selects every file.
    pub only: Vec<String>,
    /// Print diffs instead of writing.
    pub dry_run: bool,
    /// Where to write the post-apply markdown report, relative to the cwd;
    /// `None` writes no report, and neither does a dry run.
    pub report_path: Option<PathBuf>,
}

/// The per-file edit computation [`ApplyHooks::apply_file`] substitutes:
/// `(original text, file plan, current inventory) → edited text`.
pub type ApplyFileHook = dyn Fn(&str, &FilePlan, &Inventory) -> Result<String, Error>;

/// Test seams for [`run_apply`]; the default (all `None`) is production
/// behavior.
#[derive(Default)]
pub struct ApplyHooks {
    /// Replaces the per-file edit computation (`compute_edits` → firewall
    /// check (b) → `apply_edits`). The drift checks, firewall check (a),
    /// the NEW-offset checks, the verifier and the writer stay outside the
    /// seam. Its errors are classified like the real computation's.
    pub apply_file: Option<Box<ApplyFileHook>>,
}

/// The ambient inputs of [`run_apply`].
pub struct ApplyDeps<'a> {
    /// Progress, warnings and the summary.
    pub log: &'a dyn Log,
    /// Dry-run diffs.
    pub stdout: &'a mut dyn Write,
    /// The directory every request path is resolved against.
    pub cwd: &'a Path,
    /// Test seams.
    pub hooks: ApplyHooks,
}

/// The outcome of a run that got as far as producing per-file results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Every artifact file's outcome.
    pub result: ApplyResult,
    /// The process exit code the run maps to.
    pub exit_code: u8,
}

/// The exit code of an apply run's per-file outcomes, by the precedence
/// table of [`pick_exit_code`]: `failed-invariant` → 7, `skipped-firewall`
/// and `skipped-verify` → 4, `failed-io` → 6; every other status is
/// success.
pub fn exit_code_for(result: &ApplyResult) -> u8 {
    let codes: Vec<u8> = result
        .files
        .iter()
        .filter_map(|file| match file.status {
            ApplyStatus::FailedInvariant => Some(7),
            ApplyStatus::SkippedFirewall | ApplyStatus::SkippedVerify => Some(4),
            ApplyStatus::FailedIo => Some(6),
            ApplyStatus::Applied
            | ApplyStatus::Unchanged
            | ApplyStatus::DryRun
            | ApplyStatus::NotSelected
            | ApplyStatus::SkippedPlanStatus => None,
        })
        .collect();
    pick_exit_code(&codes)
}

/// Applies the artifact at `req.artifact` (see the module docs).
///
/// `deps.cwd` is canonicalized once; the artifact, the report path and
/// every `--only` path are joined onto it.
///
/// # Errors
///
/// Run-level aborts, before any source file is written:
/// - [`Error::Artifact`] (exit 2): a missing or invalid artifact, an
///   invalid artifact file path (any file, selected or not), and, in any
///   selected file, a scanned record whose range or text does not match
///   the current file, a bad NEW offset, or any other plan that does not
///   fit the text;
/// - [`Error::Drift`] (exit 3): a selected file deleted, replaced by a
///   symlink or non-file, or whose bytes no longer hash to `contentHash`;
/// - [`Error::Usage`] (exit 1): an `--only` path naming no artifact file,
///   or a report path naming no file;
/// - [`Error::Fs`] (exit 6): the cwd, the artifact or the report's parent
///   directory cannot be resolved or created.
pub fn run_apply(req: ApplyRequest, deps: ApplyDeps<'_>) -> Result<ApplyOutcome, Error> {
    let cwd = fs::canonicalize(deps.cwd).map_err(|source| Error::Fs {
        path: deps.cwd.to_path_buf(),
        source,
    })?;
    if let Some(report) = &req.report_path
        && report.file_name().is_none()
    {
        return Err(Error::Usage(format!(
            "the report path must name a file, got {}",
            report.display()
        )));
    }

    // 1. Load + validate the artifact.
    let artifact_path = canonical_artifact_path(&cwd, &req.artifact)?;
    let artifact = read_artifact(&artifact_path)?;
    // 2. Resolve the root and validate every artifact path.
    let root = resolve_root(&artifact_path, &artifact.root)?;
    let targets = artifact
        .files
        .iter()
        .map(|file| check_file_path(&root, &file.path))
        .collect::<Result<Vec<PathBuf>, Error>>()?;
    // 3. Resolve `--only`.
    let selected = resolve_selection(&req.only, &cwd, &root, &targets)?;

    // 4. Phase A.
    let ctx = FileCtx {
        allow_add: artifact.config.allow_add,
        log: deps.log,
        hooks: &deps.hooks,
    };
    // `None` while a file is staged (or aborts the run).
    let mut results: Vec<Option<ApplyFileResult>> = Vec::with_capacity(artifact.files.len());
    let mut staged: Vec<Staged> = Vec::new();
    let mut aborts = Aborts::default();
    for (index, (file, target)) in artifact.files.iter().zip(&targets).enumerate() {
        let settled = |status, reason: String| ApplyFileResult {
            path: file.path.clone(),
            status,
            reason: Some(reason),
            skipped_edits: Vec::new(),
        };
        let result = if !selected[index] {
            settled(
                ApplyStatus::NotSelected,
                "not selected by --only".to_owned(),
            )
        } else if file.status != FileStatus::Complete {
            settled(ApplyStatus::SkippedPlanStatus, plan_status_reason(file))
        } else if !has_actionable_edit(file, ctx.allow_add) {
            settled(ApplyStatus::Unchanged, "no actionable edit".to_owned())
        } else {
            match stage_file(file, target, &ctx) {
                Ok(stage) if stage.edited == stage.original => ApplyFileResult {
                    path: file.path.clone(),
                    status: ApplyStatus::Unchanged,
                    reason: Some("the edits leave the file unchanged".to_owned()),
                    skipped_edits: stage.skipped,
                },
                Ok(stage) => {
                    staged.push(Staged {
                        index,
                        target: target.clone(),
                        stage,
                    });
                    results.push(None);
                    continue;
                }
                Err(err) => match per_file_status(&err) {
                    Some(status) => settled(status, err.to_string()),
                    None => {
                        aborts.push(&file.path, err);
                        results.push(None);
                        continue;
                    }
                },
            }
        };
        results.push(Some(result));
    }
    aborts.into_result()?;
    // Verification, only once the run is known not to abort.
    let mut ready: Vec<Staged> = Vec::with_capacity(staged.len());
    for item in staged {
        let file = &artifact.files[item.index];
        match verify_staged(file, &item.stage) {
            Ok(()) => ready.push(item),
            Err(err) => {
                let status = per_file_status(&err).unwrap_or(ApplyStatus::FailedInvariant);
                results[item.index] = Some(ApplyFileResult {
                    path: file.path.clone(),
                    status,
                    reason: Some(err.to_string()),
                    skipped_edits: item.stage.skipped,
                });
            }
        }
    }

    // The report's directory is created before the first write, so an
    // unusable report path fails the run while the sources are untouched.
    // A dry run writes no report.
    let report_target = match &req.report_path {
        Some(report) if !req.dry_run => Some(report_target(&cwd, report)?),
        _ => None,
    };

    // 5. Phase B (a dry run renders the diffs instead).
    let mut diffs = String::new();
    for item in ready {
        let file = &artifact.files[item.index];
        results[item.index] = Some(if req.dry_run {
            let Stage {
                original,
                edited,
                edits,
                skipped,
                ..
            } = item.stage;
            diffs.push_str(&render_unified_diff(&file.path, &original, &edited, &edits));
            ApplyFileResult {
                path: file.path.clone(),
                status: ApplyStatus::DryRun,
                reason: Some("dry run: not written".to_owned()),
                skipped_edits: skipped,
            }
        } else {
            write_staged(file, &item.target, item.stage)
        });
    }
    let files = results
        .into_iter()
        .collect::<Option<Vec<ApplyFileResult>>>()
        .ok_or_else(|| {
            Error::Invariant("an artifact file was left without an apply outcome".to_owned())
        })?;
    let mut result = ApplyResult {
        files,
        partial_apply_hint: None,
    };
    result.partial_apply_hint =
        partial_apply_hint(&req.artifact, &cwd, &targets, &result, deps.log);
    let mut exit_code = exit_code_for(&result);
    if let Some(hint) = &result.partial_apply_hint {
        deps.log.warn(&format!(
            "apply: some files were written and some failed; to retry the failed ones run: {hint}"
        ));
    }
    if req.dry_run
        && let Err(err) = deps
            .stdout
            .write_all(diffs.as_bytes())
            .and_then(|()| deps.stdout.flush())
    {
        // An output failure is filesystem I/O: exit 6.
        deps.log
            .warn(&format!("apply: could not write the dry-run diffs: {err}"));
        exit_code = pick_exit_code(&[exit_code, 6]);
    }

    // 6. Post-apply report.
    if let Some(target) = report_target {
        match write_report(&artifact, &result, &target) {
            Ok(()) => deps
                .log
                .info(&format!("apply: wrote report {}", target.display())),
            Err(err) => {
                deps.log.warn(&format!("apply: {err}"));
                exit_code = pick_exit_code(&[exit_code, err.exit_code()]);
            }
        }
    }
    // 7. Summary.
    log_summary(deps.log, &result);
    Ok(ApplyOutcome { result, exit_code })
}

/// The partial-apply recovery command: when at least one file was written
/// and at least one is `failed-io`, `munge-comments apply <artifact>
/// --only <p> …` with one `--only` per `failed-io` file (in artifact
/// order). `<artifact>` is the path exactly as the caller passed it; each
/// `<p>` is the file's target re-expressed relative to the canonical
/// `cwd` (absolute when it has no relative spelling), so the command
/// works from the invocation directory whatever the artifact's `root`.
/// Every argument goes through [`shell_quote`].
///
/// `None` when the condition does not hold, or (with a warning) when a
/// path has no UTF-8 spelling to print.
fn partial_apply_hint(
    artifact: &Path,
    cwd: &Path,
    targets: &[PathBuf],
    result: &ApplyResult,
    log: &dyn Log,
) -> Option<String> {
    let any_applied = result
        .files
        .iter()
        .any(|file| file.status == ApplyStatus::Applied);
    if !any_applied {
        return None;
    }
    let failed: Vec<&PathBuf> = result
        .files
        .iter()
        .zip(targets)
        .filter(|(file, _)| file.status == ApplyStatus::FailedIo)
        .map(|(_, target)| target)
        .collect();
    if failed.is_empty() {
        return None;
    }
    let unprintable = |path: &Path| {
        log.warn(&format!(
            "apply: no recovery command can be printed: {} has no UTF-8 spelling",
            path.display()
        ));
    };
    let Some(artifact_arg) = artifact.to_str() else {
        unprintable(artifact);
        return None;
    };
    let mut hint = format!("munge-comments apply {}", shell_quote(artifact_arg));
    for target in failed {
        let Some(path) = relative_posix(cwd, target).or_else(|| to_posix(target)) else {
            unprintable(target);
            return None;
        };
        let _ = write!(hint, " --only {}", shell_quote(&path));
    }
    Some(hint)
}

/// `cwd.join(artifact)`, canonicalized; a missing artifact is
/// [`Error::Artifact`].
fn canonical_artifact_path(cwd: &Path, artifact: &Path) -> Result<PathBuf, Error> {
    let joined = cwd.join(artifact);
    fs::canonicalize(&joined).map_err(|source| match source.kind() {
        ErrorKind::NotFound => {
            Error::Artifact(format!("{}: no such artifact file", joined.display()))
        }
        _ => Error::Fs {
            path: joined,
            source,
        },
    })
}

/// The absolute path a cwd-relative `--only` argument names, spelled the
/// way [`check_file_path`] spells artifact targets: the final component is
/// not resolved (a symlink there is drift, not a different file) and the
/// parent directory is canonicalized when it exists, so a deleted file can
/// still be selected (and then reported as drift).
fn only_target(cwd: &Path, arg: &str) -> Result<PathBuf, Error> {
    let joined = lexical_normalize(&cwd.join(arg));
    let (Some(parent), Some(name)) = (joined.parent(), joined.file_name()) else {
        return Err(Error::Usage(format!("--only {arg:?} does not name a file")));
    };
    match fs::canonicalize(parent) {
        Ok(canonical) => Ok(canonical.join(name)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(joined.clone()),
        Err(source) => Err(Error::Fs {
            path: parent.to_path_buf(),
            source,
        }),
    }
}

/// Which artifact files are selected: all of them without `--only`;
/// otherwise exactly those named. An `--only` path naming no artifact file
/// is [`Error::Usage`].
fn resolve_selection(
    only: &[String],
    cwd: &Path,
    root: &Path,
    targets: &[PathBuf],
) -> Result<Vec<bool>, Error> {
    if only.is_empty() {
        return Ok(vec![true; targets.len()]);
    }
    let mut selected = vec![false; targets.len()];
    for arg in only {
        let target = only_target(cwd, arg)?;
        let index = targets
            .iter()
            .position(|candidate| *candidate == target)
            .ok_or_else(|| {
                let spelled = relative_posix(root, &target)
                    .map_or_else(|| target.display().to_string(), |rel| format!("{rel:?}"));
                Error::Usage(format!(
                    "--only {arg:?} (root-relative {spelled}) names no file in the artifact"
                ))
            })?;
        selected[index] = true;
    }
    Ok(selected)
}

/// The canonical report path: the report's parent directory is created
/// and canonicalized, and its file name joined back on.
fn report_target(cwd: &Path, report: &Path) -> Result<PathBuf, Error> {
    let joined = cwd.join(report);
    let name = joined.file_name().ok_or_else(|| {
        Error::Usage(format!(
            "the report path must name a file, got {}",
            report.display()
        ))
    })?;
    Ok(ensure_parent_dir(&joined)?.join(name))
}

/// Renders and atomically writes the post-apply report.
fn write_report(artifact: &PlanArtifact, result: &ApplyResult, target: &Path) -> Result<(), Error> {
    let markdown = render_markdown(artifact, Some(result))?;
    atomic_write(target, markdown.as_bytes())
}

/// The reason recorded for a `failed`/`skipped` plan entry.
fn plan_status_reason(file: &FilePlan) -> String {
    let status = wire_name(&file.status);
    let detail = match file.status {
        FileStatus::Failed => file.failure_kind.as_ref().map(wire_name),
        FileStatus::Skipped => file.skip_reason.as_ref().map(wire_name),
        FileStatus::Complete => None,
    };
    let mut reason = format!("the plan recorded the file as {status}");
    if let Some(detail) = detail {
        let _ = write!(reason, " ({detail})");
    }
    if let Some(error) = &file.error {
        let _ = write!(reason, ": {error}");
    }
    reason
}

/// A unit enum's serde name (`"failed"`, `"non-utf8"`, …).
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::from("?"),
    }
}

/// Whether `file` has any edit to make: a scanned record decided
/// `REMOVE`/`KEEP_GENERATED`/`HYBRID`, or (with `allowAdd`) a record
/// decided `NEW`.
fn has_actionable_edit(file: &FilePlan, allow_add: bool) -> bool {
    file.comments.iter().any(|record| match record.decision {
        Decision::Remove | Decision::KeepGenerated | Decision::Hybrid => true,
        Decision::New => allow_add,
        Decision::KeepOriginal | Decision::NeedsReview | Decision::KeepProtected => false,
    })
}

/// What every per-file step shares.
struct FileCtx<'a> {
    /// The artifact's `config.allowAdd`.
    allow_add: bool,
    log: &'a dyn Log,
    hooks: &'a ApplyHooks,
}

/// A selected file that passed steps 1–5 of Phase A.
struct Stage {
    /// The file's current text (hash-checked against the plan).
    original: String,
    /// The edited text, not yet verified.
    edited: String,
    /// The edits turning `original` into `edited` (for the dry-run diff).
    edits: Vec<Edit>,
    /// `original`'s functional comments, for the verifier.
    functional: Vec<FunctionalComment>,
    /// Edits `compute_edits` did not make.
    skipped: Vec<SkippedEdit>,
}

/// A staged file awaiting verification and its write.
struct Staged {
    /// Index into the artifact's files.
    index: usize,
    /// The path to write.
    target: PathBuf,
    stage: Stage,
}

/// The run-level failures collected across Phase A.
#[derive(Default)]
struct Aborts {
    /// Range mismatches and other artifact/codebase disagreement (exit 2).
    artifact: Vec<String>,
    /// Drift (exit 3).
    drift: Vec<String>,
}

impl Aborts {
    fn push(&mut self, path: &str, err: Error) {
        match err {
            Error::Drift(detail) => self.drift.push(format!("{path}: {detail}")),
            Error::Artifact(detail) => self.artifact.push(format!("{path}: {detail}")),
            other => self.artifact.push(format!("{path}: {other}")),
        }
    }

    /// The abort, if any: artifact disagreement outranks drift (exit 2
    /// before 3); the message lists every offending file of the winning
    /// kind, and notes the other kind's count.
    fn into_result(self) -> Result<(), Error> {
        let list = |items: &[String]| items.join("; ");
        match (self.artifact.is_empty(), self.drift.is_empty()) {
            (true, true) => Ok(()),
            (false, drift_free) => {
                let mut message = format!(
                    "the artifact does not match {} selected file(s); nothing was written: {}",
                    self.artifact.len(),
                    list(&self.artifact)
                );
                if !drift_free {
                    let _ = write!(message, " (also drifted: {})", list(&self.drift));
                }
                Err(Error::Artifact(message))
            }
            (true, false) => Err(Error::Drift(format!(
                "{} selected file(s) changed since the plan; nothing was written: {}",
                self.drift.len(),
                list(&self.drift)
            ))),
        }
    }
}

/// The per-file status of an error from staging or verification; `None`
/// for the run-level aborts ([`Error::Artifact`], [`Error::Drift`]). Errors
/// no per-file step produces (usage, LLM) can only come from a bug, so
/// they are invariant failures.
fn per_file_status(err: &Error) -> Option<ApplyStatus> {
    match err {
        Error::Artifact(_) | Error::Drift(_) => None,
        Error::Verify(_) => Some(ApplyStatus::SkippedVerify),
        Error::Firewall(_) => Some(ApplyStatus::SkippedFirewall),
        Error::Fs { .. } => Some(ApplyStatus::FailedIo),
        Error::Invariant(_) | Error::Usage(_) | Error::Llm { .. } => {
            Some(ApplyStatus::FailedInvariant)
        }
    }
}

/// Phase A steps 1–5 for one selected `complete` file with an actionable
/// edit.
fn stage_file(file: &FilePlan, target: &Path, ctx: &FileCtx<'_>) -> Result<Stage, Error> {
    let bytes = read_planned_file(file, target, ctx.log)?;
    let original = decode_utf8(&bytes)
        .ok_or_else(|| {
            Error::Artifact("the file matches its contentHash but is not valid UTF-8".to_owned())
        })?
        .to_owned();
    let inventory = inventory_file(
        &original,
        FileRef {
            language: file.language,
            file_name: file.path.clone(),
        },
    )?;
    check_scanned_records(file, &inventory)?;
    check_new_offsets(file, &original, &inventory, ctx.allow_add)?;
    let functional = functional_sequence(&inventory);
    let (edited, edits, skipped) = match &ctx.hooks.apply_file {
        Some(hook) => {
            let edited = hook(&original, file, &inventory)?;
            let edits = spanning_edit(&original, &edited).into_iter().collect();
            (edited, edits, Vec::new())
        }
        None => {
            let computed = compute_edits(
                &original,
                file,
                &inventory,
                adapter_for(file.language),
                EditOpts {
                    allow_add: ctx.allow_add,
                },
            )?;
            check_functional_intersections(&computed.edits, &inventory)?;
            let edited = apply_edits(&original, &computed.edits)?;
            (edited, computed.edits, computed.skipped)
        }
    };
    Ok(Stage {
        original,
        edited,
        edits,
        functional,
        skipped,
    })
}

/// The one edit turning `original` into `edited` that spans exactly their
/// difference (the longest common prefix and suffix are left alone, cut
/// at `char` boundaries); `None` when they are equal. It stands in for the
/// edit list when [`ApplyHooks::apply_file`] supplies the edited text
/// alone, so the dry-run diff still sees `apply_edits(original, edits) ==
/// edited`.
fn spanning_edit(original: &str, edited: &str) -> Option<Edit> {
    if original == edited {
        return None;
    }
    let prefix: usize = original
        .chars()
        .zip(edited.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let (old_rest, new_rest) = (&original[prefix..], &edited[prefix..]);
    let suffix: usize = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    Some(Edit {
        start: prefix,
        length: old_rest.len() - suffix,
        replacement: new_rest[..new_rest.len() - suffix].to_owned(),
    })
}

/// Step 6: the semantic verifier.
fn verify_staged(file: &FilePlan, stage: &Stage) -> Result<(), Error> {
    verify_semantic_equality(
        &stage.original,
        &stage.edited,
        FileRef {
            language: file.language,
            file_name: file.path.clone(),
        },
        &stage.functional,
    )
}

/// Step 1: the link checks, the read and the drift check. A deleted file,
/// a symlink or non-file at the planned path, and bytes that no longer
/// hash to `contentHash` are [`Error::Drift`]; a failure to read an
/// existing file is [`Error::Fs`] (`failed-io`). A hard-linked file gets a
/// warning: the atomic rename will split the link.
fn read_planned_file(file: &FilePlan, target: &Path, log: &dyn Log) -> Result<Vec<u8>, Error> {
    let fs_err = |source: std::io::Error| Error::Fs {
        path: target.to_path_buf(),
        source,
    };
    let expected = file
        .content_hash
        .as_deref()
        .ok_or_else(|| Error::Artifact("a complete file has no contentHash".to_owned()))?;
    let meta = match fs::symlink_metadata(target) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(Error::Drift("the file has been deleted".to_owned()));
        }
        Err(err) => return Err(fs_err(err)),
    };
    if meta.file_type().is_symlink() {
        return Err(Error::Drift(
            "the planned path is now a symlink (the plan never records symlinks)".to_owned(),
        ));
    }
    if !meta.is_file() {
        return Err(Error::Drift(
            "the planned path is no longer a regular file".to_owned(),
        ));
    }
    warn_if_hard_linked(&meta, &file.path, log);
    let bytes = match fs::read(target) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(Error::Drift("the file has been deleted".to_owned()));
        }
        Err(err) => return Err(fs_err(err)),
    };
    let actual = content_hash(&bytes);
    if actual != expected {
        return Err(Error::Drift(format!(
            "content hash {actual} does not match the planned {expected}"
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn warn_if_hard_linked(meta: &fs::Metadata, path: &str, log: &dyn Log) {
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() > 1 {
        log.warn(&format!(
            "apply: {path} has {} hard links; writing it replaces this link only and splits it from the others",
            meta.nlink()
        ));
    }
}

#[cfg(not(unix))]
fn warn_if_hard_linked(_meta: &fs::Metadata, _path: &str, _log: &dyn Log) {}

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

/// Phase B for one verified file: re-checks that the target is still the
/// regular file whose bytes Phase A verified, then writes it atomically
/// (permissions preserved). A change since Phase A, and any I/O failure,
/// is `failed-io`.
fn write_staged(file: &FilePlan, target: &Path, stage: Stage) -> ApplyFileResult {
    let outcome = |status, reason: Option<String>, skipped_edits| ApplyFileResult {
        path: file.path.clone(),
        status,
        reason,
        skipped_edits,
    };
    match unchanged_since_phase_a(target, &stage.original) {
        Ok(None) => {}
        Ok(Some(change)) => {
            return outcome(
                ApplyStatus::FailedIo,
                Some(format!("changed during apply: {change}")),
                stage.skipped,
            );
        }
        Err(err) => {
            return outcome(ApplyStatus::FailedIo, Some(err.to_string()), stage.skipped);
        }
    }
    match atomic_write(target, stage.edited.as_bytes()) {
        Ok(()) => outcome(ApplyStatus::Applied, None, stage.skipped),
        Err(err) => outcome(ApplyStatus::FailedIo, Some(err.to_string()), stage.skipped),
    }
}

/// `None` when `target` is still a regular file holding exactly
/// `original`'s bytes; otherwise what changed. A failure to inspect an
/// existing target is [`Error::Fs`].
fn unchanged_since_phase_a(target: &Path, original: &str) -> Result<Option<&'static str>, Error> {
    let fs_err = |source: std::io::Error| Error::Fs {
        path: target.to_path_buf(),
        source,
    };
    let meta = match fs::symlink_metadata(target) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Some("the file was deleted")),
        Err(err) => return Err(fs_err(err)),
    };
    if !meta.file_type().is_file() {
        return Ok(Some("the path is no longer a regular file"));
    }
    let bytes = match fs::read(target) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Some("the file was deleted")),
        Err(err) => return Err(fs_err(err)),
    };
    if content_hash(&bytes) != content_hash(original.as_bytes()) {
        return Ok(Some("the content no longer matches the verified text"));
    }
    Ok(None)
}

/// Logs every non-applied outcome worth attention, then the per-status
/// counts.
fn log_summary(log: &dyn Log, result: &ApplyResult) {
    for file in &result.files {
        let reason = file.reason.as_deref().unwrap_or("");
        match file.status {
            ApplyStatus::SkippedFirewall
            | ApplyStatus::SkippedVerify
            | ApplyStatus::FailedInvariant
            | ApplyStatus::FailedIo => {
                log.warn(&format!(
                    "apply: {} {}: {reason}",
                    wire_name(&file.status),
                    file.path
                ));
            }
            ApplyStatus::Applied
            | ApplyStatus::Unchanged
            | ApplyStatus::DryRun
            | ApplyStatus::NotSelected
            | ApplyStatus::SkippedPlanStatus => {}
        }
        for skipped in &file.skipped_edits {
            log.info(&format!(
                "apply: {}: kept {} as is ({})",
                file.path,
                skipped.id,
                wire_name(&skipped.reason)
            ));
        }
    }
    let parts: Vec<String> = ORDERED_STATUSES
        .iter()
        .filter_map(|status| {
            let count = result
                .files
                .iter()
                .filter(|file| file.status == *status)
                .count();
            (count > 0).then(|| format!("{count} {}", wire_name(status)))
        })
        .collect();
    let body = if parts.is_empty() {
        "no files".to_owned()
    } else {
        parts.join(", ")
    };
    log.info(&format!("apply: {body}"));
}

/// The summary's status order.
const ORDERED_STATUSES: [ApplyStatus; 9] = [
    ApplyStatus::Applied,
    ApplyStatus::DryRun,
    ApplyStatus::Unchanged,
    ApplyStatus::NotSelected,
    ApplyStatus::SkippedPlanStatus,
    ApplyStatus::SkippedFirewall,
    ApplyStatus::SkippedVerify,
    ApplyStatus::FailedInvariant,
    ApplyStatus::FailedIo,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(original: &str, edited: &str) -> Option<Edit> {
        let edit = spanning_edit(original, edited);
        let edits: Vec<Edit> = edit.iter().cloned().collect();
        assert_eq!(apply_edits(original, &edits).unwrap(), edited);
        edit
    }

    #[test]
    fn spanning_edit_covers_exactly_the_difference() {
        assert_eq!(roundtrip("same", "same"), None);
        let edit = roundtrip("a // c\nb\n", "a\nb\n").unwrap();
        assert_eq!(
            (edit.start, edit.length, edit.replacement.as_str()),
            (1, 5, "")
        );
        let edit = roundtrip("x\n", "// n\nx\n").unwrap();
        assert_eq!(
            (edit.start, edit.length, edit.replacement.as_str()),
            (0, 0, "// n\n")
        );
        // Prefix and suffix overlap in a repeated run: still one edit.
        roundtrip("aaa", "aa");
        roundtrip("aa", "aaa");
    }

    #[test]
    fn spanning_edit_cuts_at_char_boundaries() {
        // `é` (C3 A9) and `è` (C3 A8) share their first byte.
        let edit = roundtrip("café\n", "cafè\n").unwrap();
        assert_eq!(
            (edit.start, edit.length, edit.replacement.as_str()),
            (3, 2, "è")
        );
        roundtrip("ü", "");
        roundtrip("", "ü");
    }
}
