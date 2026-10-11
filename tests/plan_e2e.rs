//! End-to-end `plan`: `run_plan` driven with `FakeLlm` (or offline) over
//! temp copies of fixture trees, with temp `out`/`report` paths. Every
//! artifact written is re-read through `read_artifact` (full validation)
//! and its summary checked against the records.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use munge_comments::apply::stage::has_actionable_edit;
use munge_comments::apply::types::ApplyStatus;
use munge_comments::artifact::types::{
    CommentRecord, DecidedBy, FailureKind, FileStatus, FileWarning, FileWarningKind, SkipReason,
};
use munge_comments::comments::sanitize::is_content_empty;
use munge_comments::comments::types::Classification;
use munge_comments::engine::heuristics::FLAGGED_FOR_REVIEW;
use munge_comments::engine::plan::{
    NOT_ATTEMPTED_AUTH, OFFLINE_TAG, PlanDeps, PlanHooks, PlanOutcome, PlanRequest, run_plan,
};
use munge_comments::engine::verify_rail::VERIFY_RAIL_TAG;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::client::LlmMeta;
use munge_comments::llm::fake::{ANY_PATH, FailureInjection, InjectedFailure};
use munge_comments::{
    ApplyDeps, ApplyHooks, ApplyOutcome, ApplyRequest, CapturingLog, Decision, Error, FakeLlm,
    FilePlan, FileRef, FunctionalComment, LanguageId, LlmClient, Log, Mode, PlanArtifact,
    PlanConfig, Scenario, compute_summary, inventory_file, language_for, read_artifact,
    verify_semantic_equality,
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// The mixed-language fixture tree (Go, JavaScript, Python, Rust,
/// TypeScript).
const TREE: &str = "apply-e2e";

/// Every file of [`TREE`], in artifact (path) order, with its language.
const FILES: [(&str, LanguageId); 5] = [
    ("go/count.go", LanguageId::Go),
    ("js/asi.js", LanguageId::Javascript),
    ("py/calc.py", LanguageId::Python),
    ("rs/lib.rs", LanguageId::Rust),
    ("ts/greet.ts", LanguageId::Typescript),
];

/// A config as the CLI would resolve it with default flags.
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

fn request(config: PlanConfig, out: impl Into<PathBuf>, report: Option<&str>) -> PlanRequest {
    PlanRequest {
        config,
        out: out.into(),
        report_path: report.map(PathBuf::from),
        resume: false,
    }
}

/// A shareable fake, plus the trait-object handle `run_plan` takes.
fn fake(scenario: Scenario) -> (Arc<FakeLlm>, Option<Arc<dyn LlmClient>>) {
    let fake = Arc::new(FakeLlm::new(scenario));
    let client: Arc<dyn LlmClient> = fake.clone();
    (fake, Some(client))
}

/// Runs `run_plan` from `cwd`, returning its result and the captured log.
async fn plan(
    cwd: &Path,
    req: PlanRequest,
    llm: Option<Arc<dyn LlmClient>>,
    hooks: PlanHooks,
) -> (Result<PlanOutcome, Error>, Vec<String>) {
    let log = CapturingLog::new();
    let result = run_plan(
        req,
        PlanDeps {
            log: &log,
            cwd,
            llm,
            cancel: CancellationToken::new(),
            hooks,
        },
    )
    .await;
    (result, log.lines())
}

/// Re-reads the artifact at absolute `path` through full validation and
/// checks it matches the returned one and that its summary is internally
/// consistent.
fn check_artifact(path: &Path, returned: &PlanArtifact) -> PlanArtifact {
    let validated =
        read_artifact(path).unwrap_or_else(|err| panic!("{} must validate: {err}", path.display()));
    let artifact: PlanArtifact = validated.into_inner();
    assert_eq!(
        &artifact, returned,
        "the returned artifact is the one on disk"
    );
    let summary = &artifact.summary;
    assert_eq!(
        summary,
        &compute_summary(&artifact.files, &artifact.config).unwrap(),
        "the stored summary is the recomputed one"
    );
    assert_eq!(summary.files_considered, artifact.files.len() as u64);
    assert_eq!(
        summary.removed
            + summary.rewritten
            + summary.kept
            + summary.needs_review
            + summary.protected,
        summary.comments_total
    );
    assert_eq!(
        summary.removed + summary.rewritten + summary.kept + summary.needs_review,
        summary.prose_total
    );
    for file in &artifact.files {
        assert_eq!(Some(file.language), language_for(Path::new(&file.path)));
        let mut sorted = file.comments.clone();
        sorted.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
        assert_eq!(sorted, file.comments, "{}: records sorted", file.path);
    }
    artifact
}

fn file<'a>(artifact: &'a PlanArtifact, path: &str) -> &'a FilePlan {
    artifact
        .files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the artifact"))
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[tokio::test]
async fn a_mixed_language_tree_plans_end_to_end() {
    let tree = common::copy_fixture_tree(TREE);
    let (fake, llm) = fake(Scenario::default());
    let req = request(config(false), "plan.json", Some("plan-report.md"));
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert!(!outcome.aborted);
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);

    assert_eq!(artifact.root, ".");
    assert_eq!(artifact.glob_base, ".");
    assert_eq!(artifact.config.paths, ["."]);
    let planned: Vec<(&str, LanguageId)> = artifact
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.language))
        .collect();
    assert_eq!(planned, FILES);
    for file in &artifact.files {
        assert_eq!(file.status, FileStatus::Complete, "{}", file.path);
        assert!(file.content_hash.is_some());
        assert!(!file.comments.is_empty(), "{}", file.path);
    }
    assert!(artifact.summary.languages.len() >= 3);
    assert!(!fake.calls().is_empty(), "online planning calls the model");
    assert!(tree.path().join("plan-report.md").is_file());
    assert!(
        log.iter().any(|line| line.contains("convergence")),
        "{log:#?}"
    );
    assert!(log.iter().any(|line| line.contains("[5/5]")), "{log:#?}");
}

#[tokio::test]
async fn an_llm_failure_fails_only_its_file_with_exit_5() {
    let tree = common::copy_fixture_tree(TREE);
    let scenario = Scenario {
        failures: vec![FailureInjection {
            path: "py/calc.py".to_owned(),
            call: None,
            error: InjectedFailure::Llm,
            times: None,
        }],
        ..Scenario::default()
    };
    let (_fake, llm) = fake(scenario);
    let req = request(config(false), "plan.json", None);
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 5, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    let failed = file(&artifact, "py/calc.py");
    assert_eq!(failed.status, FileStatus::Failed);
    assert_eq!(failed.failure_kind, Some(FailureKind::Llm));
    assert!(failed.content_hash.is_some(), "the file was read");
    assert!(failed.comments.is_empty());
    for (path, _) in FILES.iter().filter(|(path, _)| *path != "py/calc.py") {
        assert_eq!(file(&artifact, path).status, FileStatus::Complete, "{path}");
    }
    assert_eq!(artifact.summary.files_failed, 1);
    assert!(
        log.iter()
            .any(|line| line.contains("py/calc.py") && line.contains("llm")),
        "{log:#?}"
    );
}

