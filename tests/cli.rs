//! CLI skeleton tests: parsing, exit codes, stream routing, config
//! resolution, and the panic canary — all through `cli::run` with injected
//! `SharedBuf` streams, never the process's own stdio or environment.

#![cfg(feature = "cli")]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap::Parser;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use munge_comments::artifact::types::Decision;
use munge_comments::artifact::write::{ArtifactHeader, ArtifactWriter};
use munge_comments::cli::{Cli, Command, Io, ModeArg, PlanArgs, run};
use munge_comments::comments::types::Classification;
use munge_comments::config::{DEFAULT_MODEL, resolve_plan};
use munge_comments::{
    FakeLlm, FileRef, LanguageId, Mode, PlanConfig, Scenario, compute_summary, inventory_file,
};

use common::SharedBuf;

/// A test-run harness: owns the temp cwd (so it outlives the run), the
/// injected `Io`, and clones of both captured streams.
struct TestIo {
    dir: TempDir,
    io: Io,
    out: SharedBuf,
    err: SharedBuf,
}

impl TestIo {
    fn new(env: HashMap<String, String>) -> TestIo {
        let dir = common::tempdir();
        let out = SharedBuf::new();
        let err = SharedBuf::new();
        let io = Io::new(
            Box::new(out.clone()),
            Box::new(err.clone()),
            dir.path().to_path_buf(),
            env,
        );
        TestIo { dir, io, out, err }
    }

    /// Runs `munge-comments <args>` and returns the exit code plus both
    /// captured streams (the temp cwd lives until the run completes).
    async fn run(self, args: &[&str]) -> (u8, SharedBuf, SharedBuf) {
        let TestIo {
            dir: _dir,
            io,
            out,
            err,
        } = self;
        let argv = std::iter::once("munge-comments").chain(args.iter().copied());
        let code = run(argv, io).await;
        (code, out, err)
    }
}

/// Runs `munge-comments <args>` in a fresh temp cwd with an empty
/// environment and returns `(exit code, stdout, stderr)`.
async fn run_args(args: &[&str]) -> (u8, String, String) {
    let (code, out, err) = TestIo::new(HashMap::new()).run(args).await;
    (code, out.contents(), err.contents())
}

fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// Parses `munge-comments plan <args>` into [`PlanArgs`], proving at the
/// parse level how arguments land (positional preservation, repeatable
/// flags, defaults).
fn parse_plan(args: &[&str]) -> PlanArgs {
    let argv = std::iter::once("munge-comments")
        .chain(std::iter::once("plan"))
        .chain(args.iter().copied());
    let cli = Cli::try_parse_from(argv).expect("plan args parse");
    match cli.command {
        Command::Plan(args) => args,
        _ => unreachable!("parsed a plan invocation"),
    }
}

