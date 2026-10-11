//! The ground-truth e2e: `run_plan` → (hand-edit) → `run_apply` over temp
//! copies of `tests/fixtures/noisy-repo`, driven by the `FakeLlm` scenario
//! generated from the fixture's manifest, measured against that manifest.
//!
//! Every run plans a fresh copy of the tree; artifacts and reports go to a
//! separate temp directory, so neither the fixture nor the repo is ever
//! written. Records are joined to the manifest by `(path, match_key)` of
//! their original text — never by line — and every join is checked to be a
//! bijection before anything is measured.
//!
//! The retention targets are measured by [`common::retention`], overall
//! and per adapter id.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::noisy_scenario::{
    Label, Manifest, NOISY_REPO, SIMILARITY_THRESHOLD, extra_regenerations, load_manifest,
    noisy_scenario, rewrite_text, rewritten_key, scenario_key,
};
use common::retention::{Joined, assert_retention, join};
use munge_comments::artifact::types::{DecidedBy, Difficulty, FileWarningKind};
use munge_comments::comments::sanitize::match_key;
use munge_comments::engine::plan::{PlanDeps, PlanHooks, PlanOutcome, PlanRequest, run_plan};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::fake::{RegenScript, ScenarioEntry};
use munge_comments::report::markdown::RUST_DOC_OVERRIDE_MARKER;
use munge_comments::{
    ApplyDeps, ApplyHooks, ApplyOutcome, ApplyRequest, ApplyStatus, CapturingLog, CommentRecord,
    Decision, FakeLlm, FilePlan, FileRef, LanguageId, LlmClient, Mode, PlanArtifact, PlanConfig,
    Scenario, compute_summary, inventory_file, read_artifact,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// The convergence target of a second pass.
const CONVERGENCE: f64 = 0.95;

// ---------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------

/// One run's workspace: a fresh copy of the noisy repo, plus a separate
/// directory for artifacts and reports.
struct Run {
    tree: TempDir,
    work: TempDir,
    manifest: Manifest,
}

impl Run {
    fn new() -> Run {
        Run {
            tree: common::copy_fixture_tree(NOISY_REPO),
            work: common::tempdir(),
            manifest: load_manifest(),
        }
    }

    fn root(&self) -> &Path {
        self.tree.path()
    }

    /// An absolute path in the work directory.
    fn out(&self, name: &str) -> PathBuf {
        self.work.path().join(name)
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root().join(path))
            .unwrap_or_else(|err| panic!("reading {path}: {err}"))
    }

    /// Plans the tree (from its root) into `out`, with a report at
    /// `report`, under `config` and the manifest's scenario; asserts exit 0
    /// and returns the re-read, fully validated artifact.
    async fn plan(&self, config: PlanConfig, out: &str, report: Option<&str>) -> PlanArtifact {
        self.plan_with(noisy_scenario(&self.manifest), config, out, report)
            .await
    }

    /// [`Run::plan`] under `scenario`.
    async fn plan_with(
        &self,
        scenario: Scenario,
        config: PlanConfig,
        out: &str,
        report: Option<&str>,
    ) -> PlanArtifact {
        let fake = Arc::new(FakeLlm::new(scenario));
        let client: Arc<dyn LlmClient> = fake;
        let log = CapturingLog::new();
        let out = self.out(out);
        let outcome: PlanOutcome = run_plan(
            PlanRequest {
                config,
                out: out.clone(),
                report_path: report.map(|name| self.out(name)),
                resume: false,
            },
            PlanDeps {
                log: &log,
                cwd: self.root(),
                llm: Some(client),
                cancel: CancellationToken::new(),
                hooks: PlanHooks::default(),
            },
        )
        .await
        .unwrap_or_else(|err| panic!("plan failed: {err}\n{:#?}", log.lines()));
        assert_eq!(outcome.exit_code, 0, "{:#?}", log.lines());
        let artifact = read_artifact(&out)
            .unwrap_or_else(|err| panic!("{} must validate: {err}", out.display()))
            .into_inner();
        assert_eq!(
            artifact, outcome.artifact,
            "the artifact on disk is the returned one"
        );
        assert_eq!(
            artifact.summary,
            compute_summary(&artifact.files, &artifact.config).unwrap()
        );
        artifact
    }

    /// Writes `artifact` (hand-edited) back to `out`, pretty-printed.
    fn write_artifact(&self, artifact: &PlanArtifact, out: &str) {
        let mut json = serde_json::to_string_pretty(artifact).unwrap();
        json.push('\n');
        fs::write(self.out(out), json).unwrap();
    }

    /// Applies the artifact `out` (from the tree root), with an apply
    /// report at `report`; asserts every file verified clean (exit 0, no
    /// file rejected).
    fn apply(&self, out: &str, report: Option<&str>) -> ApplyOutcome {
        let log = CapturingLog::new();
        let mut stdout = Vec::new();
        let applied = munge_comments::run_apply(
            ApplyRequest {
                artifact: self.out(out),
                only: Vec::new(),
                dry_run: false,
                report_path: report.map(|name| self.out(name)),
            },
            ApplyDeps {
                log: &log,
                stdout: &mut stdout,
                cwd: self.root(),
                hooks: ApplyHooks::default(),
            },
        )
        .unwrap_or_else(|err| panic!("apply failed: {err}\n{:#?}", log.lines()));
        assert_eq!(applied.exit_code, 0, "{:#?}", log.lines());
        for file in &applied.result.files {
            assert!(
                matches!(file.status, ApplyStatus::Applied | ApplyStatus::Unchanged),
                "{}: {:?}\n{:#?}",
                file.path,
                file.status,
                log.lines()
            );
        }
        applied
    }
}