#[tokio::test]
async fn unsupported_inputs_are_skipped_without_affecting_the_exit_code() {
    let tree = common::tempdir();
    let root = tree.path();
    fs::write(root.join("bytes.ts"), b"// caf\xe9\nexport const x = 1;\n").unwrap();
    fs::write(
        root.join("latin.py"),
        "# -*- coding: latin-1 -*-\n# plain ascii\nx = 1\n",
    )
    .unwrap();
    fs::copy(
        common::fixture_path("syntax-errors/go-multiline-comment-newline.go"),
        root.join("multi.go"),
    )
    .unwrap();
    let deep = format!("// deep\nx = {}1{};\n", "(".repeat(2000), ")".repeat(2000));
    fs::write(root.join("deep.js"), deep).unwrap();
    fs::write(
        root.join("ok.py"),
        "# -*- coding: utf-8 -*-\n# fine\nx = 1\n",
    )
    .unwrap();

    let (fake, llm) = fake(Scenario::default());
    let req = request(config(false), "plan.json", Some("report.md"));
    let (result, log) = plan(root, req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&root.join("plan.json"), &outcome.artifact);
    for (path, reason) in [
        ("bytes.ts", SkipReason::NonUtf8),
        ("latin.py", SkipReason::DeclaredEncoding),
        ("multi.go", SkipReason::SyntaxErrors),
        ("deep.js", SkipReason::TooDeep),
    ] {
        let skipped = file(&artifact, path);
        assert_eq!(skipped.status, FileStatus::Skipped, "{path}");
        assert_eq!(skipped.skip_reason, Some(reason), "{path}");
        assert!(skipped.content_hash.is_some(), "{path}");
        assert!(skipped.error.is_some(), "{path}");
        assert!(skipped.comments.is_empty(), "{path}");
        // The final summary lists every skipped file with its reason, spelled as the serialized enum name with the JSON quotes stripped.
        let listed = format!(
            "plan: skipped {path} ({})",
            serde_json::to_value(reason).unwrap()
        );
        let listed = listed.replace('"', "");
        assert!(
            log.iter().any(|line| line.starts_with(&listed)),
            "{listed}: {log:#?}"
        );
    }
    assert_eq!(file(&artifact, "ok.py").status, FileStatus::Complete);
    assert_eq!(artifact.summary.files_skipped, 4);
    assert!(fake.calls().iter().all(|call| call.path == "ok.py"));
}

/// A scenario that removes calc.py's "add the two numbers" (row 4: EASY,
/// and the model would not comment there).
fn removing_scenario() -> Scenario {
    serde_json::from_value(json!({
        "byKey": {
            "add the two numbers": {
                "regen": { "comment": "", "difficulty": "EASY", "wouldYouCommentHere": false }
            }
        }
    }))
    .unwrap()
}

/// Plans [`TREE`] from its own directory into the artifact path `out`
/// computes (somewhere under `outside`), then applies the artifact from
/// `outside` as cwd: the stored root must lead back to the tree.
async fn plan_outside_then_apply(out: impl Fn(&TempDir, &TempDir) -> PathBuf) {
    let tree = common::copy_fixture_tree(TREE);
    let outside = common::tempdir();
    let out = out(&tree, &outside);
    let (_fake, llm) = fake(removing_scenario());
    let req = request(config(false), out.clone(), None);
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");

    let artifact_path = canonical(&outside.path().join("plans")).join("plan.json");
    let artifact = check_artifact(&artifact_path, &outcome.artifact);
    assert!(artifact.root.starts_with("../"), "{}", artifact.root);
    assert_eq!(
        canonical(&artifact_path.parent().unwrap().join(&artifact.root)),
        canonical(tree.path())
    );
    assert_eq!(artifact.glob_base, ".");
    assert_eq!(artifact.config.paths, ["."]);
    let calc = file(&artifact, "py/calc.py");
    assert!(
        calc.comments
            .iter()
            .any(|record| record.decision == Decision::Remove)
    );

    let log = CapturingLog::new();
    let mut stdout = Vec::new();
    let applied = munge_comments::run_apply(
        ApplyRequest {
            artifact: artifact_path,
            only: Vec::new(),
            dry_run: false,
            report_path: None,
        },
        ApplyDeps {
            log: &log,
            stdout: &mut stdout,
            cwd: outside.path(),
            hooks: ApplyHooks::default(),
        },
    )
    .unwrap();
    assert_eq!(applied.exit_code, 0, "{:#?}", log.lines());
    let status = |path: &str| {
        applied
            .result
            .files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.status)
    };
    assert_eq!(status("py/calc.py"), Some(ApplyStatus::Applied));
    let written = fs::read_to_string(tree.path().join("py/calc.py")).unwrap();
    assert!(!written.contains("add the two numbers"), "{written}");
}

#[tokio::test]
async fn an_out_outside_the_tree_gets_a_dotdot_root_that_applies() {
    plan_outside_then_apply(|_tree, outside| {
        Path::new("..")
            .join(outside.path().file_name().unwrap())
            .join("plans")
            .join("plan.json")
    })
    .await;
}

#[tokio::test]
async fn an_absolute_non_canonical_out_gets_a_dotdot_root_that_applies() {
    plan_outside_then_apply(|_tree, outside| {
        // On macOS temp dirs live under `/var`, a symlink to `/private/var`:
        // the spelling given is deliberately not the canonical one.
        #[cfg(target_os = "macos")]
        {
            assert!(outside.path().starts_with("/var/"));
            assert_ne!(outside.path(), canonical(outside.path()));
        }
        outside.path().join("plans").join("plan.json")
    })
    .await;
}

