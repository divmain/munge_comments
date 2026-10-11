//! The plan orchestrator: [`run_plan`] turns a set of discovery inputs into
//! a plan artifact.
//!
//! **Run level.** The request is validated (the library contract), the
//! output paths are resolved under the absolute-path rule (their parent
//! directories created and canonicalized), an existing `out` is refused
//! without `resume`, and files are discovered. The artifact `root` is
//! computed over the canonical discovered paths ([`compute_root`]) and
//! resolved back to an absolute directory exactly as `apply` will resolve
//! it ([`resolve_root`]); every `FilePlan.path`, the snapshot `paths` and
//! `globBase` are expressed relative to it. The [`ArtifactWriter`] writes
//! the header immediately, then once per finished file and once at the end.
//!
//! **Pool.** Files are processed concurrently under a bounded pool,
//! `stream::iter(files).take_while(not cancelled).map(process_file)
//! .buffer_unordered(concurrency)`, driven by this task alone: the writer
//! is owned by the driver loop, so writes are serialized by construction.
//!
//! **Run control.** Scheduling stops on a token that is a child of the
//! caller's [`CancellationToken`]; in-flight files always finish. It is
//! cancelled by:
//! - the user (`PlanDeps::cancel`): unprocessed files are simply absent
//!   from the artifact, which is still finished along with the report, and
//!   the outcome is `aborted` with exit 130 whatever else happened;
//! - an endpoint authentication failure (`Error::Llm { reason: Auth }`),
//!   which is run-fatal: every discovered file without an outcome is
//!   recorded `failed`/`llm` ("not attempted: …"), and the artifact and
//!   report are still finished (exit 5 by the precedence);
//! - a failed artifact write: the run returns the writer's own error
//!   (`Error::Fs` → 6, a summary `Error::Invariant` → 7).
//!
//! **Per file.** One `spawn_blocking` closure over owned data does the CPU
//! work — read, strict UTF-8 decode, the Python declared-encoding gate,
//! the syntax gate (`too_deep`, `has_error`) and the inventory — so the
//! driver task's in-flight HTTP futures are never stalled. Then either the
//! offline branch (heuristic-only decisions, no transform, no LLM) or
//! call #1 ([`regenerate_file`]) followed by call #2 ([`judge_file`]).
//! Last, on a second `spawn_blocking` closure, the plan-time verification
//! rail ([`verify_rail`]) runs apply's in-memory pipeline on the planned
//! records and demotes any edit that would fail verification, so every
//! `complete` file of a fresh artifact applies cleanly.
//! Any error is recorded as a per-file failure classified by variant; the
//! run never aborts because of one file. A panic inside either blocking
//! closure is re-raised as a panic, so the CLI still maps it to exit 7.
//!
//! **Resume.** With `resume` and an existing `out`, the artifact is loaded
//! and validated, its `root` and `globBase` are reused (discovery matches
//! globs against the stored base; every discovered file must lie under the
//! stored root), the request must match its config snapshot, and
//! [`reconcile`] carries the unchanged `complete`/`skipped` files into the
//! writer's first write — only failed, changed and new files are planned.
//! `createdAt` is preserved; the config records the new `concurrency`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use futures::StreamExt;
use futures::future::ready;
use futures::stream;
use tokio_util::sync::CancellationToken;

use super::judge::judge_file;
use super::records::{build_draft_records, finalize_record};
use super::regenerate::{FileInput, regenerate_file};
use super::resume::{
    carry_candidates, check_under_root, load_for_resume, reconcile, resume_base, stored_paths,
};
use super::verify_rail::{VerifyFn, verify_rail};
use crate::apply::verify::verify_semantic_equality;
use crate::artifact::types::{
    CommentRecord, DecidedBy, Decision, FailureKind, FilePlan, FileStatus, FileWarning,
    PlanArtifact, SkipReason, ValidatedArtifact,
};
use crate::artifact::validate::{compute_root, content_hash, resolve_root};
use crate::artifact::write::{ArtifactHeader, ArtifactWriter};
use crate::comments::classify::{declared_encoding, is_utf8_alias};
use crate::comments::inventory::{Inventory, inventory_file};
use crate::comments::types::FileRef;
use crate::config::{Mode, PlanConfig};
use crate::discover::{DiscoverOptions, discover};
use crate::errors::{Error, LlmErrorReason, pick_exit_code};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::lang::registry::{Registry, registry};
use crate::llm::client::LlmClient;
use crate::log::Log;
use crate::report::markdown::render_markdown;
use crate::report::stats::{compute_summary, convergence_pct};
use crate::util::fs::{atomic_write, ensure_parent_dir};
use crate::util::paths::relative_posix;
use crate::util::time::utc_rfc3339;

/// The tag offline mode puts on every comment it keeps by default.
pub const OFFLINE_TAG: &str = "offline";

/// The exit code of an interrupted plan run (outside the precedence).
pub const INTERRUPTED_EXIT_CODE: u8 = 130;

/// What to plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRequest {
    /// The resolved configuration. `paths` holds the discovery inputs as
    /// given (relative to [`PlanDeps::cwd`], or absolute); the artifact
    /// stores them root-relative.
    pub config: PlanConfig,
    /// The artifact path, relative to the cwd (or absolute).
    pub out: PathBuf,
    /// Where to write the post-plan markdown report, relative to the cwd;
    /// `None` writes no report.
    pub report_path: Option<PathBuf>,
    /// Continue from an existing `out` artifact.
    pub resume: bool,
}

