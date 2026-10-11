//! Env-gated e2e against a real OpenAI-compatible endpoint.
//!
//! Never run by default: every test is `#[ignore]`d and also returns early
//! unless `MUNGE_COMMENTS_E2E=1`. Run them with
//!
//! ```text
//! MUNGE_COMMENTS_E2E=1 cargo test --test real_endpoint -- --ignored
//! ```
//!
//! The endpoint is configured exactly as for the binary (`OPENAI_API_KEY`,
//! `OPENAI_BASE_URL`, `MUNGE_COMMENTS_MODEL`): each test drives the real
//! CLI path, `run([...], Io::from_process())`, so configuration resolution
//! and client construction are the production ones. Sources are temp
//! copies; artifacts and reports go to a separate temp directory (absolute
//! paths, so nothing lands in the process cwd).

#![cfg(feature = "cli")]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::noisy_scenario::{NOISY_REPO, load_manifest};
use common::retention::assert_retention;
use munge_comments::artifact::types::FileStatus;
use munge_comments::cli::{Io, run};
use munge_comments::{Decision, PlanArtifact, read_artifact};

/// The opt-in switch.
const GATE: &str = "MUNGE_COMMENTS_E2E";

/// Whether the real-endpoint tests are enabled; prints a note when not.
#[allow(clippy::disallowed_methods)] // the sanctioned env-gate read
fn enabled() -> bool {
    let on = std::env::var(GATE).is_ok_and(|value| value == "1");
    if !on {
        eprintln!("skipping: set {GATE}=1 to run the real-endpoint e2e");
    }
    on
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn arg(path: &Path) -> String {
    path.to_str().expect("temp paths are UTF-8").to_owned()
}

/// `munge-comments plan <src> --out <out> --report <report>` through the
/// real CLI; asserts exit 0 and returns the artifact, re-read through full
/// validation.
async fn plan(src: &Path, out: &Path, report: &Path) -> PlanArtifact {
    let code = run(
        [
            "munge-comments".to_owned(),
            "plan".to_owned(),
            arg(src),
            "--out".to_owned(),
            arg(out),
            "--report".to_owned(),
            arg(report),
        ],
        Io::from_process(),
    )
    .await;
    assert_eq!(code, 0, "plan exited {code}");
    assert!(report.is_file(), "the plan report is written");
    let artifact = read_artifact(out)
        .unwrap_or_else(|err| panic!("{} must validate: {err}", out.display()))
        .into_inner();
    for file in &artifact.files {
        assert_eq!(
            file.status,
            FileStatus::Complete,
            "{}: {:?}",
            file.path,
            file.error
        );
    }
    artifact
}

/// `munge-comments apply <artifact> --report <report>` through the real
/// CLI; asserts exit 0 (every file verifier-clean).
async fn apply(artifact: &Path, report: &Path) {
    let code = run(
        [
            "munge-comments".to_owned(),
            "apply".to_owned(),
            arg(artifact),
            "--report".to_owned(),
            arg(report),
        ],
        Io::from_process(),
    )
    .await;
    assert_eq!(code, 0, "apply exited {code}");
    assert!(report.is_file(), "the apply report is written");
}

#[tokio::test]
#[ignore = "real endpoint: set MUNGE_COMMENTS_E2E=1"]
async fn smoke_one_tiny_file_plans_against_the_configured_endpoint() {
    if !enabled() {
        return;
    }
    let src = common::tempdir();
    let work = common::tempdir();
    fs::write(
        src.path().join("tiny.py"),
        "def double(x):\n    # multiply x by two and return it\n    return x * 2  # noqa: E501\n",
    )
    .unwrap();
    let work_dir = canonical(work.path());
    let artifact = plan(
        &canonical(src.path()),
        &work_dir.join("plan.json"),
        &work_dir.join("plan.md"),
    )
    .await;
    assert_eq!(artifact.files.len(), 1);
    let tiny = &artifact.files[0];
    assert_eq!(tiny.path, "tiny.py");
    let prose = tiny
        .comments
        .iter()
        .find(|record| record.original.as_deref() == Some("# multiply x by two and return it"))
        .expect("the prose comment is planned");
    assert!(
        prose.regenerated.is_some(),
        "the model regenerated it: {prose:#?}"
    );
    assert!(prose.difficulty.is_some(), "the model rated it: {prose:#?}");
    let directive = tiny
        .comments
        .iter()
        .find(|record| record.original.as_deref() == Some("# noqa: E501"))
        .expect("the directive is planned");
    assert_eq!(directive.decision, Decision::KeepProtected);
}

#[tokio::test]
#[ignore = "real endpoint: set MUNGE_COMMENTS_E2E=1"]
async fn the_noisy_repo_hits_the_retention_targets_in_redact_mode() {
    if !enabled() {
        return;
    }
    let tree = common::copy_fixture_tree(NOISY_REPO);
    let work = common::tempdir();
    let work_dir = canonical(work.path());
    let out = work_dir.join("plan.json");
    let artifact = plan(&canonical(tree.path()), &out, &work_dir.join("plan.md")).await;
    assert_retention(&load_manifest(), &artifact, "real endpoint, redact mode");
    apply(&out, &work_dir.join("apply.md")).await;
}