#[tokio::test]
async fn output_directories_are_created() {
    let tree = common::copy_fixture_tree(TREE);
    let req = request(
        config(true),
        "new/deeper/plan.json",
        Some("reports/today/report.md"),
    );
    let (result, log) = plan(tree.path(), req, None, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let out = tree.path().join("new/deeper/plan.json");
    let artifact = check_artifact(&out, &outcome.artifact);
    assert_eq!(artifact.root, "../..");
    assert_eq!(artifact.glob_base, ".");
    assert!(tree.path().join("reports/today/report.md").is_file());
}

#[tokio::test]
async fn an_inventory_invariant_fails_only_its_file_with_exit_7() {
    let tree = common::copy_fixture_tree(TREE);
    let hooks = PlanHooks {
        inventory: Some(Arc::new(|text: &str, file: munge_comments::FileRef| {
            if file.file_name == "go/count.go" {
                Err(Error::Invariant("injected inventory failure".to_owned()))
            } else {
                inventory_file(text, file)
            }
        })),
        ..PlanHooks::default()
    };
    let (_fake, llm) = fake(Scenario::default());
    let req = request(config(false), "plan.json", Some("report.md"));
    let (result, log) = plan(tree.path(), req, llm, hooks).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 7, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    let failed = file(&artifact, "go/count.go");
    assert_eq!(failed.status, FileStatus::Failed);
    assert_eq!(failed.failure_kind, Some(FailureKind::Invariant));
    assert!(failed.content_hash.is_some());
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("injected inventory failure")
    );
    for (path, _) in FILES.iter().filter(|(path, _)| *path != "go/count.go") {
        assert_eq!(file(&artifact, path).status, FileStatus::Complete, "{path}");
    }
    assert!(tree.path().join("report.md").is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_file_fails_with_io_and_no_hash() {
    use std::os::unix::fs::PermissionsExt;

    let tree = common::copy_fixture_tree(TREE);
    let locked = tree.path().join("py/calc.py");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&locked).is_ok() {
        // Running as root (or on a filesystem that ignores mode bits) defeats the lock, so skip instead of asserting a failure that cannot be produced.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }
    let (_fake, llm) = fake(Scenario::default());
    let req = request(config(false), "plan.json", Some("report.md"));
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 6, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    let failed = file(&artifact, "py/calc.py");
    assert_eq!(failed.status, FileStatus::Failed);
    assert_eq!(failed.failure_kind, Some(FailureKind::Io));
    assert_eq!(failed.content_hash, None);
    assert_eq!(failed.byte_length, 0);
    for (path, _) in FILES.iter().filter(|(path, _)| *path != "py/calc.py") {
        assert_eq!(file(&artifact, path).status, FileStatus::Complete, "{path}");
    }
}

#[tokio::test]
async fn an_existing_out_is_refused_without_resume() {
    let tree = common::copy_fixture_tree(TREE);
    let out = tree.path().join("plan.json");
    fs::write(&out, "precious paid work\n").unwrap();
    let (fake, llm) = fake(Scenario::default());
    let req = request(config(false), "plan.json", Some("report.md"));
    let (result, _log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert_eq!(err.exit_code(), 1);
    assert_eq!(fs::read_to_string(&out).unwrap(), "precious paid work\n");
    assert!(!tree.path().join("report.md").exists());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn an_empty_discovery_writes_a_valid_empty_artifact() {
    let tree = common::tempdir();
    fs::write(tree.path().join("notes.txt"), "not source\n").unwrap();
    let (fake, llm) = fake(Scenario::default());
    let req = request(config(false), "plan.json", Some("report.md"));
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert!(artifact.files.is_empty());
    assert_eq!(artifact.root, ".");
    assert_eq!(artifact.summary.files_considered, 0);
    assert!(tree.path().join("report.md").is_file());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn offline_planning_needs_no_client_and_makes_no_calls() {
    let tree = common::copy_fixture_tree(TREE);
    let req = request(config(true), "plan.json", Some("report.md"));
    let (result, log) = plan(tree.path(), req, None, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert_eq!(artifact.files.len(), FILES.len());
    let mut offline_kept = 0;
    for file in &artifact.files {
        assert_eq!(file.status, FileStatus::Complete, "{}", file.path);
        assert_eq!(file.effective_mode, Mode::Redact);
        for record in &file.comments {
            assert_eq!(record.regenerated, None);
            assert_eq!(record.difficulty, None);
            assert_eq!(record.confidence, 1.0);
            if record.tags.iter().any(|tag| tag == OFFLINE_TAG) {
                offline_kept += 1;
                assert_eq!(record.decision, Decision::KeepOriginal);
                assert_eq!(record.decided_by, DecidedBy::Heuristic);
            }
        }
    }
    assert!(offline_kept > 0);

    // A supplied client is not called either.
    let tree = common::copy_fixture_tree(TREE);
    let (fake, llm) = fake(Scenario::default());
    let req = request(config(true), "plan.json", None);
    let (result, _log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let with_client = result.unwrap();
    assert_eq!(with_client.exit_code, 0);
    assert!(fake.calls().is_empty());
    assert_eq!(with_client.artifact.files, artifact.files);
}

/// Checks every decision of an offline artifact is one offline mode can
/// make: `KEEP_PROTECTED` (non-prose), a heuristic `KEEP_ORIGINAL` (row
/// 1b, row 2 or the offline default), a heuristic content-empty `REMOVE`
/// (row 3), or a `verify-rail` demotion of one of those to
/// `KEEP_ORIGINAL` by the engine. No model field is ever set. Returns the
/// number of `(protected, offline-kept, removed, demoted)` records.
fn check_offline_decisions(artifact: &PlanArtifact) -> [usize; 4] {
    let mut counts = [0; 4];
    for file in &artifact.files {
        let adapter = adapter_for(file.language);
        let warned = file
            .warnings
            .iter()
            .filter(|warning| warning.kind == FileWarningKind::VerifyDemoted)
            .count();
        let mut demoted = 0;
        for record in &file.comments {
            let at = format!("{} {}", file.path, record.id);
            let original = record
                .original
                .as_deref()
                .unwrap_or_else(|| panic!("{at}: offline mode never creates a NEW-origin record"));
            assert_eq!(record.regenerated, None, "{at}");
            assert_eq!(record.would_you_comment_here, None, "{at}");
            assert_eq!(record.difficulty, None, "{at}");
            assert_eq!(record.missing_context, None, "{at}");
            assert_eq!(record.confidence, 1.0, "{at}");
            let has = |tag: &str| record.tags.iter().any(|t| t == tag);
            let prose = record.classification == Classification::Prose;
            match (record.decision, record.decided_by) {
                (Decision::KeepProtected, DecidedBy::Protected) => {
                    assert!(!prose, "{at}: only non-prose is protected");
                    counts[0] += 1;
                }
                (Decision::KeepOriginal, DecidedBy::Heuristic) => {
                    assert!(prose, "{at}");
                    assert!(
                        has(OFFLINE_TAG) || has(FLAGGED_FOR_REVIEW) || has("jsx-expression"),
                        "{at}: an untagged heuristic keep: {:?}",
                        record.tags
                    );
                    counts[1] += usize::from(has(OFFLINE_TAG));
                }
                (Decision::Remove, DecidedBy::Heuristic) => {
                    assert!(prose, "{at}");
                    assert!(
                        is_content_empty(original, adapter),
                        "{at}: only content-empty comments are removed offline"
                    );
                    counts[2] += 1;
                }
                (Decision::KeepOriginal, DecidedBy::Engine) => {
                    assert!(has(VERIFY_RAIL_TAG), "{at}: {:?}", record.tags);
                    let rationale = record.decision_rationale.as_deref().unwrap_or_default();
                    assert!(
                        rationale.contains("previously decided REMOVE by heuristic"),
                        "{at}: only a content-empty REMOVE has an edit to demote: {rationale}"
                    );
                    assert!(is_content_empty(original, adapter), "{at}");
                    demoted += 1;
                }
                other => panic!("{at}: offline mode cannot decide {other:?}"),
            }
        }
        assert_eq!(warned, demoted, "{}: one warning per demotion", file.path);
        counts[3] += demoted;
    }
    counts
}

#[tokio::test]
async fn an_offline_plan_of_the_golden_tree_applies_cleanly_in_every_tier_1_language() {
    let tree = common::copy_fixture_tree("golden");
    let outputs = common::tempdir();
    let out = outputs.path().join("plan.json");
    let report = outputs.path().join("plan-report.md");
    let req = PlanRequest {
        config: config(true),
        out: out.clone(),
        report_path: Some(report.clone()),
        resume: false,
    };
    let (result, log) = plan(tree.path(), req, None, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert!(!outcome.aborted);
    let artifact = check_artifact(&out, &outcome.artifact);
    assert!(report.is_file());

    for file in &artifact.files {
        assert_eq!(
            file.status,
            FileStatus::Complete,
            "{}: {file:#?}",
            file.path
        );
    }
    let languages: BTreeSet<LanguageId> = artifact.files.iter().map(|f| f.language).collect();
    assert_eq!(
        languages,
        BTreeSet::from(LanguageId::ALL),
        "the golden tree spans every tier-1 language"
    );
    let [protected, kept, removed, demoted] = check_offline_decisions(&artifact);
    assert!(
        protected > 0 && kept > 0 && removed > 0,
        "{protected} {kept} {removed}"
    );
    // The verify rail demotes exactly the golden edits the verifier is
    // known to reject (grammar-induced false rejections), so apply never
    // meets them.
    let demotions: Vec<(&str, &str)> = artifact
        .files
        .iter()
        .flat_map(|file| file.comments.iter().map(move |record| (file, record)))
        .filter(|(_, record)| record.tags.iter().any(|tag| tag == VERIFY_RAIL_TAG))
        .map(|(file, record)| (file.path.as_str(), record.original.as_deref().unwrap()))
        .collect();
    let expected: Vec<(&str, &str)> = common::EXPECTED_VERIFIER_REJECTIONS
        .iter()
        .map(|(key, raw, _)| (*key, *raw))
        .collect();
    assert_eq!(demotions, expected);
    assert_eq!(demoted, expected.len());

    let (applied, apply_log) = apply_artifact(tree.path(), &out);
    assert_eq!(applied.exit_code, 0, "{apply_log:#?}");
    for result in &applied.result.files {
        let planned = file(&artifact, &result.path);
        let expected = if has_actionable_edit(planned, artifact.config.allow_add) {
            ApplyStatus::Applied
        } else {
            ApplyStatus::Unchanged
        };
        assert_eq!(result.status, expected, "{}: {apply_log:#?}", result.path);
    }
    // Every grammar had an offline edit (a content-empty removal) applied and verified, not just a file left unchanged; otherwise the round trip is not exercising it.
    let applied_languages: BTreeSet<LanguageId> = applied
        .result
        .files
        .iter()
        .filter(|result| result.status == ApplyStatus::Applied)
        .map(|result| file(&artifact, &result.path).language)
        .collect();
    assert_eq!(applied_languages, languages);

    // A second offline pass over the applied tree finds nothing left to
    // remove: every removal landed, and the demoted ones are demoted again.
    let out2 = outputs.path().join("plan-2.json");
    let req = PlanRequest {
        config: config(true),
        out: out2.clone(),
        report_path: None,
        resume: false,
    };
    let (result, log) = plan(tree.path(), req, None, PlanHooks::default()).await;
    let second = result.unwrap();
    assert_eq!(second.exit_code, 0, "{log:#?}");
    let second = check_artifact(&out2, &second.artifact);
    let [_, _, removed_again, demoted_again] = check_offline_decisions(&second);
    assert_eq!((removed_again, demoted_again), (0, demoted));
}

#[tokio::test]
async fn the_artifact_does_not_depend_on_the_concurrency() {
    let mut artifacts = Vec::new();
    for concurrency in [1, 4] {
        let tree = common::copy_fixture_tree(TREE);
        let mut config = config(false);
        config.concurrency = concurrency;
        let (_fake, llm) = fake(removing_scenario());
        let req = request(config, "plan.json", None);
        let (result, _log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
        let outcome = result.unwrap();
        assert_eq!(outcome.exit_code, 0);
        artifacts.push(outcome.artifact);
    }
    assert_eq!(artifacts[0].files, artifacts[1].files);
    assert_eq!(artifacts[0].summary, artifacts[1].summary);
}

#[tokio::test]
async fn a_run_cancelled_before_scheduling_leaves_a_valid_artifact() {
    let tree = common::copy_fixture_tree(TREE);
    let (fake, llm) = fake(Scenario::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let log = CapturingLog::new();
    let outcome = run_plan(
        request(config(false), "plan.json", Some("report.md")),
        PlanDeps {
            log: &log,
            cwd: tree.path(),
            llm,
            cancel,
            hooks: PlanHooks::default(),
        },
    )
    .await
    .unwrap();
    assert!(outcome.aborted);
    assert_eq!(outcome.exit_code, 130);
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert!(artifact.files.is_empty(), "no file was scheduled");
    assert!(tree.path().join("report.md").is_file());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
#[should_panic(expected = "injected inventory panic")]
async fn a_panic_in_the_blocking_stage_stays_a_panic() {
    let tree = common::copy_fixture_tree(TREE);
    let hooks = PlanHooks {
        inventory: Some(Arc::new(|_: &str, _: munge_comments::FileRef| {
            panic!("injected inventory panic")
        })),
        ..PlanHooks::default()
    };
    let req = request(config(true), "plan.json", None);
    let _ = plan(tree.path(), req, None, hooks).await;
}

// ---------------------------------------------------------------------
// The plan-time verification rail.
// ---------------------------------------------------------------------

/// Verifications per file path.
type VerifyCounts = Arc<Mutex<BTreeMap<String, usize>>>;

/// Hooks whose verifier is the real one, counting its calls per file.
fn counting_verifier() -> (VerifyCounts, PlanHooks) {
    let counts = VerifyCounts::default();
    let seen = counts.clone();
    let hooks = PlanHooks {
        verify: Some(Arc::new(
            move |original: &str, edited: &str, file: FileRef, functional: &[FunctionalComment]| {
                *seen
                    .lock()
                    .unwrap()
                    .entry(file.file_name.clone())
                    .or_default() += 1;
                verify_semantic_equality(original, edited, file, functional)
            },
        )),
        ..PlanHooks::default()
    };
    (counts, hooks)
}

/// Applies the artifact `plan.json` in `root`, from `root`.
fn apply_plan(root: &Path) -> (ApplyOutcome, Vec<String>) {
    apply_artifact(root, Path::new("plan.json"))
}

/// Applies the artifact at `artifact` (relative to `root`, or absolute),
/// from `root`.
fn apply_artifact(root: &Path, artifact: &Path) -> (ApplyOutcome, Vec<String>) {
    let log = CapturingLog::new();
    let mut stdout = Vec::new();
    let applied = munge_comments::run_apply(
        ApplyRequest {
            artifact: artifact.to_path_buf(),
            only: Vec::new(),
            dry_run: false,
            report_path: None,
        },
        ApplyDeps {
            log: &log,
            stdout: &mut stdout,
            cwd: root,
            hooks: ApplyHooks::default(),
        },
    )
    .unwrap();
    (applied, log.lines())
}

fn apply_status(applied: &ApplyOutcome, path: &str) -> ApplyStatus {
    applied
        .result
        .files
        .iter()
        .find(|file| file.path == path)
        .map(|file| file.status)
        .unwrap_or_else(|| panic!("{path} is not in the apply result"))
}

/// The record whose original (or, for a NEW record, finalComment)
/// contains `needle`.
fn record_with<'a>(file: &'a FilePlan, needle: &str) -> &'a CommentRecord {
    file.comments
        .iter()
        .find(|record| {
            record
                .original
                .as_deref()
                .or(record.final_comment.as_deref())
                .is_some_and(|text| text.contains(needle))
        })
        .unwrap_or_else(|| panic!("{}: no record contains {needle:?}", file.path))
}

/// Asserts `record` was demoted by the rail from `prior` to `held`, and
/// that its file carries the matching `verify-demoted` warning.
fn assert_demoted(file: &FilePlan, record: &CommentRecord, prior: Decision, held: Decision) {
    assert_eq!(record.decision, held, "{record:#?}");
    assert_eq!(record.decided_by, DecidedBy::Engine, "{record:#?}");
    assert_eq!(record.confidence, 1.0);
    assert!(
        record.tags.iter().any(|tag| tag == VERIFY_RAIL_TAG),
        "{record:#?}"
    );
    let rationale = record.decision_rationale.as_deref().unwrap();
    assert!(
        rationale.contains(&format!("previously decided {}", prior.as_str())),
        "{rationale}"
    );
    let warnings: Vec<&FileWarning> = file
        .warnings
        .iter()
        .filter(|warning| warning.kind == FileWarningKind::VerifyDemoted)
        .collect();
    assert_eq!(warnings.len(), 1, "{:#?}", file.warnings);
    assert!(
        warnings[0]
            .detail
            .contains(&format!("record {}", record.id)),
        "{}",
        warnings[0].detail
    );
}

/// A scenario removing every comment whose body is one of `bodies` (row
/// 4: EASY, and the model would not comment there).
fn removing(bodies: &[&str]) -> serde_json::Map<String, serde_json::Value> {
    bodies
        .iter()
        .map(|body| {
            (
                (*body).to_owned(),
                json!({
                    "regen": { "comment": "", "difficulty": "EASY", "wouldYouCommentHere": false }
                }),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_removal_that_activates_a_dormant_cookie_is_demoted() {
    let tree = common::tempdir();
    let root = tree.path();
    let source = "# Tally the order lines before tax.\n\n# coding=utf-8\ndef total(items):\n    \
                  # add every price together\n    return sum(item.price for item in items)\n";
    fs::write(root.join("orders.py"), source).unwrap();
    let scenario = json!({
        "byKey": removing(&["Tally the order lines before tax.", "add every price together"])
    });
    let (_fake, llm) = fake(serde_json::from_value(scenario).unwrap());
    let (counts, hooks) = counting_verifier();
    let req = request(config(false), "plan.json", Some("report.md"));
    let (result, log) = plan(root, req, llm, hooks).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&root.join("plan.json"), &outcome.artifact);
    let orders = file(&artifact, "orders.py");
    assert_eq!(orders.status, FileStatus::Complete);

    // Removing line 1 would move the line-3 `# coding=utf-8` cookie onto line 2, so the rail demotes it.
    let first = record_with(orders, "Tally the order lines");
    assert_demoted(orders, first, Decision::Remove, Decision::KeepOriginal);
    // The file's other edit survives.
    let inner = record_with(orders, "add every price together");
    assert_eq!(inner.decision, Decision::Remove);
    assert!(!inner.tags.iter().any(|tag| tag == VERIFY_RAIL_TAG));
    // The rail runs per comment rather than per file: the failed full verification, then one per re-admitted record.
    assert_eq!(counts.lock().unwrap().get("orders.py"), Some(&3));
    assert!(
        log.iter()
            .any(|line| line.contains("orders.py") && line.contains("demoted 1 edit")),
        "{log:#?}"
    );

    let (applied, apply_log) = apply_plan(root);
    assert_eq!(applied.exit_code, 0, "{apply_log:#?}");
    assert_eq!(apply_status(&applied, "orders.py"), ApplyStatus::Applied);
    let written = fs::read_to_string(root.join("orders.py")).unwrap();
    assert!(written.starts_with("# Tally the order lines before tax.\n\n# coding=utf-8\n"));
    assert!(!written.contains("add every price together"), "{written}");
}

#[tokio::test]
async fn a_rewrite_that_turns_the_first_run_into_a_license_header_is_demoted() {
    let tree = common::tempdir();
    let root = tree.path();
    // Two records, one run (only whitespace between them).
    let source = "/* Order total helpers. */\n// Kept free of side effects.\nexport function \
                  total(prices: number[]): number {\n  // add every price together\n  return \
                  prices.reduce((sum, price) => sum + price, 0);\n}\n";
    fs::write(root.join("orders.ts"), source).unwrap();
    let mut by_key = removing(&["add every price together"]);
    by_key.insert(
        "Kept free of side effects.".to_owned(),
        json!({
            "regen": { "comment": "Pure functions only.", "difficulty": "MEDIUM" },
            "judge": {
                "decision": "KEEP_GENERATED",
                "finalComment": "Shipped under the project license terms.",
                "confidence": 0.9
            }
        }),
    );
    let scenario = json!({ "byKey": by_key });
    let (_fake, llm) = fake(serde_json::from_value(scenario).unwrap());
    let req = request(config(false), "plan.json", None);
    let (result, log) = plan(root, req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&root.join("plan.json"), &outcome.artifact);
    let orders = file(&artifact, "orders.ts");
    assert_eq!(orders.status, FileStatus::Complete);

    let rewrite = record_with(orders, "Kept free of side effects.");
    assert_demoted(
        orders,
        rewrite,
        Decision::KeepGenerated,
        Decision::KeepOriginal,
    );
    // The proposed text is kept for a reviewer, as every rail keeps it.
    assert_eq!(
        rewrite.final_comment.as_deref(),
        Some("Shipped under the project license terms.")
    );
    let inner = record_with(orders, "add every price together");
    assert_eq!(inner.decision, Decision::Remove);

    let (applied, apply_log) = apply_plan(root);
    assert_eq!(applied.exit_code, 0, "{apply_log:#?}");
    assert_eq!(apply_status(&applied, "orders.ts"), ApplyStatus::Applied);
    let written = fs::read_to_string(root.join("orders.ts")).unwrap();
    assert!(written.starts_with("/* Order total helpers. */\n// Kept free of side effects.\n"));
    assert!(!written.contains("license"), "{written}");
    assert!(!written.contains("add every price together"), "{written}");
}

#[tokio::test]
async fn a_file_whose_edits_verify_is_verified_exactly_once() {
    let tree = common::copy_fixture_tree(TREE);
    let (_fake, llm) = fake(removing_scenario());
    let (counts, hooks) = counting_verifier();
    let req = request(config(false), "plan.json", None);
    let (result, log) = plan(tree.path(), req, llm, hooks).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    let counts = counts.lock().unwrap().clone();
    assert_eq!(counts.get("py/calc.py"), Some(&1), "{counts:?}");
    for file in &artifact.files {
        // One verification for a file with an edit to make, none otherwise
        // (apply would leave it unchanged without verifying it either).
        let expected = usize::from(has_actionable_edit(file, artifact.config.allow_add));
        assert_eq!(
            counts.get(&file.path).copied().unwrap_or(0),
            expected,
            "{}",
            file.path
        );
        assert!(
            file.warnings
                .iter()
                .all(|warning| warning.kind != FileWarningKind::VerifyDemoted),
            "{}",
            file.path
        );
    }
}

#[tokio::test]
async fn a_new_comment_that_would_fail_verification_is_held_for_review() {
    let tree = common::tempdir();
    let root = tree.path();
    let source = "def total(items):\n    # add every price together\n    subtotal = \
                  sum(item.price for item in items)\n    return round(subtotal, 2)\n";
    fs::write(root.join("orders.py"), source).unwrap();
    let extra = |anchor: serde_json::Value, comment: &str| {
        json!({
            "path": "orders.py",
            "anchor": anchor,
            "position": "leading",
            "comment": comment,
            "difficulty": "MEDIUM"
        })
    };
    let scenario = json!({
        "extraRegenerations": [
            // The file's first comment, with a license keyword: inserted,
            // it would be a (functional) license header.
            extra(
                json!({ "nodeKind": "function_definition", "name": "total", "snippet": "def total(items):" }),
                "Released under the MIT license.",
            ),
            extra(
                json!({ "nodeKind": "return_statement", "name": null, "snippet": "return round(subtotal, 2)" }),
                "Rounds to whole cents.",
            ),
        ]
    });
    let (_fake, llm) = fake(serde_json::from_value(scenario).unwrap());
    let mut config = config(false);
    config.mode = Mode::Remove;
    config.allow_add = true;
    let req = request(config, "plan.json", None);
    let (result, log) = plan(root, req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&root.join("plan.json"), &outcome.artifact);
    let orders = file(&artifact, "orders.py");
    assert_eq!(orders.status, FileStatus::Complete);

    let license = record_with(orders, "MIT license");
    assert!(license.original.is_none(), "a NEW-origin record");
    assert_demoted(orders, license, Decision::New, Decision::NeedsReview);
    let cents = record_with(orders, "whole cents");
    assert_eq!(cents.decision, Decision::New, "{cents:#?}");

    let (applied, apply_log) = apply_plan(root);
    assert_eq!(applied.exit_code, 0, "{apply_log:#?}");
    assert_eq!(apply_status(&applied, "orders.py"), ApplyStatus::Applied);
    let written = fs::read_to_string(root.join("orders.py")).unwrap();
    assert!(!written.contains("license"), "{written}");
    assert!(
        written.contains("# Rounds to whole cents.\n    return round"),
        "{written}"
    );
}

#[tokio::test]
async fn a_non_verify_error_in_the_rail_fails_the_file_by_variant() {
    let tree = common::copy_fixture_tree(TREE);
    let hooks = PlanHooks {
        verify: Some(Arc::new(
            |original: &str, edited: &str, file: FileRef, functional: &[FunctionalComment]| {
                if file.file_name == "py/calc.py" {
                    Err(Error::Invariant(format!(
                        "injected verifier failure for {}",
                        file.file_name
                    )))
                } else {
                    verify_semantic_equality(original, edited, file, functional)
                }
            },
        )),
        ..PlanHooks::default()
    };
    let (_fake, llm) = fake(removing_scenario());
    let req = request(config(false), "plan.json", None);
    let (result, log) = plan(tree.path(), req, llm, hooks).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 7, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    let calc = file(&artifact, "py/calc.py");
    assert_eq!(calc.status, FileStatus::Failed);
    assert_eq!(calc.failure_kind, Some(FailureKind::Invariant));
    assert!(calc.content_hash.is_some());
    assert!(calc.comments.is_empty());
    assert!(
        calc.error
            .as_deref()
            .unwrap()
            .contains("injected verifier failure for py/calc.py")
    );
    for (path, _) in FILES.iter().filter(|(path, _)| *path != "py/calc.py") {
        assert_eq!(file(&artifact, path).status, FileStatus::Complete, "{path}");
    }
}

// ---------------------------------------------------------------------
// Run control: user cancellation, run-fatal conditions, the CLI wrapper.
// ---------------------------------------------------------------------

/// Runs `run_plan` from `cwd` with an explicit cancellation token.
async fn plan_with_cancel(
    cwd: &Path,
    req: PlanRequest,
    llm: Option<Arc<dyn LlmClient>>,
    cancel: CancellationToken,
) -> (Result<PlanOutcome, Error>, Vec<String>) {
    let log = CapturingLog::new();
    let result = run_plan(
        req,
        PlanDeps {
            log: &log,
            cwd,
            llm,
            cancel,
            hooks: PlanHooks::default(),
        },
    )
    .await;
    (result, log.lines())
}

fn with_concurrency(concurrency: u64) -> PlanConfig {
    PlanConfig {
        concurrency,
        ..config(false)
    }
}

/// A fake whose every call (or the first `times`) fails with `Auth`.
fn auth_failing(times: Option<u32>) -> Scenario {
    Scenario {
        failures: vec![FailureInjection {
            path: ANY_PATH.to_owned(),
            call: None,
            error: InjectedFailure::Auth,
            times,
        }],
        ..Scenario::default()
    }
}

#[tokio::test]
async fn a_run_cancelled_mid_way_keeps_only_the_files_it_finished() {
    let tree = common::copy_fixture_tree(TREE);
    let cancel = CancellationToken::new();
    let on_call = {
        let cancel = cancel.clone();
        Arc::new(move |index: usize, _: &LlmMeta| {
            if index == 1 {
                cancel.cancel();
            }
        })
    };
    let fake = Arc::new(FakeLlm::new(Scenario::default()).with_on_call(on_call));
    let client: Arc<dyn LlmClient> = fake.clone();
    let req = request(with_concurrency(1), "plan.json", Some("report.md"));
    let (result, log) = plan_with_cancel(tree.path(), req, Some(client), cancel).await;
    let outcome = result.unwrap();
    assert!(outcome.aborted, "{log:#?}");
    assert_eq!(outcome.exit_code, 130);
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert!(tree.path().join("report.md").is_file());

    assert!(!artifact.files.is_empty(), "the in-flight file finished");
    assert!(artifact.files.len() < FILES.len(), "{:?}", artifact.files);
    for file in &artifact.files {
        assert_eq!(file.status, FileStatus::Complete, "{}", file.path);
    }
    // Calls are bounded by the files started: every call belongs to a
    // file that is in the artifact.
    let started: Vec<&str> = artifact.files.iter().map(|f| f.path.as_str()).collect();
    let calls = fake.calls();
    assert!(calls.len() >= 2);
    for call in &calls {
        assert!(started.contains(&call.path.as_str()), "{}", call.path);
    }
}

/// A log that cancels a token when the run announces its pool, i.e. after
/// the artifact's first write and before any file is scheduled.
struct CancelOnPoolStart {
    inner: CapturingLog,
    cancel: CancellationToken,
}

impl Log for CancelOnPoolStart {
    fn info(&self, message: &str) {
        if message.contains("file(s) to plan") {
            self.cancel.cancel();
        }
        self.inner.info(message);
    }

    fn warn(&self, message: &str) {
        self.inner.warn(message);
    }
}

#[tokio::test]
async fn a_run_cancelled_before_any_file_completes_has_zero_files() {
    let tree = common::copy_fixture_tree(TREE);
    let (fake, llm) = fake(Scenario::default());
    let cancel = CancellationToken::new();
    let log = CancelOnPoolStart {
        inner: CapturingLog::new(),
        cancel: cancel.clone(),
    };
    let outcome = run_plan(
        request(config(false), "plan.json", Some("report.md")),
        PlanDeps {
            log: &log,
            cwd: tree.path(),
            llm,
            cancel,
            hooks: PlanHooks::default(),
        },
    )
    .await
    .unwrap();
    assert!(outcome.aborted, "{:#?}", log.inner.lines());
    assert_eq!(outcome.exit_code, 130);
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert!(artifact.files.is_empty());
    assert!(tree.path().join("report.md").is_file());
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn an_auth_failure_stops_scheduling_and_fails_the_rest_with_exit_5() {
    let tree = common::copy_fixture_tree(TREE);
    // The endpoint rejects only the first call: had scheduling continued,
    // later files would have succeeded.
    let (fake, llm) = fake(auth_failing(Some(1)));
    let req = request(with_concurrency(1), "plan.json", Some("report.md"));
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert!(!outcome.aborted);
    assert_eq!(outcome.exit_code, 5, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert!(tree.path().join("report.md").is_file());
    assert_eq!(fake.calls().len(), 1, "total calls <= concurrency");

    // Every discovered file has an outcome: the one whose call was
    // rejected, and the rest never attempted.
    assert_eq!(artifact.files.len(), FILES.len());
    let first = &fake.calls()[0].path;
    for file in &artifact.files {
        assert_eq!(file.status, FileStatus::Failed, "{}", file.path);
        assert_eq!(file.failure_kind, Some(FailureKind::Llm), "{}", file.path);
        assert!(file.content_hash.is_some(), "{}", file.path);
        let error = file.error.as_deref().unwrap();
        if &file.path == first {
            assert_ne!(error, NOT_ATTEMPTED_AUTH);
        } else {
            assert_eq!(error, NOT_ATTEMPTED_AUTH, "{}", file.path);
        }
    }
    assert!(
        log.iter()
            .any(|line| line.contains("authentication failed")),
        "{log:#?}"
    );
}

#[tokio::test]
async fn an_endpoint_rejecting_every_call_costs_at_most_one_call_per_slot() {
    let tree = common::copy_fixture_tree(TREE);
    let (fake, llm) = fake(auth_failing(None));
    let req = request(with_concurrency(2), "plan.json", None);
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.exit_code, 5, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    let calls = fake.calls();
    assert!((1..=2).contains(&calls.len()), "{calls:#?}");
    assert_eq!(artifact.files.len(), FILES.len());
    // Every file is one of: rejected (it made the one call its slot
    // allowed), finished without needing the model, or never attempted.
    let mut not_attempted = 0;
    for file in &artifact.files {
        let called = calls.iter().any(|call| call.path == file.path);
        match (called, file.status, file.error.as_deref()) {
            (true, FileStatus::Failed, Some(error)) => {
                assert_eq!(file.failure_kind, Some(FailureKind::Llm));
                assert!(error.contains("auth"), "{error}");
            }
            (false, FileStatus::Complete, _) => {}
            (false, FileStatus::Failed, Some(NOT_ATTEMPTED_AUTH)) => {
                assert_eq!(file.failure_kind, Some(FailureKind::Llm));
                not_attempted += 1;
            }
            other => panic!("{}: unexpected outcome {other:?}", file.path),
        }
    }
    assert!(not_attempted > 0, "scheduling stopped");
}

/// Makes `dir` read-only; `false` when permission bits are not enforced
/// for this user (the condition under test cannot be produced).
#[cfg(unix)]
fn make_read_only(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let enforced = fs::write(dir.join(".probe"), "").is_err();
    if !enforced {
        let _ = fs::remove_file(dir.join(".probe"));
        make_writable(dir);
    }
    enforced
}

#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn an_unwritable_out_is_exit_6() {
    let tree = common::copy_fixture_tree(TREE);
    let outputs = common::tempdir();
    if !make_read_only(outputs.path()) {
        return;
    }
    let (fake, llm) = fake(Scenario::default());
    let out = outputs.path().join("plan.json");
    let req = request(config(false), out.clone(), None);
    let (result, log) = plan(tree.path(), req, llm, PlanHooks::default()).await;
    make_writable(outputs.path());
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Fs { .. }), "{err:?} {log:#?}");
    assert_eq!(err.exit_code(), 6);
    assert!(!out.exists());
    assert!(
        fake.calls().is_empty(),
        "nothing is planned without an artifact"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_artifact_write_failing_mid_run_stops_the_run_with_the_writers_error() {
    let tree = common::copy_fixture_tree(TREE);
    let outputs = common::tempdir();
    let out_dir = canonical(outputs.path());
    // The first call (during the first file) revokes write access to the
    // artifact's directory, so recording that file fails.
    let on_call = {
        let out_dir = out_dir.clone();
        Arc::new(move |index: usize, _: &LlmMeta| {
            if index == 0 {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&out_dir, fs::Permissions::from_mode(0o555)).unwrap();
            }
        })
    };
    if !make_read_only(&out_dir) {
        return;
    }
    make_writable(&out_dir);
    let fake = Arc::new(FakeLlm::new(Scenario::default()).with_on_call(on_call));
    let client: Arc<dyn LlmClient> = fake.clone();
    let req = request(
        with_concurrency(1),
        out_dir.join("plan.json"),
        Some("report.md"),
    );
    let (result, log) = plan(tree.path(), req, Some(client), PlanHooks::default()).await;
    make_writable(&out_dir);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Fs { .. }), "{err:?} {log:#?}");
    assert_eq!(err.exit_code(), 6);
    // Scheduling stopped with the first file: every call belongs to it.
    let calls = fake.calls();
    assert!(!calls.is_empty());
    assert!(
        calls.iter().all(|call| call.path == calls[0].path),
        "{calls:#?}"
    );
    // The artifact on disk is the header-only one written at start; the failed write left it untouched.
    let on_disk = read_artifact(&out_dir.join("plan.json"))
        .unwrap()
        .into_inner();
    assert!(on_disk.files.is_empty());
    assert!(!tree.path().join("report.md").exists(), "no report");
    assert!(
        log.iter()
            .any(|line| line.contains("writing the artifact failed")),
        "{log:#?}"
    );
}

#[cfg(feature = "cli")]
mod cli_wrapper {
    use std::collections::HashMap;

    use munge_comments::cli::{INTERRUPTED_NOTE, Io, run};

    use super::common::SharedBuf;
    use super::*;

    /// An `Io` over `cwd` with an empty environment, plus its captured
    /// stderr.
    fn io(cwd: &Path) -> (Io, SharedBuf) {
        let err = SharedBuf::new();
        let io = Io::new(
            Box::new(SharedBuf::new()),
            Box::new(err.clone()),
            cwd.to_path_buf(),
            HashMap::new(),
        );
        (io, err)
    }

    #[tokio::test]
    async fn an_interrupted_plan_prints_the_note_and_exits_130() {
        let tree = common::copy_fixture_tree(TREE);
        let elsewhere = common::tempdir();
        let outputs = common::tempdir();
        let out = outputs.path().join("plan.json");
        let report = outputs.path().join("report.md");
        let (fake, llm) = fake(Scenario::default());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (io, err) = io(elsewhere.path());
        let io = io.with_llm(llm.unwrap()).with_cancel(cancel);
        let argv = [
            "munge-comments",
            "plan",
            tree.path().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--report",
            report.to_str().unwrap(),
        ];
        let code = run(argv, io).await;
        let stderr = err.contents();
        assert_eq!(code, 130, "{stderr}");
        assert!(stderr.contains(INTERRUPTED_NOTE), "{stderr}");
        assert_eq!(INTERRUPTED_NOTE, "interrupted — continue with --resume");
        let artifact = read_artifact(&out).unwrap().into_inner();
        assert!(artifact.files.is_empty());
        assert!(report.is_file());
        assert!(fake.calls().is_empty());
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_offline_plan_needs_no_api_key_and_constructs_no_client() {
        let tree = common::copy_fixture_tree(TREE);
        // An empty environment: building a client would be a usage error
        // (no key, no base URL), so exit 0 proves none was constructed.
        let (io, err) = io(tree.path());
        let code = run(["munge-comments", "plan", "--offline", "."], io).await;
        assert_eq!(code, 0, "{}", err.contents());
        let artifact = read_artifact(&tree.path().join("munge-comments-plan.json"))
            .unwrap()
            .into_inner();
        assert_eq!(artifact.files.len(), FILES.len());
        assert!(artifact.config.offline);
        assert!(!err.contents().contains(INTERRUPTED_NOTE));
    }

    #[tokio::test]
    async fn plan_offline_then_apply_round_trips_the_golden_tree_through_the_cli() {
        let tree = common::copy_fixture_tree("golden");
        let outputs = common::tempdir();
        let out = outputs.path().join("plan.json");
        let plan_report = outputs.path().join("plan-report.md");
        let apply_report = outputs.path().join("apply-report.md");
        let (io_plan, err) = io(tree.path());
        let argv = [
            "munge-comments",
            "plan",
            "--offline",
            ".",
            "--out",
            out.to_str().unwrap(),
            "--report",
            plan_report.to_str().unwrap(),
        ];
        let code = run(argv, io_plan).await;
        assert_eq!(code, 0, "{}", err.contents());
        let artifact = read_artifact(&out).unwrap().into_inner();
        assert!(artifact.config.offline);
        assert!(plan_report.is_file());
        let [_, _, removed, _] = check_offline_decisions(&artifact);
        assert!(removed > 0);

        let (io_apply, err) = io(tree.path());
        let argv = [
            "munge-comments",
            "apply",
            out.to_str().unwrap(),
            "--report",
            apply_report.to_str().unwrap(),
        ];
        let code = run(argv, io_apply).await;
        assert_eq!(code, 0, "{}", err.contents());
        assert!(apply_report.is_file());
        // Only the named outputs were written beside the tree's sources.
        assert!(!tree.path().join("munge-comments-plan.json").exists());
    }

    #[tokio::test]
    async fn default_outputs_land_in_the_injected_cwd_not_the_process_cwd() {
        let tree = common::copy_fixture_tree(TREE);
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let defaults = ["munge-comments-plan.json", "munge-comments-plan-report.md"];
        let preexisting: Vec<bool> = defaults.iter().map(|n| repo.join(n).exists()).collect();
        let (io, err) = io(tree.path());
        let code = run(["munge-comments", "plan", "--offline"], io).await;
        assert_eq!(code, 0, "{}", err.contents());
        for (name, existed) in defaults.iter().zip(preexisting) {
            assert!(
                tree.path().join(name).is_file(),
                "{name} in the injected cwd"
            );
            assert_eq!(repo.join(name).exists(), existed, "{name} not in the repo");
        }
        let artifact = read_artifact(&tree.path().join(defaults[0]))
            .unwrap()
            .into_inner();
        assert_eq!(artifact.root, ".");
        assert_eq!(artifact.files.len(), FILES.len());
    }
}