/// The inventory step [`PlanHooks::inventory`] substitutes:
/// `(text, file) → inventory`, exactly the shape of [`inventory_file`].
pub type InventoryHook = dyn Fn(&str, FileRef) -> Result<Inventory, Error> + Send + Sync;

/// Test seams for [`run_plan`]; the default (all `None`) is production
/// behavior.
#[derive(Default, Clone)]
pub struct PlanHooks {
    /// Replaces [`inventory_file`] in the per-file pipeline (it runs inside
    /// the blocking stage, hence `Send + Sync`). Its errors are classified
    /// like the real inventory's.
    pub inventory: Option<Arc<InventoryHook>>,
    /// Replaces [`verify_semantic_equality`] in the plan-time verification
    /// rail (it runs inside the rail's blocking stage). Its verdicts are
    /// treated like the real verifier's: [`Error::Verify`] demotes, any
    /// other error fails the file.
    pub verify: Option<Arc<VerifyFn>>,
}

/// The ambient inputs of [`run_plan`].
pub struct PlanDeps<'a> {
    /// Progress, warnings and the summary.
    pub log: &'a dyn Log,
    /// The directory every request path is resolved against.
    pub cwd: &'a Path,
    /// The model client; required unless `config.offline`.
    pub llm: Option<Arc<dyn LlmClient>>,
    /// User cancellation: stops scheduling new files (in-flight files
    /// finish) and makes the outcome `aborted` with exit 130.
    pub cancel: CancellationToken,
    /// Test seams.
    pub hooks: PlanHooks,
}

/// The outcome of a plan run that produced an artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanOutcome {
    /// The artifact as finally written.
    pub artifact: PlanArtifact,
    /// The process exit code the run maps to.
    pub exit_code: u8,
    /// Whether the user cancelled the run before it finished (the artifact
    /// is valid but partial; `exit_code` is 130).
    pub aborted: bool,
}