#[tokio::test]
async fn version_prints_name_and_version_and_exits_zero() {
    let (code, out, err) = run_args(&["--version"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!("munge-comments {}\n", env!("CARGO_PKG_VERSION")),
    );
    assert_eq!(err, "");
}

#[tokio::test]
async fn help_goes_to_stdout_and_exits_zero() {
    let (code, out, err) = run_args(&["--help"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("munge-comments"));
    for command in ["plan", "apply", "report"] {
        assert!(out.contains(command), "help must mention {command}");
    }
    assert_eq!(err, "");
}

#[tokio::test]
async fn bare_invocation_shows_help_on_stderr_and_exits_one() {
    let (code, out, err) = run_args(&[]).await;
    assert_eq!(code, 1);
    assert_eq!(out, "", "stdout stays empty for a bare invocation");
    assert!(
        err.contains("Usage: munge-comments"),
        "stderr carries the help text:\n{err}",
    );
}

#[tokio::test]
async fn unknown_flag_is_a_usage_error() {
    let (code, out, err) = run_args(&["plan", "--bogus"]).await;
    assert_eq!(code, 1);
    assert_eq!(out, "");
    assert!(
        err.contains("--bogus"),
        "the error names the offending flag:\n{err}",
    );
}

#[tokio::test]
async fn out_of_range_similarity_threshold_is_rejected() {
    let (code, out, err) = run_args(&["plan", "--similarity-threshold", "1.5"]).await;
    assert_eq!(code, 1);
    assert_eq!(out, "");
    assert!(
        err.contains("--similarity-threshold") && err.contains("1.5"),
        "the error names the flag and the value:\n{err}",
    );
}

#[tokio::test]
async fn out_of_range_concurrency_is_rejected() {
    let (code, _, err) = run_args(&["plan", "--concurrency", "0"]).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("--concurrency"),
        "the error names the flag:\n{err}",
    );
}

#[tokio::test]
async fn an_online_plan_without_an_api_key_or_base_url_is_a_usage_error() {
    let (code, out, err) = run_args(&["plan"]).await;
    assert_eq!(code, 1, "stderr: {err}");
    assert_eq!(out, "");
    assert!(err.contains("no API key"), "stderr: {err}");
}

#[tokio::test]
async fn an_offline_plan_of_an_empty_cwd_writes_its_outputs_there_and_exits_zero() {
    let dir = common::tempdir();
    let (out, err) = (SharedBuf::new(), SharedBuf::new());
    let io = Io::new(
        Box::new(out.clone()),
        Box::new(err.clone()),
        dir.path().to_path_buf(),
        HashMap::new(),
    );
    let code = run(["munge-comments", "plan", "--offline"], io).await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    assert_eq!(out.contents(), "");
    assert!(dir.path().join("munge-comments-plan.json").is_file());
    assert!(dir.path().join("munge-comments-plan-report.md").is_file());
}

#[tokio::test]
async fn apply_with_nonexistent_artifact_is_exit_two() {
    let (code, out, err) = run_args(&["apply", "missing.json"]).await;
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert!(
        err.contains("missing.json"),
        "the error names the artifact:\n{err}",
    );
}

#[tokio::test]
async fn bare_apply_is_exit_two() {
    let (code, _, err) = run_args(&["apply"]).await;
    assert_eq!(code, 2);
    assert!(
        err.contains("ARTIFACT"),
        "the error explains the missing positional:\n{err}",
    );
}

#[tokio::test]
async fn bare_report_is_exit_two() {
    let (code, _, err) = run_args(&["report"]).await;
    assert_eq!(code, 2);
    assert!(
        err.contains("ARTIFACT"),
        "the error explains the missing positional:\n{err}",
    );
}

#[tokio::test]
async fn relative_artifact_resolves_against_the_injected_cwd() {
    let harness = TestIo::new(HashMap::new());
    write_apply_fixture(harness.dir.path(), "plan.json");
    let (code, _, err) = harness.run(&["apply", "plan.json"]).await;
    assert_eq!(
        code,
        0,
        "the artifact is found under the injected cwd: {}",
        err.contents()
    );
    assert!(
        err.contents().contains("apply: 1 applied"),
        "{}",
        err.contents()
    );

    // The same relative path is NOT found under the process cwd: the temp
    // dir is unique, so a join onto the process cwd would have missed it.
    let (code, _, _) = run_args(&["apply", "plan.json"]).await;
    assert_eq!(code, 2);
}

/// The source of the file the report artifact describes. It is never
/// written to disk: `report` works from the artifact alone.
const REPORT_SRC: &str = "\
// Adds one.
fn inc(x: i32) -> i32 {
    x + 1 // plus one
}
";

/// Writes a valid artifact describing `src/inc.rs` (absent from disk) at
/// `<dir>/<name>` through the real writer and `compute_summary`.
fn write_report_artifact(dir: &Path, name: &str) -> PathBuf {
    let inventory = inventory_file(
        REPORT_SRC,
        FileRef {
            language: LanguageId::Rust,
            file_name: "inc.rs".to_owned(),
        },
    )
    .expect("the fixture scans");
    let file = common::file_plan_from_inventory(
        "src/inc.rs",
        REPORT_SRC,
        &inventory,
        LanguageId::Rust,
        |comment| match comment.classification {
            Classification::Prose => (Decision::Remove, None),
            _ => (Decision::KeepProtected, None),
        },
    );
    let header = ArtifactHeader {
        created_at: "2026-10-09T14:03:11Z".to_owned(),
        root: ".".to_owned(),
        glob_base: ".".to_owned(),
        config: PlanConfig {
            mode: Mode::Remove,
            model: DEFAULT_MODEL.to_owned(),
            base_url: None,
            allow_add: false,
            similarity_threshold: 0.6,
            max_file_chars: 120_000,
            concurrency: 4,
            offline: true,
            paths: vec![".".to_owned()],
            include: None,
            exclude: Vec::new(),
        },
    };
    let path = dir.join(name);
    ArtifactWriter::new(&path, header, vec![file], compute_summary)
        .and_then(ArtifactWriter::finish)
        .expect("the artifact writes");
    path
}

/// [`write_report_artifact`] plus the source it describes on disk
/// (`<dir>/src/inc.rs`), so `apply` can run against it.
fn write_apply_fixture(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).expect("tests may write into their temp dir");
    std::fs::write(dir.join("src/inc.rs"), REPORT_SRC)
        .expect("tests may write into their temp dir");
    write_report_artifact(dir, name)
}