/// A config as the CLI would resolve it with default flags, except `mode`.
fn config(mode: Mode, allow_add: bool) -> PlanConfig {
    PlanConfig {
        mode,
        model: "gpt-4.1".to_owned(),
        base_url: None,
        allow_add,
        similarity_threshold: SIMILARITY_THRESHOLD,
        max_file_chars: 120_000,
        concurrency: 4,
        offline: false,
        paths: vec![".".to_owned()],
        include: None,
        exclude: Vec::new(),
    }
}

/// Every scanned comment of the applied file `path` (re-inventoried from
/// disk), as `match_key`s.
fn applied_keys(run: &Run, path: &str, language: LanguageId) -> Vec<String> {
    let text = run.read(path);
    let inventory = inventory_file(
        &text,
        FileRef {
            language,
            file_name: path.to_owned(),
        },
    )
    .unwrap_or_else(|err| panic!("re-inventorying {path}: {err}"));
    let adapter = adapter_for(language);
    inventory
        .comments
        .iter()
        .map(|comment| match_key(&comment.text, adapter))
        .collect()
}

/// The artifact's file at `path`.
fn file<'a>(artifact: &'a PlanArtifact, path: &str) -> &'a FilePlan {
    artifact
        .files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the artifact"))
}

/// Every file of the fixture tree (manifest excluded), with its bytes.
fn fixture_snapshot() -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&common::fixture_path(NOISY_REPO), &mut out);
    out
}

// ---------------------------------------------------------------------
// Redact mode: retention, rewrites, idempotency.
// ---------------------------------------------------------------------

#[tokio::test]
async fn redact_mode_hits_every_target_rewrites_and_converges() {
    let before = fixture_snapshot();
    let run = Run::new();
    let manifest = &run.manifest;
    let pass1 = run
        .plan(config(Mode::Redact, false), "pass1.json", Some("pass1.md"))
        .await;
    for file in &pass1.files {
        assert_eq!(file.effective_mode, Mode::Redact, "{}", file.path);
    }
    let planned: Vec<LanguageId> = pass1.summary.languages.keys().copied().collect();
    assert_eq!(planned, LanguageId::ALL, "every adapter id is planned");
    assert_retention(manifest, &pass1, "redact pass 1");
    run.apply("pass1.json", Some("apply1.md"));

    // Every rewrite is planned as its scripted text and written as such.
    for entry in manifest.with_label(Label::Rewrite) {
        let record = join(manifest, &pass1)
            .into_iter()
            .find(|j| manifest.entries[j.entry] == *entry)
            .unwrap()
            .record;
        assert_eq!(record.decision, Decision::KeepGenerated, "{record:#?}");
        assert_eq!(record.final_comment.as_deref(), Some(rewrite_text(entry)));
        let keys = applied_keys(&run, &entry.path, entry.language());
        assert!(
            keys.contains(&rewritten_key(entry)),
            "{}: the rewrite is written: {keys:#?}",
            entry.scenario_key()
        );
        assert!(
            !keys.contains(&entry.key),
            "{}: the original is gone",
            entry.scenario_key()
        );
    }

    // Pass 2: plan only, over the applied tree, to a different `out`.
    let pass2 = run
        .plan(config(Mode::Redact, false), "pass2.json", None)
        .await;
    let convergence = munge_comments::convergence_pct(&pass2);
    assert!(convergence >= CONVERGENCE, "convergence {convergence}");
    for entry in manifest.with_label(Label::Rewrite) {
        let key = rewritten_key(entry);
        let adapter = adapter_for(entry.language());
        let record = file(&pass2, &entry.path)
            .comments
            .iter()
            .find(|record| {
                record
                    .original
                    .as_deref()
                    .is_some_and(|original| match_key(original, adapter) == key)
            })
            .unwrap_or_else(|| panic!("{}: no pass-2 record", entry.scenario_key()));
        assert_eq!(record.decision, Decision::KeepOriginal, "{record:#?}");
        assert_eq!(record.decided_by, DecidedBy::Engine, "{record:#?}");
        let rationale = record.decision_rationale.as_deref().unwrap_or_default();
        assert!(rationale.starts_with("rail: tie:"), "{rationale}");
    }

    assert_eq!(fixture_snapshot(), before, "the fixture is never mutated");
}