/// Plans the files `req.config.paths` selects (see the module docs).
///
/// `deps.cwd` is canonicalized once; `out`, `report_path` and every
/// discovery input are resolved against it, and the output paths' parent
/// directories are created and canonicalized before anything is written.
///
/// # Errors
///
/// Run-level failures only — per-file failures (and the files an endpoint
/// authentication failure left unattempted) are recorded in the artifact
/// and folded into [`PlanOutcome::exit_code`]; a user cancellation returns
/// `Ok` with [`PlanOutcome::aborted`]:
/// - [`Error::Usage`] (exit 1): an invalid request (online without a
///   client, `concurrency < 1`, `max_file_chars < 1`, a similarity
///   threshold outside `[0, 1]`, an output path naming no file, the report
///   and the artifact at the same path), an existing `out` without
///   `resume`, a `resume` snapshot mismatch (with a field diff), a resumed
///   discovery that escapes the stored root, or a discovery input error;
/// - [`Error::Artifact`] (exit 2): the artifact to resume is invalid, or
///   its stored root or glob base no longer resolves;
/// - [`Error::Fs`] (exit 6): the cwd or an output directory cannot be
///   resolved or created, or an artifact write fails (the files in flight
///   finish first; nothing more is scheduled);
/// - [`Error::Invariant`] (exit 7): the grammar registry failed to
///   initialize, or a path or summary invariant tripped.
pub async fn run_plan(req: PlanRequest, deps: PlanDeps<'_>) -> Result<PlanOutcome, Error> {
    validate_request(&req, deps.llm.is_some())?;
    let cwd = fs::canonicalize(deps.cwd).map_err(|source| Error::Fs {
        path: deps.cwd.to_path_buf(),
        source,
    })?;

    // 1. Output paths, and the existing-artifact check.
    let out = output_target(&cwd, &req.out, "artifact")?;
    let report_target = req
        .report_path
        .as_deref()
        .map(|report| output_target(&cwd, report, "report"))
        .transpose()?;
    if report_target.as_ref() == Some(&out) {
        return Err(Error::Usage(format!(
            "the report and the artifact cannot be the same file ({})",
            out.display()
        )));
    }
    let existing = prepare_start(&out, req.resume, deps.log)?;
    // A resumed run reuses the artifact's root and glob base, after its
    // snapshot check (before any discovery work).
    let resumed = existing
        .map(|artifact| resume_base(artifact, &out, &cwd, &req.config))
        .transpose()?;

    // 2. Discovery.
    let discovered = discover(
        &req.config.paths,
        DiscoverOptions {
            include: req.config.include.as_deref(),
            exclude: &req.config.exclude,
            cwd: &cwd,
            glob_base: resumed.as_ref().map(|base| base.glob_base_abs.as_path()),
            log: deps.log,
        },
    )?;
    // 3. The grammar registry (a failed initialization is run-fatal).
    let registry = registry()?;

    // 4. Root, glob base, snapshot paths and the per-file jobs.
    let (root, root_abs, glob_base) = match &resumed {
        Some(base) => {
            check_under_root(&discovered.files, &base.root_abs)?;
            (
                base.root.clone(),
                base.root_abs.clone(),
                base.glob_base.clone(),
            )
        }
        None => {
            let out_dir = out.parent().ok_or_else(|| {
                Error::Invariant(format!("artifact path {} has no parent", out.display()))
            })?;
            let abs_paths: Vec<PathBuf> = discovered
                .files
                .iter()
                .map(|file| file.abs_path.clone())
                .collect();
            let root = compute_root(&cwd, &abs_paths, out_dir)?;
            let root_abs = resolve_root(&out, &root)?;
            let glob_base = root_relative(&root_abs, &cwd)?;
            (root, root_abs, glob_base)
        }
    };
    let mut config = req.config;
    config.paths = stored_paths(&config.paths, &cwd, &root_abs)?;
    let mut jobs = discovered
        .files
        .iter()
        .map(|file| {
            Ok(FileJob {
                path: root_relative(&root_abs, &file.abs_path)?,
                abs_path: file.abs_path.clone(),
                language: file.language,
            })
        })
        .collect::<Result<Vec<FileJob>, Error>>()?;

    // 5. Resume reconciliation, then the writer performs the first write
    // (the header plus any carried files).
    let (created_at, carried) = match resumed {
        Some(base) => {
            let (carried, process) = reconcile_jobs(base.files, jobs, deps.log).await?;
            jobs = process;
            (base.created_at, carried)
        }
        None => (utc_rfc3339(SystemTime::now()), Vec::new()),
    };
    let header = ArtifactHeader {
        created_at,
        root,
        glob_base,
        config: config.clone(),
    };
    let carried_count = carried.len();
    let mut writer = ArtifactWriter::new(&out, header, carried, compute_summary)?;
    let carried_note = if carried_count > 0 {
        format!(" ({carried_count} carried from the existing artifact)")
    } else {
        String::new()
    };
    deps.log.info(&format!(
        "plan: {} file(s) to plan{carried_note}; writing {}",
        jobs.len(),
        out.display()
    ));

    // 6. The pool, driven by this task; each finished file is persisted.
    // `stop` (a child of the user's token) halts scheduling on user
    // cancellation and on the run-fatal conditions: an endpoint auth
    // failure, or a failed artifact write. In-flight files always finish.
    let stop = deps.cancel.child_token();
    let ctx = FileCtx {
        config: &config,
        llm: deps.llm.clone(),
        registry,
        inventory_hook: deps.hooks.inventory.clone(),
        verify_hook: deps.hooks.verify.clone(),
        log: deps.log,
        stop: stop.clone(),
        auth_failed: AtomicBool::new(false),
    };
    let concurrency = usize::try_from(config.concurrency).unwrap_or(usize::MAX);
    let mut tally = Tally {
        total: jobs.len(),
        ..Tally::default()
    };
    let mut unrecorded: BTreeMap<String, FileJob> = jobs
        .iter()
        .map(|job| (job.path.clone(), job.clone()))
        .collect();
    let mut write_failure: Option<Error> = None;
    let mut finished = stream::iter(jobs)
        .take_while(|_| ready(!stop.is_cancelled()))
        .map(|job| process_file(job, &ctx))
        .buffer_unordered(concurrency);
    while let Some(file) = finished.next().await {
        let file = file?;
        if write_failure.is_some() {
            // The artifact can no longer be persisted; in-flight files are
            // drained so no work is torn down mid-call, then the run fails.
            continue;
        }
        unrecorded.remove(&file.path);
        tally.count(&file);
        log_file(deps.log, &file, &tally);
        if let Err(err) = writer.record(file) {
            deps.log.warn(&format!(
                "plan: writing the artifact failed ({err}); stopping after the files in flight"
            ));
            stop.cancel();
            write_failure = Some(err);
        }
    }
    drop(finished);
    if let Some(err) = write_failure {
        return Err(err);
    }
    // A user cancellation observed before the pool drained: the artifact
    // is valid but partial, and the run exits 130 whatever else happened.
    let aborted = deps.cancel.is_cancelled();
    if !aborted && ctx.auth_failed.load(Ordering::Relaxed) && !unrecorded.is_empty() {
        // Auth failures are run-fatal: every file that never ran is
        // recorded as an `llm` failure, so `--resume` retries it.
        let pending: Vec<FileJob> = unrecorded.into_values().collect();
        let mode = config.mode;
        let not_attempted = blocking("(not attempted)", "hashing stage", move || {
            pending
                .iter()
                .map(|job| not_attempted(job, mode))
                .collect::<Vec<FilePlan>>()
        })
        .await?;
        deps.log.warn(&format!(
            "plan: endpoint authentication failed; {} file(s) were not attempted",
            not_attempted.len()
        ));
        writer.record_all(not_attempted)?;
    }

    // 7. The final write, always.
    let artifact = writer.finish()?;
    let mut exit_code = if aborted {
        INTERRUPTED_EXIT_CODE
    } else {
        exit_code_for(&artifact.files)
    };
    if let Some(target) = report_target {
        match write_report(&artifact, &target) {
            Ok(()) => deps
                .log
                .info(&format!("plan: wrote report {}", target.display())),
            Err(err) => {
                deps.log.warn(&format!("plan: {err}"));
                if !aborted {
                    exit_code = pick_exit_code(&[exit_code, err.exit_code()]);
                }
            }
        }
    }
    log_summary(deps.log, &artifact, discovered.walk_errors);
    Ok(PlanOutcome {
        artifact,
        exit_code,
        aborted,
    })
}