#[tokio::test]
async fn report_writes_the_markdown_to_an_absolute_out() {
    let harness = TestIo::new(HashMap::new());
    write_report_artifact(harness.dir.path(), "plan.json");
    assert!(
        !harness.dir.path().join("src").exists(),
        "the source tree is absent"
    );
    let out_dir = common::tempdir();
    let out = out_dir.path().join("report.md");
    let (code, stdout, err) = harness
        .run(&["report", "plan.json", "--out", out.to_str().unwrap()])
        .await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    assert_eq!(stdout.contents(), "", "status goes to stderr only");
    assert!(
        err.contents().contains("report: wrote"),
        "{}",
        err.contents()
    );
    let markdown = std::fs::read_to_string(&out).expect("the report exists");
    assert!(markdown.starts_with("# munge-comments report — 2026-10-09T14:03:11Z"));
    assert!(markdown.contains("## `src/inc.rs` (rust)"), "{markdown}");
    assert!(markdown.contains("| 2 | 2 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 100.0% | 100.0% |"));
}

#[tokio::test]
async fn report_defaults_out_under_the_injected_cwd() {
    // The cwd must outlive the run to inspect it, so no `TestIo` here.
    let cwd = common::tempdir();
    write_report_artifact(cwd.path(), "plan.json");
    let err = SharedBuf::new();
    let io = Io::new(
        Box::new(SharedBuf::new()),
        Box::new(err.clone()),
        cwd.path().to_path_buf(),
        HashMap::new(),
    );
    let code = run(["munge-comments", "report", "plan.json"], io).await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    assert!(cwd.path().join("munge-comments-report.md").is_file());
}

#[tokio::test]
async fn report_creates_a_missing_out_directory() {
    let harness = TestIo::new(HashMap::new());
    write_report_artifact(harness.dir.path(), "plan.json");
    let out_dir = common::tempdir();
    let out = out_dir.path().join("a").join("b").join("report.md");
    let (code, _, err) = harness
        .run(&["report", "plan.json", "--out", out.to_str().unwrap()])
        .await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    assert!(out.is_file());
}

#[tokio::test]
async fn report_resolves_a_relative_artifact_against_the_injected_cwd() {
    let harness = TestIo::new(HashMap::new());
    std::fs::create_dir(harness.dir.path().join("plans")).unwrap();
    write_report_artifact(&harness.dir.path().join("plans"), "p.json");
    let out_dir = common::tempdir();
    let out = out_dir.path().join("report.md");
    let (code, _, err) = harness
        .run(&["report", "plans/p.json", "--out", out.to_str().unwrap()])
        .await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    assert!(out.is_file());
}

#[tokio::test]
async fn report_with_a_missing_artifact_is_exit_two() {
    let out_dir = common::tempdir();
    let out = out_dir.path().join("report.md");
    let (code, _, err) =
        run_args(&["report", "missing.json", "--out", out.to_str().unwrap()]).await;
    assert_eq!(code, 2);
    assert!(err.contains("missing.json"), "{err}");
    assert!(!out.exists(), "no report for a missing artifact");
}

