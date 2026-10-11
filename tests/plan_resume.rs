//! `plan --resume`: `run_plan` resumed over an existing artifact, driven
//! with `FakeLlm` over temp copies of fixture trees. Every artifact written
//! is re-read through `read_artifact` (full validation).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use munge_comments::artifact::types::{FailureKind, FileStatus, SkipReason};
use munge_comments::llm::client::LlmMeta;
use munge_comments::llm::fake::{FailureInjection, InjectedFailure};
use munge_comments::{
    CapturingLog, Error, FakeLlm, LlmClient, Mode, PlanArtifact, PlanConfig, PlanDeps, PlanHooks,
    PlanOutcome, PlanRequest, Scenario, read_artifact, run_plan,
};
use tokio_util::sync::CancellationToken;

/// The mixed-language fixture tree (Go, JavaScript, Python, Rust,
/// TypeScript).
const TREE: &str = "apply-e2e";

/// Every file of [`TREE`], in artifact (path) order.
const FILES: [&str; 5] = [
    "go/count.go",
    "js/asi.js",
    "py/calc.py",
    "rs/lib.rs",
    "ts/greet.ts",
];

/// A config as the CLI would resolve it with default flags, at
/// concurrency 1 (deterministic interruption points).
fn config() -> PlanConfig {
    PlanConfig {
        mode: Mode::Redact,
        model: "gpt-4.1".to_owned(),
        base_url: None,
        allow_add: false,
        similarity_threshold: 0.6,
        max_file_chars: 120_000,
        concurrency: 1,
        offline: false,
        paths: vec![".".to_owned()],
        include: None,
        exclude: Vec::new(),
    }
}

fn request(config: PlanConfig, out: impl Into<PathBuf>, resume: bool) -> PlanRequest {
    PlanRequest {
        config,
        out: out.into(),
        report_path: None,
        resume,
    }
}

/// A fresh fake with the default scenario, plus the handle `run_plan`
/// takes.
fn fake(scenario: Scenario) -> (Arc<FakeLlm>, Option<Arc<dyn LlmClient>>) {
    let fake = Arc::new(FakeLlm::new(scenario));
    let client: Arc<dyn LlmClient> = fake.clone();
    (fake, Some(client))
}

/// A fake that cancels `cancel` on its second call (the first file's
/// judge call at concurrency 1): the first file finishes, nothing more is
/// scheduled.
fn interrupting(cancel: &CancellationToken) -> Option<Arc<dyn LlmClient>> {
    let cancel = cancel.clone();
    let on_call = Arc::new(move |index: usize, _: &LlmMeta| {
        if index == 1 {
            cancel.cancel();
        }
    });
    let client: Arc<dyn LlmClient> =
        Arc::new(FakeLlm::new(Scenario::default()).with_on_call(on_call));
    Some(client)
}