/// The library contract checked at the boundary, before any filesystem
/// access: an online run needs a client, and the numeric config fields
/// must be in range ([`PlanConfig::validate`]).
fn validate_request(req: &PlanRequest, has_llm: bool) -> Result<(), Error> {
    if !req.config.offline && !has_llm {
        return Err(Error::Usage(
            "an online plan needs an LLM client (PlanDeps.llm), or set config.offline".to_owned(),
        ));
    }
    req.config.validate()
}

/// The canonical output path: `path` joined onto the canonical cwd, its
/// parent directory created and canonicalized, its file name joined back.
fn output_target(cwd: &Path, path: &Path, what: &str) -> Result<PathBuf, Error> {
    let joined = cwd.join(path);
    let name = joined.file_name().ok_or_else(|| {
        Error::Usage(format!(
            "the {what} path must name a file, got {}",
            path.display()
        ))
    })?;
    Ok(ensure_parent_dir(&joined)?.join(name))
}

/// Loads the artifact to resume, if any. Without `resume`, an existing
/// `out` is refused so paid LLM work is never overwritten by accident;
/// with `resume` and no artifact yet, the run starts fresh (one log note).
fn prepare_start(out: &Path, resume: bool, log: &dyn Log) -> Result<Option<PlanArtifact>, Error> {
    if resume {
        let existing = load_for_resume(out)?;
        if existing.is_none() {
            log.info(&format!(
                "plan: no artifact at {} yet; starting a fresh plan",
                out.display()
            ));
        }
        return Ok(existing.map(ValidatedArtifact::into_inner));
    }
    match fs::symlink_metadata(out) {
        Ok(_) => Err(Error::Usage(format!(
            "{} already exists; pass --resume to continue it, or choose another --out",
            out.display()
        ))),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Fs {
            path: out.to_path_buf(),
            source,
        }),
    }
}

/// Splits a resumed run's discovered `jobs` against the `existing`
/// artifact entries ([`reconcile`]): returns the carried file plans and
/// the jobs left to process. Carry candidates are hashed on a blocking
/// stage; a file that cannot be read is simply processed (and fails as
/// `io` there). Each dropped entry is logged.
async fn reconcile_jobs(
    existing: Vec<FilePlan>,
    jobs: Vec<FileJob>,
    log: &dyn Log,
) -> Result<(Vec<FilePlan>, Vec<FileJob>), Error> {
    let candidates = carry_candidates(&existing);
    let to_hash: Vec<(String, PathBuf)> = jobs
        .iter()
        .filter(|job| candidates.contains(&job.path))
        .map(|job| (job.path.clone(), job.abs_path.clone()))
        .collect();
    let disk_hashes = blocking("(resume)", "hashing stage", move || {
        to_hash
            .into_iter()
            .filter_map(|(path, abs_path)| {
                fs::read(&abs_path)
                    .ok()
                    .map(|bytes| (path, content_hash(&bytes)))
            })
            .collect::<BTreeMap<String, String>>()
    })
    .await?;
    let paths: Vec<String> = jobs.iter().map(|job| job.path.clone()).collect();
    let reconciled = reconcile(existing, &paths, &disk_hashes);
    for path in &reconciled.dropped {
        log.info(&format!(
            "plan: {path} is no longer discovered; dropped from the artifact"
        ));
    }
    log.info(&format!(
        "plan: resuming: {} file(s) carried, {} to plan, {} dropped",
        reconciled.carry.len(),
        reconciled.process.len(),
        reconciled.dropped.len()
    ));
    let process: BTreeSet<&str> = reconciled.process.iter().map(String::as_str).collect();
    let jobs = jobs
        .into_iter()
        .filter(|job| process.contains(job.path.as_str()))
        .collect();
    Ok((reconciled.carry, jobs))
}

/// `target` relative to the absolute root, as POSIX.
fn root_relative(root: &Path, target: &Path) -> Result<String, Error> {
    relative_posix(root, target).ok_or_else(|| {
        Error::Invariant(format!(
            "{} has no POSIX spelling relative to the source root {}",
            target.display(),
            root.display()
        ))
    })
}

/// Renders and atomically writes the post-plan report.
fn write_report(artifact: &PlanArtifact, target: &Path) -> Result<(), Error> {
    let markdown = render_markdown(artifact, None)?;
    atomic_write(target, markdown.as_bytes())
}

/// The exit code of a plan's per-file outcomes, by the precedence of
/// [`pick_exit_code`]: `invariant` → 7, `llm` → 5, `io` → 6. Complete and
/// skipped files are success.
pub fn exit_code_for(files: &[FilePlan]) -> u8 {
    let codes: Vec<u8> = files
        .iter()
        .filter_map(|file| match file.failure_kind? {
            FailureKind::Invariant => Some(7),
            FailureKind::Llm => Some(5),
            FailureKind::Io => Some(6),
        })
        .collect();
    pick_exit_code(&codes)
}

/// One discovered file to plan.
#[derive(Debug, Clone)]
struct FileJob {
    /// Root-relative POSIX path (the artifact's `FilePlan.path`).
    path: String,
    /// Canonical absolute path.
    abs_path: PathBuf,
    /// The language its extension maps to.
    language: LanguageId,
}