#[tokio::test]
async fn report_with_an_invalid_artifact_is_exit_two() {
    let harness = TestIo::new(HashMap::new());
    std::fs::write(harness.dir.path().join("plan.json"), "{}")
        .expect("tests may write into their temp dir");
    let (code, _, err) = harness.run(&["report", "plan.json"]).await;
    assert_eq!(code, 2);
    assert!(err.contents().contains("plan.json"), "{}", err.contents());
}

#[tokio::test]
async fn report_with_an_unwritable_out_is_exit_six() {
    let harness = TestIo::new(HashMap::new());
    write_report_artifact(harness.dir.path(), "plan.json");
    // The would-be parent directory is a regular file, so it can be
    // neither created nor written into (whatever the user's privileges).
    let out_dir = common::tempdir();
    let blocker = out_dir.path().join("not-a-dir");
    std::fs::write(&blocker, "").unwrap();
    let out = blocker.join("report.md");
    let (code, _, err) = harness
        .run(&["report", "plan.json", "--out", out.to_str().unwrap()])
        .await;
    assert_eq!(code, 6, "stderr: {}", err.contents());
}

#[tokio::test]
async fn cooperative_cancel_is_armed_for_plan_only() {
    // plan: armed.
    let harness = TestIo::new(HashMap::new());
    let cooperative = Arc::clone(&harness.io.cooperative_cancel);
    let (code, _, err) = harness.run(&["plan", "--offline"]).await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    assert!(
        cooperative.load(Ordering::Relaxed),
        "run must arm cooperative cancellation for plan",
    );

    // apply: left unset.
    let harness = TestIo::new(HashMap::new());
    let cooperative = Arc::clone(&harness.io.cooperative_cancel);
    write_apply_fixture(harness.dir.path(), "plan.json");
    let (code, _, _) = harness.run(&["apply", "plan.json"]).await;
    assert_eq!(code, 0);
    assert!(
        !cooperative.load(Ordering::Relaxed),
        "apply is not cooperatively cancellable",
    );

    // report: left unset.
    let harness = TestIo::new(HashMap::new());
    let cooperative = Arc::clone(&harness.io.cooperative_cancel);
    write_report_artifact(harness.dir.path(), "plan.json");
    let (code, _, _) = harness.run(&["report", "plan.json"]).await;
    assert_eq!(code, 0);
    assert!(
        !cooperative.load(Ordering::Relaxed),
        "report is not cooperatively cancellable",
    );
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn panic_canary_reports_internal_error_and_exits_seven() {
    let (code, out, err) = run_args(&["__panic"]).await;
    assert_eq!(code, 7);
    assert_eq!(out, "");
    assert!(
        err.contains("internal error") && err.contains("__panic canary invoked"),
        "the caught panic is reported on stderr:\n{err}",
    );
}

#[tokio::test]
async fn plan_help_documents_the_full_option_surface() {
    let (_, out, _) = run_args(&["plan", "--help"]).await;
    for expected in [
        "--mode",
        "redact",
        "remove",
        "--model",
        "MUNGE_COMMENTS_MODEL",
        "gpt-4.1",
        "--base-url",
        "OPENAI_BASE_URL",
        "https://api.openai.com/v1",
        "--api-key",
        "OPENAI_API_KEY",
        "--include",
        "**/*.{ts,tsx,mts,cts,js,jsx,mjs,cjs,py,go,java,c,h,cc,cpp,cxx,hpp,hh,rs}",
        "--exclude",
        "**/node_modules/**",
        "--concurrency",
        "4",
        "--max-file-chars",
        "120000",
        "--similarity-threshold",
        "0.6",
        "--allow-add",
        "--offline",
        "--resume",
        "--out",
        "munge-comments-plan.json",
        "--report",
        "munge-comments-plan-report.md",
    ] {
        assert!(
            out.contains(expected),
            "plan --help is missing {expected}:\n{out}",
        );
    }
}

#[tokio::test]
async fn apply_and_report_help_document_their_options() {
    let (_, out, _) = run_args(&["apply", "--help"]).await;
    for expected in [
        "--dry-run",
        "--only",
        "--report",
        "munge-comments-apply-report.md",
    ] {
        assert!(out.contains(expected), "apply --help is missing {expected}");
    }
    let (_, out, _) = run_args(&["report", "--help"]).await;
    for expected in ["--out", "munge-comments-report.md"] {
        assert!(
            out.contains(expected),
            "report --help is missing {expected}"
        );
    }
}

#[test]
fn config_precedence_is_flag_over_env_over_default() {
    let env = env_map(&[
        ("OPENAI_API_KEY", "env-key"),
        ("OPENAI_BASE_URL", "http://env.example/v1"),
        ("MUNGE_COMMENTS_MODEL", "env-model"),
    ]);

    // Flags win over the environment.
    let flagged = parse_plan(&[
        "--model",
        "flag-model",
        "--base-url",
        "http://flag.example/v1",
        "--api-key",
        "flag-key",
    ]);
    let resolved = resolve_plan(&flagged, &env).expect("valid flags resolve");
    assert_eq!(resolved.config.model, "flag-model");
    assert_eq!(
        resolved.config.base_url.as_deref(),
        Some("http://flag.example/v1"),
    );
    assert_eq!(resolved.api_key.as_deref(), Some("flag-key"));

    // The environment wins over defaults.
    let resolved = resolve_plan(&parse_plan(&[]), &env).expect("env-backed args resolve");
    assert_eq!(resolved.config.model, "env-model");
    assert_eq!(
        resolved.config.base_url.as_deref(),
        Some("http://env.example/v1"),
    );
    assert_eq!(resolved.api_key.as_deref(), Some("env-key"));

    // Defaults when neither flag nor environment is set.
    let resolved = resolve_plan(&parse_plan(&[]), &HashMap::new()).expect("bare args resolve");
    assert_eq!(resolved.config.model, DEFAULT_MODEL);
    assert_eq!(resolved.config.base_url, None);
    assert_eq!(resolved.api_key, None);
}

#[test]
fn repeatable_array_flags_take_one_value_each() {
    // `--exclude x src/` keeps `src/` as a positional (num_args(1) per
    // occurrence), and repeated flags accumulate.
    let args = parse_plan(&[
        "--exclude",
        "x",
        "src/",
        "--include",
        "a/**",
        "--include",
        "b/**",
    ]);
    assert_eq!(args.paths, ["src/"], "the positional must survive the flag");
    assert_eq!(args.exclude, ["x"]);
    assert_eq!(args.include, ["a/**", "b/**"]);

    let args = parse_plan(&["src/", "docs/"]);
    assert_eq!(args.paths, ["src/", "docs/"]);
}

#[test]
fn parsed_defaults_match_the_documented_surface() {
    let args = parse_plan(&[]);
    assert_eq!(args.mode, ModeArg::Redact);
    assert_eq!(args.concurrency, 4);
    assert_eq!(args.max_file_chars, 120_000);
    assert_eq!(args.similarity_threshold, 0.6);
    assert!(!args.allow_add);
    assert!(!args.offline);
    assert!(!args.resume);
    assert!(args.paths.is_empty());
    assert!(args.include.is_empty());
    assert!(args.exclude.is_empty());
}

#[tokio::test]
async fn builders_attach_llm_client_and_cancel_token() {
    let harness = TestIo::new(HashMap::new());
    let token = CancellationToken::new();
    token.cancel();
    let io = harness
        .io
        .with_llm(Arc::new(FakeLlm::new(Scenario::default())))
        .with_cancel(token);
    assert!(io.llm.is_some(), "with_llm attaches the client");
    assert!(
        io.cancel.is_cancelled(),
        "with_cancel injects the given token",
    );
    let cooperative = Arc::clone(&io.cooperative_cancel);
    // The injected client stands in for a key, and the injected (already
    // cancelled) token interrupts the run: exit 130.
    let code = run(["munge-comments", "plan"], io).await;
    assert_eq!(code, 130);
    assert!(cooperative.load(Ordering::Relaxed));
}

// ---------------------------------------------------------------------
// `apply`
// ---------------------------------------------------------------------

const APPLY_PY: &str = "def double(x):\n    # doubles the input\n    return x * 2\n";
const APPLY_PY_DONE: &str = "def double(x):\n    return x * 2\n";
const APPLY_TS: &str = "// says hello\nexport const hello = 'hi';\n";
const APPLY_TS_DONE: &str = "export const hello = 'hi';\n";

/// The default report name of `apply`.
const APPLY_REPORT: &str = "munge-comments-apply-report.md";

/// Runs `munge-comments <args>` with `cwd` as the injected cwd; returns
/// `(exit code, stdout, stderr)`.
async fn run_in(cwd: &Path, args: &[&str]) -> (u8, String, String) {
    let out = SharedBuf::new();
    let err = SharedBuf::new();
    let io = Io::new(
        Box::new(out.clone()),
        Box::new(err.clone()),
        cwd.to_path_buf(),
        HashMap::new(),
    );
    let argv = std::iter::once("munge-comments").chain(args.iter().copied());
    let code = run(argv, io).await;
    (code, out.contents(), err.contents())
}

/// The source text for a test file, by extension.
fn apply_source(path: &str) -> &'static str {
    if path.ends_with(".py") {
        APPLY_PY
    } else {
        APPLY_TS
    }
}