/// Runs `run_plan` from `cwd`, returning its result and the captured log.
async fn plan(
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

/// [`plan`] without cancellation, expecting success.
async fn plan_ok(
    cwd: &Path,
    req: PlanRequest,
    llm: Option<Arc<dyn LlmClient>>,
) -> (PlanOutcome, Vec<String>) {
    let (result, log) = plan(cwd, req, llm, CancellationToken::new()).await;
    match result {
        Ok(outcome) => (outcome, log),
        Err(err) => panic!("plan failed: {err}\n{log:#?}"),
    }
}

/// Re-reads the artifact at absolute `path` through full validation and
/// checks it is the returned one.
fn check_artifact(path: &Path, returned: &PlanArtifact) -> PlanArtifact {
    let artifact = read_artifact(path)
        .unwrap_or_else(|err| panic!("{} must validate: {err}", path.display()))
        .into_inner();
    assert_eq!(&artifact, returned, "the returned artifact is on disk");
    artifact
}

/// The artifact with `createdAt` blanked, for run-to-run comparisons.
fn timeless(mut artifact: PlanArtifact) -> PlanArtifact {
    artifact.created_at = String::new();
    artifact
}

fn paths(artifact: &PlanArtifact) -> Vec<&str> {
    artifact.files.iter().map(|f| f.path.as_str()).collect()
}

fn called_paths(fake: &FakeLlm) -> Vec<String> {
    let mut paths: Vec<String> = fake.calls().into_iter().map(|call| call.path).collect();
    paths.dedup();
    paths
}

/// An uninterrupted run over a fresh copy of [`TREE`] with `config`: its
/// artifact and the paths it called the model for.
async fn uninterrupted(config: PlanConfig) -> (PlanArtifact, Vec<String>) {
    let tree = common::copy_fixture_tree(TREE);
    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(tree.path(), request(config, "plan.json", false), llm).await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    (outcome.artifact, called_paths(&fake))
}

/// Interrupts a run over `cwd` after its first file; returns the paths of
/// the files it finished.
async fn interrupt(cwd: &Path, req: PlanRequest) -> Vec<String> {
    let cancel = CancellationToken::new();
    let llm = interrupting(&cancel);
    let (result, log) = plan(cwd, req, llm, cancel).await;
    let outcome = result.unwrap_or_else(|err| panic!("{err}\n{log:#?}"));
    assert!(outcome.aborted, "{log:#?}");
    assert_eq!(outcome.exit_code, 130);
    let done: Vec<String> = outcome
        .artifact
        .files
        .iter()
        .map(|f| f.path.clone())
        .collect();
    assert!(!done.is_empty(), "the in-flight file finished");
    done
}

#[tokio::test]
async fn an_interrupted_run_resumes_without_repeating_completed_work() {
    let tree = common::copy_fixture_tree(TREE);
    let out = tree.path().join("plan.json");
    let done = interrupt(tree.path(), request(config(), "plan.json", false)).await;
    assert!(done.len() < FILES.len(), "{done:?}");

    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(tree.path(), request(config(), "plan.json", true), llm).await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert!(!outcome.aborted);
    let artifact = check_artifact(&out, &outcome.artifact);

    // Zero repeated calls for the completed files: the resumed run calls
    // the model for exactly the files an uninterrupted run would have,
    // minus those already done.
    let called = called_paths(&fake);
    let (full, full_called) = uninterrupted(config()).await;
    let expected: Vec<String> = full_called
        .into_iter()
        .filter(|path| !done.contains(path))
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(called, expected);
    // The final artifact equals an uninterrupted run's.
    assert_eq!(paths(&artifact), FILES);
    assert_eq!(timeless(artifact), timeless(full));
}

#[tokio::test]
async fn a_resume_retries_exactly_the_failed_file() {
    let tree = common::copy_fixture_tree(TREE);
    let failing = Scenario {
        failures: vec![FailureInjection {
            path: "py/calc.py".to_owned(),
            call: None,
            error: InjectedFailure::Llm,
            times: None,
        }],
        ..Scenario::default()
    };
    let (_fake, llm) = fake(failing);
    let (first, log) = plan_ok(tree.path(), request(config(), "plan.json", false), llm).await;
    assert_eq!(first.exit_code, 5, "{log:#?}");
    let failed = first
        .artifact
        .files
        .iter()
        .find(|f| f.path == "py/calc.py")
        .unwrap();
    assert_eq!(failed.failure_kind, Some(FailureKind::Llm));

    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(tree.path(), request(config(), "plan.json", true), llm).await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert_eq!(called_paths(&fake), ["py/calc.py"]);
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert!(
        artifact
            .files
            .iter()
            .all(|f| f.status == FileStatus::Complete)
    );
    assert_eq!(
        timeless(artifact),
        timeless(uninterrupted(config()).await.0)
    );
}

#[tokio::test]
async fn skipped_files_are_retried_only_when_their_hash_changed() {
    let tree = common::tempdir();
    let root = tree.path();
    fs::write(root.join("ok.ts"), "// fine\nexport const x = 1;\n").unwrap();
    fs::write(root.join("broken.ts"), "// broken\nexport const = ;\n").unwrap();
    let (_fake, llm) = fake(Scenario::default());
    let (first, log) = plan_ok(root, request(config(), "plan.json", false), llm).await;
    assert_eq!(first.exit_code, 0, "{log:#?}");
    let broken = first.artifact.files.iter().find(|f| f.path == "broken.ts");
    assert_eq!(broken.unwrap().skip_reason, Some(SkipReason::SyntaxErrors));

    // Unchanged: both files are carried, nothing is planned.
    let (fake2, llm) = fake(Scenario::default());
    let (second, log) = plan_ok(root, request(config(), "plan.json", true), llm).await;
    assert_eq!(second.exit_code, 0, "{log:#?}");
    assert!(fake2.calls().is_empty());
    assert!(
        log.iter()
            .any(|line| line.contains("2 file(s) carried, 0 to plan, 0 dropped")),
        "{log:#?}"
    );
    assert_eq!(second.artifact.files, first.artifact.files);

    // Fixed on disk: the skipped file is re-planned (and only it).
    fs::write(root.join("broken.ts"), "// fixed\nexport const y = 2;\n").unwrap();
    let (fake3, llm) = fake(Scenario::default());
    let (third, log) = plan_ok(root, request(config(), "plan.json", true), llm).await;
    assert_eq!(third.exit_code, 0, "{log:#?}");
    assert_eq!(called_paths(&fake3), ["broken.ts"]);
    let artifact = check_artifact(&root.join("plan.json"), &third.artifact);
    let fixed = artifact
        .files
        .iter()
        .find(|f| f.path == "broken.ts")
        .unwrap();
    assert_eq!(fixed.status, FileStatus::Complete);
}

/// Plans [`TREE`] completely, then resumes it with `changed` applied to
/// the config: the resume must be refused (exit 1) with `diff_line` in its
/// message, leaving the artifact untouched and the model uncalled.
async fn assert_snapshot_refused(changed: impl Fn(&mut PlanConfig), diff_line: &str) {
    let tree = common::copy_fixture_tree(TREE);
    let out = tree.path().join("plan.json");
    let (_fake, llm) = fake(Scenario::default());
    plan_ok(tree.path(), request(config(), "plan.json", false), llm).await;
    let before = fs::read(&out).unwrap();

    let mut resumed = config();
    changed(&mut resumed);
    let (fake, llm) = fake(Scenario::default());
    let (result, _log) = plan(
        tree.path(),
        request(resumed, "plan.json", true),
        llm,
        CancellationToken::new(),
    )
    .await;
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert_eq!(err.exit_code(), 1);
    let message = err.to_string();
    assert!(message.contains(diff_line), "{message}");
    assert!(
        message.contains("--out"),
        "suggests a fresh plan: {message}"
    );
    assert_eq!(fs::read(&out).unwrap(), before, "the artifact is untouched");
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn a_changed_exclude_is_refused_with_the_diff() {
    assert_snapshot_refused(
        |config| config.exclude = vec!["py/**".to_owned()],
        r#"exclude: artifact [] ≠ requested ["py/**"]"#,
    )
    .await;
}

#[tokio::test]
async fn a_changed_allow_add_is_refused_with_the_diff() {
    assert_snapshot_refused(
        |config| config.allow_add = true,
        "allowAdd: artifact false ≠ requested true",
    )
    .await;
}

#[tokio::test]
async fn a_resume_from_a_subdirectory_matches_globs_against_the_stored_base() {
    // A repo with fixtures that `--exclude 'tests/fixtures/**'` keeps out.
    let make_repo = || {
        let repo = common::tempdir();
        let root = repo.path();
        for (path, text) in [
            ("lib/a.ts", "// first\nexport const a = 1;\n"),
            ("lib/b.ts", "// second\nexport const b = 2;\n"),
            ("tests/t.ts", "// a test\nexport const t = 3;\n"),
            ("tests/fixtures/f.ts", "// a fixture\nexport const f = 4;\n"),
        ] {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), text).unwrap();
        }
        repo
    };
    let excluding = |paths: &str| PlanConfig {
        paths: vec![paths.to_owned()],
        exclude: vec!["tests/fixtures/**".to_owned()],
        ..config()
    };

    let full_repo = make_repo();
    let (_fake, llm) = fake(Scenario::default());
    let (full, log) = plan_ok(
        full_repo.path(),
        request(excluding("."), "plan.json", false),
        llm,
    )
    .await;
    assert_eq!(full.exit_code, 0, "{log:#?}");
    assert_eq!(
        paths(&full.artifact),
        ["lib/a.ts", "lib/b.ts", "tests/t.ts"]
    );

    let repo = make_repo();
    let done = interrupt(repo.path(), request(excluding("."), "plan.json", false)).await;
    assert!(done.len() < 3, "{done:?}");
    // Resumed from `lib/` with the equivalent positional path `..`: the
    // exclude glob is matched against the stored base (the repo root), not
    // against `lib/`, so the fixture stays excluded.
    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(
        &repo.path().join("lib"),
        request(excluding(".."), "../plan.json", true),
        llm,
    )
    .await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&repo.path().join("plan.json"), &outcome.artifact);
    assert_eq!(artifact.root, ".");
    assert_eq!(artifact.glob_base, ".");
    assert_eq!(artifact.config.paths, ["."]);
    assert!(called_paths(&fake).iter().all(|path| !done.contains(path)));
    assert_eq!(timeless(artifact), timeless(full.artifact));
}