/// Everything the per-file pipeline shares.
struct FileCtx<'a> {
    config: &'a PlanConfig,
    llm: Option<Arc<dyn LlmClient>>,
    registry: &'static Registry,
    inventory_hook: Option<Arc<InventoryHook>>,
    verify_hook: Option<Arc<VerifyFn>>,
    log: &'a dyn Log,
    /// The run's stop token, cancelled by a run-fatal condition.
    stop: CancellationToken,
    /// Set once a file fails with an endpoint authentication error.
    auth_failed: AtomicBool,
}

impl FileCtx<'_> {
    /// A failed file (see [`failed`]). An endpoint authentication failure
    /// is also run-fatal: every later call would fail the same way, so it
    /// stops scheduling (files already in flight still finish).
    fn failed(&self, job: &FileJob, facts: Option<&Facts>, err: &Error) -> FilePlan {
        if matches!(
            err,
            Error::Llm {
                reason: LlmErrorReason::Auth,
                ..
            }
        ) && !self.auth_failed.swap(true, Ordering::Relaxed)
        {
            self.log.warn(&format!(
                "plan: {}: endpoint authentication failed; scheduling no further files",
                job.path
            ));
            self.stop.cancel();
        }
        failed(job, self.config.mode, facts, err)
    }
}

/// What reading a file established.
struct Facts {
    content_hash: String,
    byte_length: u64,
}

/// The result of the blocking CPU stage.
enum Prepared {
    /// The bytes could not be read.
    Unread(std::io::Error),
    /// The bytes were read; the gates and the inventory ran on them.
    Read {
        facts: Facts,
        outcome: Result<Gate, Error>,
    },
}

/// What the gates decided about a readable file.
enum Gate {
    /// A deterministic refusal of unsupported input.
    Skip { reason: SkipReason, detail: String },
    /// Clean UTF-8 that parses, with its inventory.
    Ready { text: String, inventory: Inventory },
}

/// The planned records of one complete file — the input of the plan-time
/// verification rail.
struct PlannedFile {
    effective_mode: Mode,
    warnings: Vec<FileWarning>,
    comments: Vec<CommentRecord>,
}

/// Plans one file. `Ok` carries its `FilePlan` whatever its status (every
/// per-file error is recorded as a failure); `Err` is reserved for
/// run-fatal conditions.
async fn process_file(job: FileJob, ctx: &FileCtx<'_>) -> Result<FilePlan, Error> {
    let abs_path = job.abs_path.clone();
    let path = job.path.clone();
    let language = job.language;
    let registry = ctx.registry;
    let hook = ctx.inventory_hook.clone();
    let prepared = blocking(&job.path, "planning stage", move || {
        prepare(&abs_path, &path, language, registry, hook.as_deref())
    })
    .await?;
    let mode = ctx.config.mode;
    let (facts, outcome) = match prepared {
        Prepared::Unread(source) => {
            let err = Error::Fs {
                path: job.abs_path.clone(),
                source,
            };
            return Ok(ctx.failed(&job, None, &err));
        }
        Prepared::Read { facts, outcome } => (facts, outcome),
    };
    let (text, inventory) = match outcome {
        Err(err) => return Ok(ctx.failed(&job, Some(&facts), &err)),
        Ok(Gate::Skip { reason, detail }) => {
            return Ok(skipped(&job, mode, &facts, reason, detail));
        }
        Ok(Gate::Ready { text, inventory }) => (text, inventory),
    };
    let input = FileInput {
        path: &job.path,
        language: job.language,
        text: &text,
        inventory: &inventory,
    };
    let planned = match plan_comments(input, ctx).await {
        Ok(planned) => complete(&job, &facts, planned),
        Err(err) => return Ok(ctx.failed(&job, Some(&facts), &err)),
    };
    // The plan-time verification rail, last: on its own blocking stage, as
    // a demotion may cost O(n) verifications.
    let allow_add = ctx.config.allow_add;
    let verify_hook = ctx.verify_hook.clone();
    let railed = blocking(&job.path, "verification rail", move || {
        let mut planned = planned;
        let verify: &VerifyFn = verify_hook.as_deref().unwrap_or(&verify_semantic_equality);
        verify_rail(&mut planned, &text, &inventory, allow_add, verify)
            .map(|report| (planned, report))
    })
    .await?;
    Ok(match railed {
        Ok((planned, report)) => {
            if report.demoted > 0 {
                ctx.log.warn(&format!(
                    "plan: {}: the verification rail demoted {} edit(s) that would fail \
                     verification at apply time",
                    job.path, report.demoted
                ));
            }
            planned
        }
        Err(err) => ctx.failed(&job, Some(&facts), &err),
    })
}

/// Runs `stage` (named `what` in errors) on the blocking pool for the file
/// at `path`. A panic inside it is re-raised as a panic, so it stays a bug
/// the CLI maps to exit 7; a cancelled task is a run-fatal invariant.
async fn blocking<T: Send + 'static>(
    path: &str,
    what: &str,
    stage: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Error> {
    match tokio::task::spawn_blocking(stage).await {
        Ok(value) => Ok(value),
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => Err(Error::Invariant(format!(
            "{path}: the blocking {what} was cancelled: {err}"
        ))),
    }
}