/// The text the plan turns [`apply_source`]`(path)` into.
fn applied_source(path: &str) -> &'static str {
    if path.ends_with(".py") {
        APPLY_PY_DONE
    } else {
        APPLY_TS_DONE
    }
}

/// Writes every `paths` entry under `src` and a remove-the-prose artifact
/// for them at `artifact` with source root `root`.
fn write_apply_repo(src: &Path, paths: &[&str], artifact: &Path, root: &str) {
    for path in paths {
        let abs = src.join(path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, apply_source(path)).unwrap();
    }
    let files = paths
        .iter()
        .map(|path| common::plan_on_disk(src, path, common::remove_prose))
        .collect();
    common::write_plan_artifact(artifact, root, files, false);
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[tokio::test]
async fn apply_writes_the_files_and_the_default_report_under_the_injected_cwd() {
    let cwd = common::tempdir();
    write_apply_repo(
        cwd.path(),
        &["a.py", "src/b.ts"],
        &cwd.path().join("plan.json"),
        ".",
    );
    let (code, out, err) = run_in(cwd.path(), &["apply", "plan.json"]).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "", "a real run prints nothing to stdout");
    assert!(err.contains("apply: 2 applied"), "{err}");
    assert_eq!(read(&cwd.path().join("a.py")), APPLY_PY_DONE);
    assert_eq!(read(&cwd.path().join("src/b.ts")), APPLY_TS_DONE);
    assert!(
        cwd.path().join(APPLY_REPORT).is_file(),
        "the default report lands in the injected cwd"
    );
}