// ---------------------------------------------------------------------
// Hand edits: overrides are honored and verified.
// ---------------------------------------------------------------------

/// The sections every report carries, in order.
const REPORT_SECTIONS: [&str; 8] = [
    "## Summary",
    "## Needs human review",
    "## Flagged",
    "## Protected policy items",
    "## New comment proposals",
    "## Files not processed",
    "## Mode fallbacks",
    "## Warnings",
];

/// Asserts the report at `path` exists and carries [`REPORT_SECTIONS`] in
/// order (plus the apply outcome when `applied`); returns its text.
fn assert_report(path: &Path, applied: bool) -> String {
    let report = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("the report {} exists: {err}", path.display()));
    let mut from = 0;
    let outcome = applied.then_some("## Apply outcome");
    for section in REPORT_SECTIONS.iter().copied().chain(outcome) {
        let at = report[from..]
            .find(&format!("\n{section}\n"))
            .unwrap_or_else(|| panic!("{}: no {section:?} after byte {from}", path.display()));
        from += at + 1;
    }
    if !applied {
        assert!(!report.contains("## Apply outcome"), "{}", path.display());
    }
    report
}

/// The record joined to the first manifest entry satisfying `pick`.
fn pick(
    manifest: &Manifest,
    artifact: &PlanArtifact,
    pick: impl Fn(&Joined) -> bool,
) -> (String, String) {
    let joined = join(manifest, artifact);
    let chosen = joined
        .iter()
        .find(|j| pick(j))
        .expect("the fixture has a record to hand-edit");
    (chosen.path.to_owned(), chosen.record.id.clone())
}

/// Flips the record `id` of `path` to `REMOVE`, as a hand-editor would.
fn flip_to_remove(artifact: &mut PlanArtifact, (path, id): &(String, String)) {
    let record = artifact
        .files
        .iter_mut()
        .find(|file| &file.path == path)
        .and_then(|file| file.comments.iter_mut().find(|record| &record.id == id))
        .unwrap();
    record.decision = Decision::Remove;
    record.decided_by = DecidedBy::Human;
}

/// The original text of record `id` of `path`.
fn original_of(artifact: &PlanArtifact, (path, id): &(String, String)) -> String {
    file(artifact, path)
        .comments
        .iter()
        .find(|record| &record.id == id)
        .and_then(|record| record.original.clone())
        .unwrap()
}