/// The blocking CPU stage over owned data: read, strict decode, the
/// declared-encoding and syntax gates, and the inventory (or its hook).
fn prepare(
    abs_path: &Path,
    path: &str,
    language: LanguageId,
    registry: &Registry,
    hook: Option<&InventoryHook>,
) -> Prepared {
    let bytes = match fs::read(abs_path) {
        Ok(bytes) => bytes,
        Err(source) => return Prepared::Unread(source),
    };
    let facts = Facts {
        content_hash: content_hash(&bytes),
        byte_length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    };
    let outcome = gate(bytes, path, language, registry, hook);
    Prepared::Read { facts, outcome }
}

/// The gates and the inventory for one file's bytes.
fn gate(
    bytes: Vec<u8>,
    path: &str,
    language: LanguageId,
    registry: &Registry,
    hook: Option<&InventoryHook>,
) -> Result<Gate, Error> {
    let skip = |reason, detail: String| Ok(Gate::Skip { reason, detail });
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            return skip(
                SkipReason::NonUtf8,
                format!("the file is not valid UTF-8 ({})", err.utf8_error()),
            );
        }
    };
    if language == LanguageId::Python
        && let Some(encoding) = declared_encoding(&text)
        && !is_utf8_alias(&encoding)
    {
        return skip(
            SkipReason::DeclaredEncoding,
            format!(
                "the PEP 263 coding cookie declares {encoding:?}, not UTF-8; text written \
                 back as UTF-8 would be mis-decoded"
            ),
        );
    }
    let tree = registry.parse(&text, language)?;
    if tree.too_deep {
        return skip(
            SkipReason::TooDeep,
            format!(
                "the {} syntax tree is nested too deeply to process safely",
                language.as_str()
            ),
        );
    }
    if tree.has_error {
        return skip(
            SkipReason::SyntaxErrors,
            format!(
                "the file does not parse cleanly under the {} grammar",
                language.as_str()
            ),
        );
    }
    drop(tree);
    let file = FileRef {
        language,
        file_name: path.to_owned(),
    };
    let inventory = match hook {
        Some(hook) => hook(&text, file)?,
        None => inventory_file(&text, file)?,
    };
    Ok(Gate::Ready { text, inventory })
}

/// Decides every comment of one readable, clean file: the offline branch,
/// or call #1 then call #2. Records come back sorted by `(start, id)`.
async fn plan_comments(input: FileInput<'_>, ctx: &FileCtx<'_>) -> Result<PlannedFile, Error> {
    let mut planned = if ctx.config.offline {
        plan_offline(input, ctx.config.mode)?
    } else {
        let llm = ctx.llm.as_deref().ok_or_else(|| {
            Error::Invariant("an online plan reached a file without an LLM client".to_owned())
        })?;
        let regen = regenerate_file(input, ctx.config, llm, ctx.log).await?;
        let mut warnings = regen.warnings;
        let judged = judge_file(
            input,
            regen.effective_mode,
            regen.records,
            ctx.config,
            llm,
            ctx.log,
        )
        .await?;
        warnings.extend(judged.warnings);
        PlannedFile {
            effective_mode: regen.effective_mode,
            warnings,
            comments: judged.records,
        }
    };
    planned
        .comments
        .sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    Ok(planned)
}

/// Offline mode: the deterministic rows (1–3, incl. 1b) still decide what
/// they can; every other comment is kept as is — `KEEP_ORIGINAL`,
/// `decidedBy: heuristic`, `confidence: 1`, no difficulty, tag `offline`.
/// No transform runs, so the file is processed in the configured mode.
fn plan_offline(input: FileInput<'_>, mode: Mode) -> Result<PlannedFile, Error> {
    let drafts = build_draft_records(input.inventory, adapter_for(input.language));
    let comments = drafts
        .into_iter()
        .map(|mut draft| {
            if draft.decision.is_none() {
                draft.add_tags([OFFLINE_TAG]);
                draft.decide(Decision::KeepOriginal, DecidedBy::Heuristic, 1.0);
            }
            finalize_record(draft)
        })
        .collect::<Result<Vec<CommentRecord>, Error>>()?;
    Ok(PlannedFile {
        effective_mode: mode,
        warnings: Vec::new(),
        comments,
    })
}

/// The failure kind of a per-file error, by variant: `Llm` → `llm`, `Fs` →
/// `io`, `Invariant` and every other variant (a bug) → `invariant`.
pub fn failure_kind_of(err: &Error) -> FailureKind {
    match err {
        Error::Llm { .. } => FailureKind::Llm,
        Error::Fs { .. } => FailureKind::Io,
        _ => FailureKind::Invariant,
    }
}

/// A file plan with no outcome-specific fields set yet.
fn base_plan(job: &FileJob, mode: Mode, facts: Option<&Facts>, status: FileStatus) -> FilePlan {
    FilePlan {
        path: job.path.clone(),
        content_hash: facts.map(|facts| facts.content_hash.clone()),
        byte_length: facts.map_or(0, |facts| facts.byte_length),
        language: job.language,
        effective_mode: mode,
        status,
        failure_kind: None,
        skip_reason: None,
        error: None,
        warnings: Vec::new(),
        comments: Vec::new(),
    }
}

fn complete(job: &FileJob, facts: &Facts, planned: PlannedFile) -> FilePlan {
    FilePlan {
        warnings: planned.warnings,
        comments: planned.comments,
        ..base_plan(
            job,
            planned.effective_mode,
            Some(facts),
            FileStatus::Complete,
        )
    }
}