#[tokio::test]
async fn apply_dry_run_diffs_go_to_the_injected_stdout_and_nothing_else_does() {
    let cwd = common::tempdir();
    write_apply_repo(
        cwd.path(),
        &["a.py", "src/b.ts"],
        &cwd.path().join("plan.json"),
        ".",
    );
    let (code, out, err) = run_in(cwd.path(), &["apply", "plan.json", "--dry-run"]).await;
    assert_eq!(code, 0, "{err}");
    let expected = "\
--- a/a.py
+++ b/a.py
@@ -1,3 +1,2 @@
 def double(x):
-    # doubles the input
     return x * 2
--- a/src/b.ts
+++ b/src/b.ts
@@ -1,2 +1,1 @@
-// says hello
 export const hello = 'hi';
";
    assert_eq!(out, expected);
    assert!(err.contains("apply: 2 dry-run"), "{err}");
    assert!(!err.contains("--- a/"), "no diff on stderr: {err}");
    assert_eq!(read(&cwd.path().join("a.py")), APPLY_PY);
    assert_eq!(read(&cwd.path().join("src/b.ts")), APPLY_TS);
    assert!(
        !cwd.path().join(APPLY_REPORT).exists(),
        "a dry run writes no report"
    );
}

#[tokio::test]
async fn apply_only_resolves_against_the_injected_cwd() {
    let tmp = common::tempdir();
    write_apply_repo(
        tmp.path(),
        &["pkg/a.py", "pkg/b.ts"],
        &tmp.path().join("plan.json"),
        ".",
    );
    let pkg = tmp.path().join("pkg");
    let (code, _, err) = run_in(&pkg, &["apply", "../plan.json", "--only", "a.py"]).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(read(&pkg.join("a.py")), APPLY_PY_DONE);
    assert_eq!(read(&pkg.join("b.ts")), APPLY_TS, "not selected");
    assert!(pkg.join(APPLY_REPORT).is_file());
    assert!(!tmp.path().join(APPLY_REPORT).exists());
}

