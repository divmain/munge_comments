//! CLI skeleton tests: parsing, exit codes, stream routing, config
//! resolution, and the panic canary — all through `cli::run` with injected
//! `SharedBuf` streams, never the process's own stdio or environment.

#![cfg(feature = "cli")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap::Parser;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use munge_comments::cli::{Cli, Command, Io, ModeArg, PlanArgs, run};
use munge_comments::config::{DEFAULT_MODEL, resolve_plan};
use munge_comments::llm::client::LlmClient;

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
async fn plan_reaches_the_stub_handler_and_exits_zero() {
    let (code, out, err) = run_args(&["plan"]).await;
    assert_eq!(code, 0);
    assert_eq!(out, "");
    assert!(err.contains("plan: not implemented"));
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
    std::fs::write(harness.dir.path().join("plan.json"), "{}")
        .expect("tests may write into their temp dir");
    let (code, _, err) = harness.run(&["apply", "plan.json"]).await;
    assert_eq!(code, 0, "the artifact is found under the injected cwd");
    assert!(err.contents().contains("apply: not implemented"));

    // The same relative path is NOT found under the process cwd: the temp
    // dir is unique, so a join onto the process cwd would have missed it.
    let (code, _, _) = run_args(&["apply", "plan.json"]).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn report_with_existing_artifact_reaches_the_handler() {
    let harness = TestIo::new(HashMap::new());
    std::fs::write(harness.dir.path().join("plan.json"), "{}")
        .expect("tests may write into their temp dir");
    let (code, _, err) = harness.run(&["report", "plan.json"]).await;
    assert_eq!(code, 0);
    assert!(err.contents().contains("report: not implemented"));
}

#[tokio::test]
async fn cooperative_cancel_is_armed_for_plan_only() {
    // plan: armed.
    let harness = TestIo::new(HashMap::new());
    let cooperative = Arc::clone(&harness.io.cooperative_cancel);
    let (code, _, _) = harness.run(&["plan"]).await;
    assert_eq!(code, 0);
    assert!(
        cooperative.load(Ordering::Relaxed),
        "run must arm cooperative cancellation for plan",
    );

    // apply: left unset.
    let harness = TestIo::new(HashMap::new());
    let cooperative = Arc::clone(&harness.io.cooperative_cancel);
    std::fs::write(harness.dir.path().join("plan.json"), "{}")
        .expect("tests may write into their temp dir");
    let (code, _, _) = harness.run(&["apply", "plan.json"]).await;
    assert_eq!(code, 0);
    assert!(
        !cooperative.load(Ordering::Relaxed),
        "apply is not cooperatively cancellable",
    );

    // report: left unset.
    let harness = TestIo::new(HashMap::new());
    let cooperative = Arc::clone(&harness.io.cooperative_cancel);
    std::fs::write(harness.dir.path().join("plan.json"), "{}")
        .expect("tests may write into their temp dir");
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
    struct NoClient;
    impl LlmClient for NoClient {}

    let harness = TestIo::new(HashMap::new());
    let token = CancellationToken::new();
    token.cancel();
    let io = harness.io.with_llm(Arc::new(NoClient)).with_cancel(token);
    assert!(io.llm.is_some(), "with_llm attaches the client");
    assert!(
        io.cancel.is_cancelled(),
        "with_cancel injects the given token",
    );
    let cooperative = Arc::clone(&io.cooperative_cancel);
    let code = run(["munge-comments", "plan"], io).await;
    assert_eq!(code, 0);
    assert!(cooperative.load(Ordering::Relaxed));
}