#[tokio::test]
async fn hand_edited_overrides_are_honored_and_verify_clean() {
    let run = Run::new();
    let manifest = &run.manifest;
    let mut artifact = run
        .plan(config(Mode::Redact, false), "plan.json", Some("plan.md"))
        .await;
    let plan_report = assert_report(&run.out("plan.md"), false);
    assert!(plan_report.contains("Protected policy items"));

    // A valuable comment the judge kept; an exported-symbol JSDoc (JS
    // doc-protected: tooling-facing only); a Rust `///` doc comment.
    let valuable = pick(manifest, &artifact, |j| {
        j.label == Label::Valuable
            && j.record.decision == Decision::KeepOriginal
            && j.record.decided_by == DecidedBy::Judge
    });
    let jsdoc = pick(manifest, &artifact, |j| {
        j.label == Label::Protected && j.language == LanguageId::Javascript
    });
    let rust_doc = pick(manifest, &artifact, |j| {
        j.label == Label::Protected
            && j.language == LanguageId::Rust
            && j.record
                .original
                .as_deref()
                .is_some_and(|text| text.starts_with("/// "))
    });
    let originals: Vec<(String, String)> = [&valuable, &jsdoc, &rust_doc]
        .into_iter()
        .map(|target| (target.0.clone(), original_of(&artifact, target)))
        .collect();
    for target in [&valuable, &jsdoc, &rust_doc] {
        flip_to_remove(&mut artifact, target);
    }
    // The two doc-protected flips are the run's overrides; the prose flip
    // is an ordinary decision.
    let summary = compute_summary(&artifact.files, &artifact.config).unwrap();
    assert_eq!(summary.overrides, 2);
    run.write_artifact(&artifact, "plan.json");

    let applied = run.apply("plan.json", Some("apply.md"));
    for (path, original) in &originals {
        let status = applied
            .result
            .files
            .iter()
            .find(|file| &file.path == path)
            .map(|file| file.status);
        assert_eq!(status, Some(ApplyStatus::Applied), "{path}");
        let language = file(&artifact, path).language;
        let key = match_key(original, adapter_for(language));
        assert!(
            !applied_keys(&run, path, language).contains(&key),
            "{path}: the override removed {original:?}"
        );
    }

    let apply_report = assert_report(&run.out("apply.md"), true);
    let protected = section(&apply_report, "## Protected policy items");
    let item = |(path, id): &(String, String)| {
        protected
            .lines()
            .find(|line| line.starts_with(&format!("- `{path}` {id} ")))
            .unwrap_or_else(|| panic!("{path} {id} is not listed:\n{protected}"))
            .to_owned()
    };
    let js_item = item(&jsdoc);
    assert!(
        js_item.contains("**override** → REMOVE (human)"),
        "{js_item}"
    );
    assert!(!js_item.contains(RUST_DOC_OVERRIDE_MARKER), "{js_item}");
    let rust_item = item(&rust_doc);
    assert!(
        rust_item.contains("**override** → REMOVE (human)"),
        "{rust_item}"
    );
    assert!(rust_item.contains(RUST_DOC_OVERRIDE_MARKER), "{rust_item}");
}

/// The body of the report section headed `heading` (up to the next `## `).
fn section<'a>(report: &'a str, heading: &str) -> &'a str {
    let start = report
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("no {heading:?}"))
        + heading.len()
        + 2;
    let end = report[start..]
        .find("\n## ")
        .map_or(report.len(), |at| start + at);
    &report[start..end]
}

// ---------------------------------------------------------------------
// Remove mode, with and without `--allow-add`.
// ---------------------------------------------------------------------

/// The NEW-origin record of `artifact` proposing `comment`.
fn new_record<'a>(artifact: &'a PlanArtifact, path: &str, comment: &str) -> &'a CommentRecord {
    file(artifact, path)
        .comments
        .iter()
        .find(|record| record.is_new_origin() && record.final_comment.as_deref() == Some(comment))
        .unwrap_or_else(|| panic!("{path}: no NEW-origin record proposes {comment:?}"))
}

/// Plans and applies remove mode under `allow_add`: the retention targets
/// hold, every file applies verifier-clean, and each scripted extra
/// regeneration is a NEW-origin record — inserted only when `allow_add`
/// and not DIFFICULT (a DIFFICULT one is held as `NEEDS_REVIEW`).
async fn remove_mode(allow_add: bool) {
    let run = Run::new();
    let artifact = run
        .plan(
            config(Mode::Remove, allow_add),
            "plan.json",
            Some("plan.md"),
        )
        .await;
    for file in &artifact.files {
        assert_eq!(file.effective_mode, Mode::Remove, "{}", file.path);
    }
    assert_retention(&run.manifest, &artifact, "remove mode");
    let extras = extra_regenerations();
    let new_origin = artifact
        .files
        .iter()
        .flat_map(|file| &file.comments)
        .filter(|record| record.is_new_origin())
        .count();
    assert_eq!(new_origin, extras.len(), "only the scripted extras are NEW");
    for extra in &extras {
        let record = new_record(&artifact, &extra.path, &extra.comment);
        assert_eq!(record.difficulty, Some(extra.difficulty), "{record:#?}");
        let expected = if extra.difficulty == Difficulty::Difficult {
            Decision::NeedsReview
        } else {
            Decision::New
        };
        assert_eq!(record.decision, expected, "{record:#?}");
    }

    let report = assert_report(&run.out("plan.md"), false);
    let proposals = section(&report, "## New comment proposals");
    let intro = if allow_add {
        "allowAdd is on: apply inserts records decided NEW."
    } else {
        "allowAdd is off: every proposal is report-only."
    };
    assert!(proposals.contains(intro), "{proposals}");

    run.apply("plan.json", None);
    for extra in &extras {
        let language = file(&artifact, &extra.path).language;
        let key = match_key(&extra.comment, adapter_for(language));
        let inserted = applied_keys(&run, &extra.path, language).contains(&key);
        let expected = allow_add && extra.difficulty != Difficulty::Difficult;
        assert_eq!(
            inserted, expected,
            "{}: {:?} inserted = {inserted} (allowAdd {allow_add}, {:?})",
            extra.path, extra.comment, extra.difficulty
        );
    }
}