#[tokio::test]
async fn apply_only_naming_no_artifact_file_is_exit_one() {
    let cwd = common::tempdir();
    write_apply_repo(cwd.path(), &["a.py"], &cwd.path().join("plan.json"), ".");
    let (code, _, err) = run_in(cwd.path(), &["apply", "plan.json", "--only", "b.py"]).await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("b.py"), "{err}");
    assert_eq!(read(&cwd.path().join("a.py")), APPLY_PY);
}

#[tokio::test]
async fn apply_maps_the_runs_exit_code_to_the_process_exit_code() {
    // tree-sitter-typescript parses `return /*\n  */ x;` as `return x;`,
    // so removing the comment changes the tree and the verifier rejects
    // the file (see `common::EXPECTED_VERIFIER_REJECTIONS`).
    let cwd = common::tempdir();
    let hazard = "export function f(x: number): number | undefined {\n  return /*\n  */ x;\n}\n";
    std::fs::write(cwd.path().join("hazard.ts"), hazard).unwrap();
    std::fs::write(cwd.path().join("a.py"), APPLY_PY).unwrap();
    let files = vec![
        common::plan_on_disk(cwd.path(), "a.py", common::remove_prose),
        common::plan_on_disk(cwd.path(), "hazard.ts", common::remove_prose),
    ];
    common::write_plan_artifact(&cwd.path().join("plan.json"), ".", files, false);
    let (code, _, err) = run_in(cwd.path(), &["apply", "plan.json"]).await;
    assert_eq!(code, 4, "{err}");
    assert!(err.contains("skipped-verify hazard.ts"), "{err}");
    assert_eq!(read(&cwd.path().join("hazard.ts")), hazard);
    assert_eq!(read(&cwd.path().join("a.py")), APPLY_PY_DONE);
}

// ---------------------------------------------------------------------
// `apply`: the partial-apply recovery hint
// ---------------------------------------------------------------------

/// Makes directories read-only for the guard's lifetime (restoring
/// `0o755` on drop, so the temp tree can be cleaned up even when a test
/// fails). A file in such a directory can be read but not replaced: the
/// atomic write cannot create its temp file there, so Phase B fails it as
/// `failed-io` while every other file is written.
#[cfg(unix)]
struct ReadOnlyDirs(Vec<PathBuf>);

#[cfg(unix)]
impl ReadOnlyDirs {
    /// `None` when file modes do not restrict this user (privileged
    /// users), where the precondition cannot be set up.
    fn new(dirs: Vec<PathBuf>) -> Option<ReadOnlyDirs> {
        use std::os::unix::fs::PermissionsExt;
        let guard = ReadOnlyDirs(dirs);
        for dir in &guard.0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            if std::fs::write(dir.join(".probe"), "").is_ok() {
                eprintln!("skipped: file modes do not restrict this user");
                return None;
            }
        }
        Some(guard)
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyDirs {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        for dir in &self.0 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        }
    }
}

/// One recovery-hint scenario; every path is relative to a fresh temp
/// directory unless stated otherwise.
#[cfg(unix)]
struct HintCase<'a> {
    /// The source root.
    src: &'a str,
    /// Where the artifact is written.
    artifact: &'a str,
    /// The artifact's `root`, relative to its directory.
    root: &'a str,
    /// The injected cwd.
    cwd: &'a str,
    /// The `[ARTIFACT]` argument (relative to `cwd`, or absolute).
    artifact_arg: &'a str,
    /// Files (relative to `src`) that are written by the first run.
    written: &'a [&'a str],
    /// Files (relative to `src`) whose directories are read-only during
    /// the first run.
    failing: &'a [&'a str],
    /// The hint the first run must print.
    expected_hint: &'a str,
}

