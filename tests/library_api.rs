//! The supported library surface, proven end to end: an offline
//! `plan` → `apply` → report round trip over a temp copy of the golden
//! fixture tree, every output in a `TempDir`.
//!
//! This crate deliberately names nothing but `munge_comments::` crate-root
//! re-exports (no module paths, and no `tests/common` helpers, which reach
//! into modules), so it breaks whenever the documented root API stops
//! being enough for a consumer. It needs no `cli` feature: the gate runs
//! it with `cargo test --no-default-features --lib --test library_api`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use munge_comments::{
    ApplyDeps, ApplyHooks, ApplyRequest, ApplyStatus, CancellationToken, CapturingLog, Decision,
    FileRef, LanguageId, Mode, PlanConfig, PlanDeps, PlanHooks, PlanRequest, compute_summary,
    functional_sequence, inventory_file, language_for, read_artifact, render_markdown, run_apply,
    run_plan, verify_semantic_equality,
};
use tempfile::TempDir;

/// The fixture tree planned: one directory per tier-1 language.
fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden")
}

/// Copies the tree at `src` to `dst` byte-exactly, returning every file's
/// text keyed by its `/`-separated path relative to `dst`.
fn copy_tree(src: &Path, dst: &Path, prefix: &str, files: &mut BTreeMap<String, String>) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dst.join(&name), &rel, files);
        } else {
            let bytes = fs::read(entry.path()).unwrap();
            fs::write(dst.join(&name), &bytes).unwrap();
            files.insert(rel, String::from_utf8(bytes).unwrap());
        }
    }
}

/// An offline config, as the CLI resolves `plan --offline` with default
/// flags.
fn offline_config() -> PlanConfig {
    PlanConfig {
        mode: Mode::Redact,
        model: "gpt-4.1".to_owned(),
        base_url: None,
        allow_add: false,
        similarity_threshold: 0.6,
        max_file_chars: 120_000,
        concurrency: 4,
        offline: true,
        paths: vec![".".to_owned()],
        include: None,
        exclude: Vec::new(),
    }
}

#[tokio::test]
async fn an_offline_plan_apply_report_round_trip_runs_on_the_root_api() {
    let temp = TempDir::new().unwrap();
    let tree = temp.path().join("tree");
    let mut originals = BTreeMap::new();
    copy_tree(&golden_root(), &tree, "", &mut originals);
    let outputs = temp.path().join("outputs");
    let artifact_path = outputs.join("plan.json");
    let plan_report = outputs.join("plan-report.md");
    let apply_report = outputs.join("apply-report.md");

    // Plan: offline, so no client is supplied; the captured log backs the assertion messages.
    let log = CapturingLog::new();
    let outcome = run_plan(
        PlanRequest {
            config: offline_config(),
            out: artifact_path.clone(),
            report_path: Some(plan_report.clone()),
            resume: false,
        },
        PlanDeps {
            log: &log,
            cwd: &tree,
            llm: None,
            cancel: CancellationToken::new(),
            hooks: PlanHooks::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.exit_code, 0, "{:#?}", log.lines());
    assert!(!outcome.aborted);

    let artifact = read_artifact(&artifact_path).unwrap().into_inner();
    assert_eq!(
        artifact, outcome.artifact,
        "the returned artifact is on disk"
    );
    let summary = &artifact.summary;
    assert_eq!(
        summary,
        &compute_summary(&artifact.files, &artifact.config).unwrap()
    );
    assert_eq!(summary.files_considered, originals.len() as u64);
    assert_eq!((summary.files_failed, summary.files_skipped), (0, 0));
    let languages: BTreeSet<LanguageId> = summary.languages.keys().copied().collect();
    assert_eq!(languages, BTreeSet::from(LanguageId::ALL));
    // Redact mode never rewrites or adds; offline decisions only keep, protect or remove (content-empty).
    assert_eq!((summary.rewritten, summary.new), (0, 0));
    assert!(summary.removed > 0);
    for record in artifact.files.iter().flat_map(|file| &file.comments) {
        assert!(
            matches!(
                record.decision,
                Decision::KeepOriginal | Decision::KeepProtected | Decision::Remove
            ),
            "{record:?}"
        );
        assert_eq!(record.regenerated, None);
    }
    assert_eq!(
        fs::read_to_string(&plan_report).unwrap(),
        render_markdown(&artifact, None).unwrap(),
        "the plan report is the artifact's rendering"
    );

    // Apply: synchronous, from the tree.
    let mut stdout = Vec::new();
    let applied = run_apply(
        ApplyRequest {
            artifact: artifact_path.clone(),
            only: Vec::new(),
            dry_run: false,
            report_path: Some(apply_report.clone()),
        },
        ApplyDeps {
            log: &log,
            stdout: &mut stdout,
            cwd: &tree,
            hooks: ApplyHooks::default(),
        },
    )
    .unwrap();
    assert_eq!(applied.exit_code, 0, "{:#?}", log.lines());
    assert!(stdout.is_empty(), "only a dry run prints");
    assert_eq!(applied.result.files.len(), originals.len());
    let mut applied_languages = BTreeSet::new();
    for result in &applied.result.files {
        let original = &originals[&result.path];
        let edited = fs::read_to_string(tree.join(&result.path)).unwrap();
        match result.status {
            ApplyStatus::Applied => {
                let language = language_for(Path::new(&result.path)).unwrap();
                let file = FileRef {
                    language,
                    file_name: result.path.clone(),
                };
                let inventory = inventory_file(original, file.clone()).unwrap();
                verify_semantic_equality(original, &edited, file, &functional_sequence(&inventory))
                    .unwrap_or_else(|err| panic!("{}: {err}", result.path));
                assert!(edited.len() < original.len(), "{}", result.path);
                applied_languages.insert(language);
            }
            ApplyStatus::Unchanged => assert_eq!(&edited, original, "{}", result.path),
            other => panic!("{}: {other:?}", result.path),
        }
    }
    assert_eq!(applied_languages, languages, "every language was edited");

    assert_eq!(
        fs::read_to_string(&apply_report).unwrap(),
        render_markdown(&artifact, Some(&applied.result)).unwrap()
    );

    // Every output landed in the TempDir's output directory.
    let mut written: Vec<String> = fs::read_dir(&outputs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    written.sort();
    assert_eq!(written, ["apply-report.md", "plan-report.md", "plan.json"]);
}