#[tokio::test]
async fn an_all_carried_resume_makes_no_calls_and_rewrites_the_artifact() {
    let tree = common::copy_fixture_tree(TREE);
    let out = tree.path().join("plan.json");
    let (_fake, llm) = fake(Scenario::default());
    let (first, _log) = plan_ok(tree.path(), request(config(), "plan.json", false), llm).await;
    // A recognizable createdAt shows the original one is preserved.
    let mut original = first.artifact.clone();
    original.created_at = "2020-01-02T03:04:05Z".to_owned();
    fs::write(
        &out,
        munge_comments::artifact::write::artifact_to_json(&original).unwrap(),
    )
    .unwrap();

    let resumed = PlanConfig {
        concurrency: 8,
        ..config()
    };
    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(tree.path(), request(resumed, "plan.json", true), llm).await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    let artifact = check_artifact(&out, &outcome.artifact);
    assert_eq!(
        artifact.config.concurrency, 8,
        "the new concurrency is written"
    );
    assert_eq!(artifact.created_at, "2020-01-02T03:04:05Z");
    assert_eq!(artifact.root, original.root);
    assert_eq!(artifact.glob_base, original.glob_base);
    assert_eq!(artifact.files, original.files);
    assert_eq!(artifact.summary, original.summary);
}