#[tokio::test]
async fn remove_mode_hits_the_targets_and_keeps_new_comments_report_only() {
    remove_mode(false).await;
}

#[tokio::test]
async fn remove_mode_with_allow_add_inserts_new_comments_but_holds_the_difficult_one() {
    remove_mode(true).await;
}

// ---------------------------------------------------------------------
// Placeholder collision: a per-file remove-mode fallback.
// ---------------------------------------------------------------------

/// Where the collision fixture is added to the copy.
const COLLISION: &str = "collision.ts";

#[tokio::test]
async fn a_placeholder_collision_falls_back_to_remove_mode_and_applies_cleanly() {
    let run = Run::new();
    fs::copy(
        common::fixture_path("collision/collision.ts"),
        run.root().join(COLLISION),
    )
    .unwrap();
    // The collision file is not in the manifest: script its restatement
    // as superfluous, so the fallback has a remove-mode edit to make.
    let restatement = "Wrap the marker in brackets for display.";
    let mut scenario = noisy_scenario(&run.manifest);
    scenario.by_key.insert(
        scenario_key(COLLISION, restatement),
        ScenarioEntry {
            regen: Some(RegenScript {
                comment: restatement.to_owned(),
                difficulty: Difficulty::Easy,
                would_you_comment_here: true,
                rationale: "The code says exactly this.".to_owned(),
                missing_context: None,
            }),
            judge: None,
        },
    );
    let mut artifact = run
        .plan_with(
            scenario,
            config(Mode::Redact, false),
            "plan.json",
            Some("plan.md"),
        )
        .await;

    let collision = file(&artifact, COLLISION);
    assert_eq!(collision.effective_mode, Mode::Remove);
    let warnings: Vec<_> = collision
        .warnings
        .iter()
        .filter(|warning| warning.kind == FileWarningKind::PlaceholderCollision)
        .collect();
    assert_eq!(warnings.len(), 1, "{:#?}", collision.warnings);
    assert!(!collision.comments.is_empty());
    for record in &collision.comments {
        assert!(record.regenerated.is_some(), "{record:#?}");
        assert_eq!(record.would_you_comment_here, None, "{record:#?}");
    }
    let removed = collision
        .comments
        .iter()
        .find(|record| record.original.as_deref() == Some(&format!("// {restatement}")))
        .expect("the restatement is planned");
    assert_eq!(removed.decision, Decision::Remove, "{removed:#?}");
    let report = assert_report(&run.out("plan.md"), false);
    let fallbacks = section(&report, "## Mode fallbacks");
    assert!(fallbacks.contains(COLLISION), "{fallbacks}");

    // Every other file keeps redact mode and its targets.
    artifact.files.retain(|file| file.path != COLLISION);
    for file in &artifact.files {
        assert_eq!(file.effective_mode, Mode::Redact, "{}", file.path);
    }
    assert_retention(&run.manifest, &artifact, "redact mode beside a collision");

    let applied = run.apply("plan.json", None);
    let status = applied
        .result
        .files
        .iter()
        .find(|file| file.path == COLLISION)
        .map(|file| file.status);
    assert_eq!(status, Some(ApplyStatus::Applied));
    let written = run.read(COLLISION);
    assert!(!written.contains(restatement), "{written}");
    assert!(written.contains("const marker = \"⟦C1⟧\";"), "{written}");
}