fn skipped(
    job: &FileJob,
    mode: Mode,
    facts: &Facts,
    reason: SkipReason,
    detail: String,
) -> FilePlan {
    FilePlan {
        skip_reason: Some(reason),
        error: Some(detail),
        ..base_plan(job, mode, Some(facts), FileStatus::Skipped)
    }
}

/// A failed file, classified by [`failure_kind_of`]. An `io` failure
/// records no hash (and byte length 0): the artifact contract ties a null
/// `contentHash` to exactly that kind.
fn failed(job: &FileJob, mode: Mode, facts: Option<&Facts>, err: &Error) -> FilePlan {
    let kind = failure_kind_of(err);
    let facts = if kind == FailureKind::Io { None } else { facts };
    FilePlan {
        failure_kind: Some(kind),
        error: Some(err.to_string()),
        ..base_plan(job, mode, facts, FileStatus::Failed)
    }
}

/// The message every file left unattempted by an auth failure records.
pub const NOT_ATTEMPTED_AUTH: &str = "not attempted: endpoint authentication failed";

/// A file the run never processed because the endpoint rejected its
/// credentials: `failed`/`llm`, so `--resume` retries it. Its bytes are
/// hashed (the artifact ties a null hash to `io` failures only); a file
/// that cannot even be read is recorded as the `io` failure it is.
fn not_attempted(job: &FileJob, mode: Mode) -> FilePlan {
    match fs::read(&job.abs_path) {
        Ok(bytes) => {
            let facts = Facts {
                content_hash: content_hash(&bytes),
                byte_length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            };
            FilePlan {
                failure_kind: Some(FailureKind::Llm),
                error: Some(NOT_ATTEMPTED_AUTH.to_owned()),
                ..base_plan(job, mode, Some(&facts), FileStatus::Failed)
            }
        }
        Err(source) => failed(
            job,
            mode,
            None,
            &Error::Fs {
                path: job.abs_path.clone(),
                source,
            },
        ),
    }
}

/// The running per-status counts of a run.
#[derive(Debug, Default)]
struct Tally {
    total: usize,
    complete: usize,
    failed: usize,
    skipped: usize,
}

impl Tally {
    fn count(&mut self, file: &FilePlan) {
        match file.status {
            FileStatus::Complete => self.complete += 1,
            FileStatus::Failed => self.failed += 1,
            FileStatus::Skipped => self.skipped += 1,
        }
    }

    fn done(&self) -> usize {
        self.complete + self.failed + self.skipped
    }
}

/// One progress line per finished file, with the running tally. Failed
/// and skipped files are warnings, so they stand out.
fn log_file(log: &dyn Log, file: &FilePlan, tally: &Tally) {
    let counts = format!(
        "{} complete, {} failed, {} skipped",
        tally.complete, tally.failed, tally.skipped
    );
    let progress = format!("plan: [{}/{}]", tally.done(), tally.total);
    let detail = file.error.as_deref().unwrap_or("");
    match file.status {
        FileStatus::Complete => log.info(&format!(
            "{progress} {}: complete, {} comment(s), {} warning(s) ({counts})",
            file.path,
            file.comments.len(),
            file.warnings.len()
        )),
        FileStatus::Failed => log.warn(&format!(
            "{progress} {}: failed ({}): {detail} ({counts})",
            file.path,
            outcome_name(file)
        )),
        FileStatus::Skipped => log.warn(&format!(
            "{progress} {}: skipped ({}): {detail} ({counts})",
            file.path,
            outcome_name(file)
        )),
    }
}

/// The final stderr summary: every skipped and failed file, the run's
/// totals with the convergence metric, and the discovery walk errors.
fn log_summary(log: &dyn Log, artifact: &PlanArtifact, walk_errors: usize) {
    for file in &artifact.files {
        if file.status != FileStatus::Complete {
            log.warn(&format!(
                "plan: {} {} ({}): {}",
                wire_name(&file.status),
                file.path,
                outcome_name(file),
                file.error.as_deref().unwrap_or("")
            ));
        }
    }
    let summary = &artifact.summary;
    log.info(&format!(
        "plan: {} file(s): {} complete, {} failed, {} skipped",
        summary.files_considered,
        summary
            .files_considered
            .saturating_sub(summary.files_failed + summary.files_skipped),
        summary.files_failed,
        summary.files_skipped
    ));
    log.info(&format!(
        "plan: {} comment(s) ({} prose): {} removed, {} rewritten, {} kept, {} needs review, \
         {} protected, {} new, {} new held, {} warning(s); convergence {:.1}%",
        summary.comments_total,
        summary.prose_total,
        summary.removed,
        summary.rewritten,
        summary.kept,
        summary.needs_review,
        summary.protected,
        summary.new,
        summary.new_held,
        summary.warnings,
        convergence_pct(artifact) * 100.0
    ));
    if walk_errors > 0 {
        log.warn(&format!(
            "plan: {walk_errors} discovery walk error(s); see the warnings above"
        ));
    }
}

/// The failure kind or skip reason of a non-complete file, by wire name.
fn outcome_name(file: &FilePlan) -> String {
    match (file.failure_kind, file.skip_reason) {
        (Some(kind), _) => wire_name(&kind),
        (None, Some(reason)) => wire_name(&reason),
        (None, None) => wire_name(&file.status),
    }
}