/// Runs a [`HintCase`]: the first run writes `written`, fails `failing`
/// (exit 6) and prints the hint (log and report); the hint, split by the
/// shell and re-run through `run()` with the same cwd, then applies the
/// remaining files.
#[cfg(unix)]
async fn assert_hint_recovers(case: HintCase<'_>) {
    let tmp = common::tempdir();
    let src = tmp.path().join(case.src);
    let cwd = tmp.path().join(case.cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    let artifact = tmp.path().join(case.artifact);
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    let all: Vec<&str> = case.written.iter().chain(case.failing).copied().collect();
    write_apply_repo(&src, &all, &artifact, case.root);

    let mut dirs: Vec<PathBuf> = case
        .failing
        .iter()
        .map(|path| src.join(path).parent().unwrap().to_path_buf())
        .collect();
    dirs.dedup();
    let Some(guard) = ReadOnlyDirs::new(dirs) else {
        return;
    };
    let (code, _, err) = run_in(&cwd, &["apply", case.artifact_arg]).await;
    drop(guard);
    assert_eq!(code, 6, "{err}");
    for path in case.written {
        assert_eq!(read(&src.join(path)), applied_source(path), "{path}");
    }
    for path in case.failing {
        assert_eq!(read(&src.join(path)), apply_source(path), "{path}");
    }
    let marker = "to retry the failed ones run: ";
    let hint = err
        .lines()
        .find_map(|line| line.split_once(marker).map(|(_, hint)| hint))
        .unwrap_or_else(|| panic!("no hint logged:\n{err}"));
    assert_eq!(hint, case.expected_hint);
    let report = read(&cwd.join(APPLY_REPORT));
    assert!(
        report.contains(hint),
        "the report carries the hint:\n{report}"
    );

    // Re-run the hint as printed.
    let words = common::shell_words(hint);
    assert_eq!(words[..2], ["munge-comments", "apply"], "{words:?}");
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let (code, _, err) = run_in(&cwd, &args).await;
    assert_eq!(code, 0, "the hint re-runs cleanly: {err}");
    for path in all {
        assert_eq!(read(&src.join(path)), applied_source(path), "{path}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn the_recovery_hint_reruns_one_failed_file() {
    assert_hint_recovers(HintCase {
        src: ".",
        artifact: "plan.json",
        root: ".",
        cwd: ".",
        artifact_arg: "plan.json",
        written: &["a.py"],
        failing: &["ro/b.ts"],
        expected_hint: "munge-comments apply plan.json --only ro/b.ts",
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn the_recovery_hint_reruns_two_failed_files() {
    assert_hint_recovers(HintCase {
        src: ".",
        artifact: "plan.json",
        root: ".",
        cwd: ".",
        artifact_arg: "plan.json",
        written: &["a.py"],
        failing: &["ro/b.ts", "ro/c.py"],
        expected_hint: "munge-comments apply plan.json --only ro/b.ts --only ro/c.py",
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn the_recovery_hint_quotes_a_path_with_a_space() {
    assert_hint_recovers(HintCase {
        src: ".",
        artifact: "my plans/plan.json",
        root: "..",
        cwd: ".",
        artifact_arg: "my plans/plan.json",
        written: &["a.py"],
        failing: &["my dir/b.ts"],
        expected_hint: "munge-comments apply 'my plans/plan.json' --only 'my dir/b.ts'",
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn the_recovery_hint_works_from_a_cwd_other_than_a_dotdot_root() {
    assert_hint_recovers(HintCase {
        src: "src",
        artifact: "plans/plan.json",
        root: "../src",
        cwd: "work",
        artifact_arg: "../plans/plan.json",
        written: &["a.py"],
        failing: &["ro/b.ts"],
        expected_hint: "munge-comments apply ../plans/plan.json --only ../src/ro/b.ts",
    })
    .await;
}