#[tokio::test]
async fn a_resume_without_an_artifact_starts_fresh() {
    let tree = common::copy_fixture_tree(TREE);
    let (_fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(tree.path(), request(config(), "plan.json", true), llm).await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert!(
        log.iter()
            .any(|line| line.contains("starting a fresh plan")),
        "{log:#?}"
    );
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert_eq!(paths(&artifact), FILES);
    assert_eq!(
        timeless(artifact),
        timeless(uninterrupted(config()).await.0)
    );
}

#[tokio::test]
async fn an_invalid_artifact_is_not_resumed() {
    let tree = common::copy_fixture_tree(TREE);
    let out = tree.path().join("plan.json");
    fs::write(&out, "{ \"version\": 1, \n").unwrap();
    let (fake, llm) = fake(Scenario::default());
    let (result, _log) = plan(
        tree.path(),
        request(config(), "plan.json", true),
        llm,
        CancellationToken::new(),
    )
    .await;
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Artifact(_)), "{err:?}");
    assert_eq!(err.exit_code(), 2);
    assert_eq!(fs::read_to_string(&out).unwrap(), "{ \"version\": 1, \n");
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn deleted_files_are_dropped_and_edited_files_replanned() {
    let tree = common::copy_fixture_tree(TREE);
    let (_fake, llm) = fake(Scenario::default());
    let (first, _log) = plan_ok(tree.path(), request(config(), "plan.json", false), llm).await;
    let old_hash = first
        .artifact
        .files
        .iter()
        .find(|f| f.path == "ts/greet.ts")
        .and_then(|f| f.content_hash.clone());

    fs::remove_file(tree.path().join("go/count.go")).unwrap();
    let greet = tree.path().join("ts/greet.ts");
    let mut text = fs::read_to_string(&greet).unwrap();
    text.push_str("// say goodbye\nexport const bye = \"bye\";\n");
    fs::write(&greet, text).unwrap();

    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(tree.path(), request(config(), "plan.json", true), llm).await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    let artifact = check_artifact(&tree.path().join("plan.json"), &outcome.artifact);
    assert_eq!(
        paths(&artifact),
        ["js/asi.js", "py/calc.py", "rs/lib.rs", "ts/greet.ts"]
    );
    assert!(
        log.iter()
            .any(|line| line.contains("go/count.go") && line.contains("no longer discovered")),
        "{log:#?}"
    );
    assert_eq!(called_paths(&fake), ["ts/greet.ts"]);
    let replanned = artifact
        .files
        .iter()
        .find(|f| f.path == "ts/greet.ts")
        .unwrap();
    assert_eq!(replanned.status, FileStatus::Complete);
    assert_ne!(replanned.content_hash, old_hash);
    assert!(
        replanned
            .comments
            .iter()
            .any(|record| record.original.as_deref() == Some("// say goodbye"))
    );
    // The unchanged files are carried as they were.
    for file in artifact.files.iter().filter(|f| f.path != "ts/greet.ts") {
        let before = first.artifact.files.iter().find(|f| f.path == file.path);
        assert_eq!(Some(file), before, "{}", file.path);
    }
}