/// A unit enum's serde (wire) name.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::from("?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{FakeLlm, Scenario};
    use crate::log::CapturingLog;

    fn config(offline: bool) -> PlanConfig {
        PlanConfig {
            mode: Mode::Redact,
            model: "gpt-4.1".to_owned(),
            base_url: None,
            allow_add: false,
            similarity_threshold: 0.6,
            max_file_chars: 120_000,
            concurrency: 4,
            offline,
            paths: vec![".".to_owned()],
            include: None,
            exclude: Vec::new(),
        }
    }

    /// Runs `config` against an empty temp tree and returns the result
    /// plus the tree (so the caller can check nothing was written).
    async fn plan_with(
        config: PlanConfig,
        llm: Option<Arc<dyn LlmClient>>,
    ) -> (Result<PlanOutcome, Error>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let log = CapturingLog::new();
        let result = run_plan(
            PlanRequest {
                config,
                out: PathBuf::from("out/plan.json"),
                report_path: Some(PathBuf::from("out/report.md")),
                resume: false,
            },
            PlanDeps {
                log: &log,
                cwd: dir.path(),
                llm,
                cancel: CancellationToken::new(),
                hooks: PlanHooks::default(),
            },
        )
        .await;
        (result, dir)
    }

    fn fake() -> Option<Arc<dyn LlmClient>> {
        Some(Arc::new(FakeLlm::new(Scenario::default())))
    }

    async fn assert_usage(config: PlanConfig, llm: Option<Arc<dyn LlmClient>>, needle: &str) {
        let (result, dir) = plan_with(config, llm).await;
        match result {
            Err(Error::Usage(message)) => assert!(message.contains(needle), "{message}"),
            other => panic!("expected a usage error mentioning {needle:?}, got {other:?}"),
        }
        // Validation precedes every filesystem side effect.
        assert!(!dir.path().join("out").exists());
    }

    #[tokio::test]
    async fn an_online_request_without_a_client_is_a_usage_error() {
        assert_usage(config(false), None, "LLM client").await;
    }

    #[tokio::test]
    async fn out_of_range_numeric_fields_are_usage_errors() {
        let mut zero_concurrency = config(true);
        zero_concurrency.concurrency = 0;
        assert_usage(zero_concurrency, None, "concurrency").await;

        let mut zero_chars = config(false);
        zero_chars.max_file_chars = 0;
        assert_usage(zero_chars, fake(), "max-file-chars").await;

        for threshold in [-0.1, 1.5, f64::NAN] {
            let mut bad = config(true);
            bad.similarity_threshold = threshold;
            assert_usage(bad, None, "similarity-threshold").await;
        }
    }

    #[tokio::test]
    async fn boundary_values_are_accepted() {
        for threshold in [0.0, 1.0] {
            let mut edge = config(true);
            edge.similarity_threshold = threshold;
            edge.concurrency = 1;
            edge.max_file_chars = 1;
            let (result, _dir) = plan_with(edge, None).await;
            let outcome = result.unwrap();
            assert_eq!(outcome.exit_code, 0);
            assert!(!outcome.aborted);
        }
        // An online request with a client is valid too.
        let (result, _dir) = plan_with(config(false), fake()).await;
        assert_eq!(result.unwrap().exit_code, 0);
    }

    #[tokio::test]
    async fn output_paths_must_name_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let log = CapturingLog::new();
        let deps = || PlanDeps {
            log: &log,
            cwd: dir.path(),
            llm: None,
            cancel: CancellationToken::new(),
            hooks: PlanHooks::default(),
        };
        let request = |out: &str, report: &str| PlanRequest {
            config: config(true),
            out: PathBuf::from(out),
            report_path: Some(PathBuf::from(report)),
            resume: false,
        };
        let same = run_plan(request("a/plan.json", "a/../a/plan.json"), deps()).await;
        assert!(matches!(same, Err(Error::Usage(_))), "{same:?}");
        let no_name = run_plan(request("..", "report.md"), deps()).await;
        assert!(matches!(no_name, Err(Error::Usage(_))), "{no_name:?}");
    }

    #[test]
    fn failure_kinds_follow_the_error_variant() {
        let llm = Error::Llm {
            reason: crate::errors::LlmErrorReason::Exhausted,
            message: String::new(),
        };
        let fs = Error::Fs {
            path: PathBuf::from("/x"),
            source: std::io::Error::other("disk"),
        };
        assert_eq!(failure_kind_of(&llm), FailureKind::Llm);
        assert_eq!(failure_kind_of(&fs), FailureKind::Io);
        assert_eq!(
            failure_kind_of(&Error::Invariant(String::new())),
            FailureKind::Invariant
        );
        assert_eq!(
            failure_kind_of(&Error::Verify(String::new())),
            FailureKind::Invariant
        );
    }

    #[test]
    fn the_run_plan_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let log = CapturingLog::new();
        let cwd = PathBuf::from("/");
        let future = run_plan(
            PlanRequest {
                config: config(true),
                out: PathBuf::from("plan.json"),
                report_path: None,
                resume: false,
            },
            PlanDeps {
                log: &log,
                cwd: &cwd,
                llm: None,
                cancel: CancellationToken::new(),
                hooks: PlanHooks::default(),
            },
        );
        assert_send(&future);
    }
}