#[tokio::test]
async fn a_resume_from_another_cwd_reuses_the_root() {
    let tree = common::copy_fixture_tree(TREE);
    let outside = common::tempdir();
    let out = outside.path().join("plans/plan.json");
    let (_fake, llm) = fake(Scenario::default());
    let (first, _log) = plan_ok(tree.path(), request(config(), &out, false), llm).await;
    assert_eq!(first.exit_code, 0);

    // From `outside`, the same artifact (relative `--out`) and the tree
    // given by its absolute path: equivalent inputs.
    let tree_abs = fs::canonicalize(tree.path()).unwrap();
    let resumed = PlanConfig {
        paths: vec![tree_abs.to_str().unwrap().to_owned()],
        ..config()
    };
    let (fake, llm) = fake(Scenario::default());
    let (outcome, log) = plan_ok(
        outside.path(),
        request(resumed, "plans/plan.json", true),
        llm,
    )
    .await;
    assert_eq!(outcome.exit_code, 0, "{log:#?}");
    assert!(fake.calls().is_empty());
    let artifact = check_artifact(&out, &outcome.artifact);
    assert_eq!(artifact.root, first.artifact.root);
    assert_eq!(artifact.glob_base, ".");
    assert_eq!(artifact.config.paths, ["."]);
    assert_eq!(artifact.files, first.artifact.files);
}

#[tokio::test]
async fn a_resume_whose_discovery_escapes_the_stored_root_is_refused() {
    let tree = common::tempdir();
    let a = tree.path().join("a");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(tree.path().join("b")).unwrap();
    fs::write(a.join("x.ts"), "// in a\nexport const x = 1;\n").unwrap();
    let both = PlanConfig {
        paths: vec![".".to_owned(), "../b".to_owned()],
        ..config()
    };
    let (_fake, llm) = fake(Scenario::default());
    let (first, _log) = plan_ok(&a, request(both.clone(), "plan.json", false), llm).await;
    assert_eq!(first.artifact.root, ".", "every file lay under the cwd");
    let before = fs::read(a.join("plan.json")).unwrap();

    // `../b` now yields a file outside the stored root `a/`.
    fs::write(tree.path().join("b/y.ts"), "// in b\nexport const y = 2;\n").unwrap();
    let (fake, llm) = fake(Scenario::default());
    let (result, _log) = plan(
        &a,
        request(both, "plan.json", true),
        llm,
        CancellationToken::new(),
    )
    .await;
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string()
            .contains("discovery escapes the resumed root; start a fresh plan"),
        "{err}"
    );
    assert_eq!(fs::read(a.join("plan.json")).unwrap(), before);
    assert!(fake.calls().is_empty());
}
