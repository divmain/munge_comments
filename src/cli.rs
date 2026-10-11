//! Command-line interface for `munge-comments`.
//!
//! [`run`] is the only place errors become exit codes: it parses with
//! `Cli::try_parse_from` (parsing never exits the process), maps every
//! [`Error`] to its exit code with the `Display` text on the injected
//! stderr, and converts a caught panic into `internal error: <message>` plus
//! exit 7. All ambient state arrives through [`Io`]; the single ambient read
//! in the library is [`Io::from_process`], which exists for the binary.
//!
//! Help output is deterministic: fixed `name`/`bin_name`, `term_width(100)`,
//! never colored. Env-backed options carry no clap defaults — resolution
//! happens in [`crate::config::resolve_plan`] against the injected
//! environment — and their help strings state the env var and default by
//! hand. Array-valued flags take one value per occurrence, so a following
//! positional is never swallowed.

use std::any::Any;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{ArgAction, Args, ColorChoice, Parser, Subcommand, ValueEnum};
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use crate::apply::run::{ApplyDeps, ApplyHooks, ApplyRequest, run_apply};
use crate::artifact::read::read_artifact;
use crate::config::{DEFAULT_BASE_URL, ResolvedPlan, resolve_plan};
use crate::engine::plan::{PlanDeps, PlanHooks, PlanRequest, run_plan};
use crate::errors::Error;
use crate::llm::client::{LlmClient, OpenAiCompatClient, OpenAiCompatConfig, ProxyMode};
use crate::log::WriterLog;
use crate::report::markdown::render_markdown;
use crate::util::fs::{atomic_write, ensure_parent_dir};

/// Injected process state: the streams commands write to, the directory
/// relative paths resolve against, the environment consulted for
/// configuration, the LLM client, and the cancellation plumbing for
/// interruptible `plan` runs.
///
/// Tests construct `Io` explicitly; the binary builds it once via
/// [`Io::from_process`].
pub struct Io {
    /// Where command artifacts (`--help`/`--version` text, `apply --dry-run`
    /// diffs) are written.
    pub stdout: Box<dyn Write + Send>,
    /// Where diagnostics (errors, progress) are written.
    pub stderr: Box<dyn Write + Send>,
    /// The directory relative paths resolve against (never the process
    /// cwd).
    pub cwd: PathBuf,
    /// The environment consulted by configuration resolution.
    pub env: HashMap<String, String>,
    /// The LLM client, when one has been constructed.
    pub llm: Option<Arc<dyn LlmClient>>,
    /// Cancellation token, passed to `plan` (the signal watcher cancels it).
    pub cancel: CancellationToken,
    /// Whether the running command is cooperatively cancellable (`plan`);
    /// set by [`run`] just before dispatching `plan`. The binary's signal
    /// watcher reads it to decide between cancelling and a hard exit.
    pub cooperative_cancel: Arc<AtomicBool>,
}

impl Io {
    /// New `Io` with no LLM client, a fresh cancellation token, and
    /// cooperative cancellation off (it is turned on for `plan`).
    pub fn new(
        stdout: Box<dyn Write + Send>,
        stderr: Box<dyn Write + Send>,
        cwd: PathBuf,
        env: HashMap<String, String>,
    ) -> Io {
        Io {
            stdout,
            stderr,
            cwd,
            env,
            llm: None,
            cancel: CancellationToken::new(),
            cooperative_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Builder: attach an LLM client (`plan` uses it instead of building one;
    /// tests inject a fake).
    pub fn with_llm(mut self, llm: Arc<dyn LlmClient>) -> Io {
        self.llm = Some(llm);
        self
    }

    /// Builder: use this cancellation token instead of a fresh one (used by
    /// tests to observe cancellation).
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Io {
        self.cancel = cancel;
        self
    }

    /// The one sanctioned ambient read in the library: reads the process
    /// environment (non-UTF-8 pairs skipped, never panicked on), the process
    /// cwd, and the real stdout and stderr. Exists for the binary shim;
    /// everything else receives `Io` explicitly.
    #[allow(clippy::disallowed_methods)]
    pub fn from_process() -> Io {
        let env = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.into_string().ok()?;
                let value = value.into_string().ok()?;
                Some((key, value))
            })
            .collect();
        // A cwd that cannot be determined degenerates to "." so that
        // relative joins still behave like the process cwd would have.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Io::new(
            Box::new(std::io::stdout()),
            Box::new(std::io::stderr()),
            cwd,
            env,
        )
    }
}

/// Parses `--similarity-threshold`: any float parses, but only values in
/// `0..=1` are accepted.
///
/// clap 4.6 has a ranged value parser for integers but none for floats, so
/// the range check lives here; the message mirrors clap's own style
/// ("`… is not in 0..=1`").
fn parse_similarity_threshold(value: &str) -> Result<f64, String> {
    let parsed: f64 = value
        .parse()
        .map_err(|_| format!("'{value}' is not a number"))?;
    if (0.0..=1.0).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(format!("{parsed} is not in 0..=1"))
    }
}

/// The `munge-comments` command line.
///
/// The version string is `munge-comments <CARGO_PKG_VERSION>` and the help
/// description is `CARGO_PKG_DESCRIPTION`, so neither can drift from
/// `Cargo.toml`. Both `about` and `long_about` are set explicitly so this
/// doc comment stays rustdoc-only and never leaks into `--help`.
#[derive(Parser, Debug)]
#[command(
    name = "munge-comments",
    bin_name = "munge-comments",
    version,
    about,
    long_about = None,
    term_width = 100,
    color = ColorChoice::Never,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The three subcommands (plus the hidden debug canary).
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Plan comment changes: discover files, hide prose comments, have the
    /// model regenerate them, and judge each one. Writes a JSON artifact.
    Plan(PlanArgs),
    /// Apply a plan artifact to the codebase (verified, atomic writes).
    Apply(ApplyArgs),
    /// Render a plan artifact as a markdown report, without the sources.
    Report(ReportArgs),
    /// Panic immediately: the deliberate canary proving `run`'s
    /// panic-to-exit-7 path. Hidden, and debug builds only, so it never
    /// ships in `cargo install` (release) builds.
    #[cfg(debug_assertions)]
    #[command(name = "__panic", hide = true)]
    Panic,
}

/// `--mode` values (the clap-side mirror of [`Mode`](crate::config::Mode); kept out of
/// `config.rs` so the always-compiled config module never sees clap types).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    /// Replace each prose comment with a stable-id placeholder.
    Redact,
    /// Delete target prose comments outright.
    Remove,
}

/// `munge-comments plan` arguments.
#[derive(Args, Clone, Debug)]
pub struct PlanArgs {
    /// Files or directories to process (default: the current directory).
    #[arg(value_name = "PATHS")]
    pub paths: Vec<String>,

    /// Hiding strategy for prose comments (default: redact)
    #[arg(long, value_name = "MODE", value_enum, default_value = "redact")]
    pub mode: ModeArg,

    /// LLM model (env: MUNGE_COMMENTS_MODEL; default: gpt-4.1)
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// OpenAI-compatible endpoint (env: OPENAI_BASE_URL; default: https://api.openai.com/v1)
    // clap renders this doc comment verbatim as `--help` text, where
    // rustdoc's `<…>` autolink syntax would show up as literal brackets.
    #[allow(rustdoc::bare_urls)]
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// API key for the endpoint (env: OPENAI_API_KEY)
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Include glob, repeatable (default: **/*.{ts,tsx,mts,cts,js,jsx,mjs,cjs,py,go,java,c,h,cc,cpp,cxx,hpp,hh,rs})
    #[arg(long, value_name = "GLOB", action = ArgAction::Append, num_args = 1)]
    pub include: Vec<String>,

    /// Extra exclude glob, repeatable (defaults always applied: **/*.{d.ts,d.mts,d.cts}, **/*.pyi, **/*.min.js, **/node_modules/**, **/vendor/**, **/{dist,build,out}/**)
    #[arg(long, value_name = "GLOB", action = ArgAction::Append, num_args = 1)]
    pub exclude: Vec<String>,

    /// Parallel LLM files (default: 4)
    #[arg(
        long,
        value_name = "N",
        default_value = "4",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub concurrency: u64,

    /// Chunking threshold in characters (default: 120000)
    #[arg(
        long,
        value_name = "N",
        default_value = "120000",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub max_file_chars: u64,

    /// Heuristic redundancy cutoff (default: 0.6)
    #[arg(
        long,
        value_name = "0..1",
        default_value = "0.6",
        value_parser = parse_similarity_threshold
    )]
    pub similarity_threshold: f64,

    /// Also apply NEW comments (remove-mode files only, including per-file
    /// remove-mode fallbacks)
    #[arg(long)]
    pub allow_add: bool,

    /// No LLM calls; heuristic-only decisions (demo/no-endpoint path)
    #[arg(long)]
    pub offline: bool,

    /// Skip files already complete or skipped (unchanged hash) in the --out
    /// artifact; retry failed ones
    #[arg(long)]
    pub resume: bool,

    /// Artifact path (default: munge-comments-plan.json)
    #[arg(long, value_name = "FILE", default_value = "munge-comments-plan.json")]
    pub out: PathBuf,

    /// Post-plan markdown report (default: munge-comments-plan-report.md)
    #[arg(
        long,
        value_name = "PATH",
        default_value = "munge-comments-plan-report.md"
    )]
    pub report: PathBuf,
}

/// `munge-comments apply` arguments.
#[derive(Args, Clone, Debug)]
pub struct ApplyArgs {
    /// Path to the plan artifact.
    #[arg(value_name = "ARTIFACT")]
    pub artifact: Option<PathBuf>,

    /// Print unified diffs to stdout; write nothing (no report either)
    #[arg(long)]
    pub dry_run: bool,

    /// Apply only this file from the artifact, repeatable
    #[arg(long, value_name = "PATH", action = ArgAction::Append, num_args = 1)]
    pub only: Vec<String>,

    /// Post-apply markdown (default: munge-comments-apply-report.md)
    #[arg(
        long,
        value_name = "PATH",
        default_value = "munge-comments-apply-report.md"
    )]
    pub report: PathBuf,
}

/// `munge-comments report` arguments.
#[derive(Args, Clone, Debug)]
pub struct ReportArgs {
    /// Path to the plan artifact.
    #[arg(value_name = "ARTIFACT")]
    pub artifact: Option<PathBuf>,

    /// Output markdown file (default: munge-comments-report.md)
    #[arg(long, value_name = "FILE", default_value = "munge-comments-report.md")]
    pub out: PathBuf,
}

/// Runs the CLI for the given arguments.
///
/// `args` mirrors `argv`, `argv[0]` included. Errors become their
/// [`Error::exit_code`] with the `Display` text on the injected stderr; a
/// panic becomes `internal error: <message>` and exit 7. `plan` sets
/// [`Io::cooperative_cancel`] just before dispatching, which the binary's
/// signal watcher consults.
pub async fn run<I, T>(args: I, mut io: Io) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => return report_parse_error(&err, &mut io),
    };
    let outcome = AssertUnwindSafe(dispatch(cli.command, &mut io))
        .catch_unwind()
        .await;
    match outcome {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => {
            let _ = writeln!(io.stderr, "{err}");
            err.exit_code()
        }
        Err(payload) => {
            let _ = writeln!(io.stderr, "internal error: {}", panic_message(payload));
            7
        }
    }
}

/// Maps a parse outcome per the CLI contract:
///
/// - `--help`/`--version`: the rendered text goes to the injected stdout and
///   `run` returns 0;
/// - a bare invocation with no subcommand: help goes to stderr, exit 1;
/// - any other clap error (unknown flag, invalid value, missing argument):
///   the rendered error goes to stderr, exit 1.
///
/// Parsing never exits the process and never reads the ambient terminal.
fn report_parse_error(err: &clap::error::Error, io: &mut Io) -> u8 {
    use clap::error::ErrorKind;
    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = write!(io.stdout, "{}", err.render());
            0
        }
        // Bare `munge-comments` (missing subcommand) renders help text, and
        // every other parse error renders an error message; both go to
        // stderr with exit 1.
        _ => {
            let _ = write!(io.stderr, "{}", err.render());
            1
        }
    }
}

/// Dispatches a parsed command.
async fn dispatch(command: Command, io: &mut Io) -> Result<u8, Error> {
    match command {
        Command::Plan(args) => {
            // `plan` alone is cooperatively interruptible; the flag is set before dispatching so the signal watcher cancels on the first signal instead of hard-exiting.
            io.cooperative_cancel.store(true, Ordering::Relaxed);
            run_plan_command(&args, io).await
        }
        Command::Apply(args) => run_apply_command(args, io),
        Command::Report(args) => run_report(&args, io),
        #[cfg(debug_assertions)]
        Command::Panic => canary_panic(),
    }
}

/// The one sanctioned deliberate panic in the library: the `__panic`
/// canary proving run()'s panic-to-exit-7 path. A scoped allow is required
/// (the crate denies `clippy::panic` in non-test builds), mirroring the
/// `disallowed-methods` allow-list pattern.
#[cfg(debug_assertions)]
#[allow(clippy::panic)]
fn canary_panic() -> ! {
    panic!("__panic canary invoked");
}

/// The stderr note of an interrupted `plan` run.
pub const INTERRUPTED_NOTE: &str = "interrupted — continue with --resume";

/// `plan`: a thin wrapper over [`run_plan`]. Configuration is resolved
/// against the injected environment, and `--out`/`--report` (defaults
/// included) are joined onto the injected cwd, which `run_plan` also
/// resolves the inputs against. The client is the injected [`Io::llm`] if
/// set, else one built by [`build_llm_client`]; under `--offline` none is
/// used. The run's exit code becomes the process exit code (130, after the
/// interrupt note, when the run was cancelled); run-level failures surface
/// as errors.
async fn run_plan_command(args: &PlanArgs, io: &mut Io) -> Result<u8, Error> {
    let resolved = resolve_plan(args, &io.env)?;
    let llm = match &io.llm {
        Some(llm) if !resolved.config.offline => Some(Arc::clone(llm)),
        _ => build_llm_client(&resolved)?,
    };
    let request = PlanRequest {
        out: io.cwd.join(&resolved.out),
        report_path: Some(io.cwd.join(&resolved.report_path)),
        resume: resolved.resume,
        config: resolved.config,
    };
    let cancel = io.cancel.clone();
    let cwd = io.cwd.clone();
    let outcome = {
        let log = WriterLog::new(&mut io.stderr);
        run_plan(
            request,
            PlanDeps {
                log: &log,
                cwd: &cwd,
                llm,
                cancel,
                hooks: PlanHooks::default(),
            },
        )
        .await?
    };
    if outcome.aborted {
        let _ = writeln!(io.stderr, "{INTERRUPTED_NOTE}");
    }
    Ok(outcome.exit_code)
}

/// `apply`: a thin wrapper over [`run_apply`]. The `[ARTIFACT]` argument
/// is passed through as given — `run_apply` joins it (and the `--only` and
/// `--report` paths) onto the injected cwd, and its recovery hint repeats
/// it verbatim. The report goes to `--report` (default
/// `munge-comments-apply-report.md` under the cwd) unless `--dry-run`,
/// whose diffs go to the injected stdout; progress and warnings go to the
/// injected stderr. The run's per-file exit code becomes the process exit
/// code; run-level aborts surface as errors.
fn run_apply_command(args: ApplyArgs, io: &mut Io) -> Result<u8, Error> {
    let artifact = artifact_arg(&args.artifact)?.clone();
    let report_path = (!args.dry_run).then_some(args.report);
    let log = WriterLog::new(&mut io.stderr);
    let outcome = run_apply(
        ApplyRequest {
            artifact,
            only: args.only,
            dry_run: args.dry_run,
            report_path,
        },
        ApplyDeps {
            log: &log,
            stdout: &mut io.stdout,
            cwd: &io.cwd,
            hooks: ApplyHooks::default(),
        },
    )?;
    Ok(outcome.exit_code)
}

/// `report`: reads and validates the artifact, renders it (from the
/// artifact alone; the source tree need not exist) and atomically writes
/// the markdown to `--out`, joined onto the injected cwd, creating its
/// parent directory first. The status line goes to stderr.
///
/// A missing or invalid artifact is exit 2; an `--out` that cannot be
/// created or written is exit 6.
fn run_report(args: &ReportArgs, io: &mut Io) -> Result<u8, Error> {
    let artifact_path = resolve_artifact(&args.artifact, io)?;
    let artifact = read_artifact(&artifact_path)?;
    let markdown = render_markdown(&artifact, None)?;
    let out = io.cwd.join(&args.out);
    let Some(file_name) = out.file_name() else {
        return Err(Error::Usage(format!(
            "--out must name a file, got {}",
            args.out.display()
        )));
    };
    let out = ensure_parent_dir(&out)?.join(file_name);
    atomic_write(&out, markdown.as_bytes())?;
    let _ = writeln!(io.stderr, "report: wrote {}", out.display());
    Ok(0)
}

/// Joins the `[ARTIFACT]` positional onto the injected cwd before any
/// existence check (the process cwd is never consulted), and errors with
/// [`Error::Artifact`] when the argument is missing or names no file.
fn resolve_artifact(artifact: &Option<PathBuf>, io: &Io) -> Result<PathBuf, Error> {
    let artifact = artifact_arg(artifact)?;
    let joined = io.cwd.join(artifact);
    if joined.is_file() {
        Ok(joined)
    } else {
        Err(Error::Artifact(format!(
            "artifact not found: {}",
            artifact.display()
        )))
    }
}

/// The `[ARTIFACT]` positional; its absence is [`Error::Artifact`].
fn artifact_arg(artifact: &Option<PathBuf>) -> Result<&PathBuf, Error> {
    artifact.as_ref().ok_or_else(|| {
        Error::Artifact(
            "missing [ARTIFACT] argument: expected a munge-comments plan artifact".to_string(),
        )
    })
}

/// Builds the `plan` command's LLM client from resolved configuration,
/// applying the API-key rule:
///
/// - under `--offline` no client is constructed (`Ok(None)`);
/// - a missing key is a usage error (exit 1) only when no base URL was
///   resolved (neither `--base-url` nor `OPENAI_BASE_URL`);
/// - with a base URL and no key (local OpenAI-compatible servers), the
///   client sends `Bearer unused`.
///
/// An empty key counts as missing. The client honors the user's proxy
/// settings ([`ProxyMode::System`]): a command-line tool should behave like
/// other network tools in the user's shell.
pub fn build_llm_client(resolved: &ResolvedPlan) -> Result<Option<Arc<dyn LlmClient>>, Error> {
    match llm_client_config(resolved)? {
        None => Ok(None),
        Some(config) => {
            let client: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(config)?);
            Ok(Some(client))
        }
    }
}

/// The client configuration [`build_llm_client`] uses (`None` under
/// `--offline`); split out so the API-key rule is testable without
/// constructing a client.
fn llm_client_config(resolved: &ResolvedPlan) -> Result<Option<OpenAiCompatConfig>, Error> {
    if resolved.config.offline {
        return Ok(None);
    }
    let api_key = resolved.api_key.clone().filter(|key| !key.is_empty());
    let base_url = match (&resolved.config.base_url, &api_key) {
        (Some(base_url), _) => base_url.clone(),
        (None, Some(_)) => DEFAULT_BASE_URL.to_string(),
        (None, None) => {
            return Err(Error::Usage(
                "no API key: set OPENAI_API_KEY or pass --api-key (a local endpoint given via \
                 --base-url or OPENAI_BASE_URL needs none; --offline needs no endpoint)"
                    .to_string(),
            ));
        }
    };
    Ok(Some(OpenAiCompatConfig {
        base_url,
        api_key,
        proxy: ProxyMode::System,
        ..OpenAiCompatConfig::default()
    }))
}

/// Best-effort extraction of a panic message from `catch_unwind`'s payload.
///
/// Takes the payload box by value and dereferences it explicitly: coercing
/// `&Box<dyn Any + Send>` to `&dyn Any` would make the reference point at
/// the *box itself* (whose concrete type is the box), so the `&str`/`String`
/// downcasts would never match. `&*payload` points at the box's contents.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    let payload = &*payload;
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves `munge-comments plan <flags>` against `env`.
    fn resolved(flags: &[&str], env: &[(&str, &str)]) -> ResolvedPlan {
        let argv = ["munge-comments", "plan"].iter().chain(flags).copied();
        let Command::Plan(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected the plan subcommand");
        };
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        resolve_plan(&args, &env).unwrap()
    }

    #[test]
    fn offline_constructs_no_client_even_without_a_key() {
        let plan = resolved(&["--offline"], &[]);
        assert!(llm_client_config(&plan).unwrap().is_none());
        assert!(build_llm_client(&plan).unwrap().is_none());
    }

    #[test]
    fn missing_key_without_base_url_is_a_usage_error() {
        let plan = resolved(&[], &[]);
        let err = llm_client_config(&plan).unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert_eq!(err.exit_code(), 1);
        let err = build_llm_client(&plan).err().unwrap();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn empty_key_counts_as_missing() {
        let plan = resolved(&["--api-key", ""], &[]);
        assert_eq!(llm_client_config(&plan).unwrap_err().exit_code(), 1);
        let plan = resolved(&[], &[("OPENAI_API_KEY", "")]);
        assert_eq!(llm_client_config(&plan).unwrap_err().exit_code(), 1);
    }

    #[test]
    fn base_url_from_env_without_key_sends_no_key() {
        let plan = resolved(&[], &[("OPENAI_BASE_URL", "http://localhost:8080/v1")]);
        let config = llm_client_config(&plan).unwrap().unwrap();
        assert_eq!(config.base_url, "http://localhost:8080/v1");
        assert_eq!(config.api_key, None, "the transport sends `Bearer unused`");
        assert_eq!(config.proxy, ProxyMode::System);
    }

    #[test]
    fn base_url_from_flag_without_key_sends_no_key() {
        let plan = resolved(&["--base-url", "http://127.0.0.1:11434/v1"], &[]);
        let config = llm_client_config(&plan).unwrap().unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(config.api_key, None);
        assert!(build_llm_client(&plan).unwrap().is_some());
    }

    #[test]
    fn key_without_base_url_targets_the_default_endpoint() {
        let plan = resolved(&[], &[("OPENAI_API_KEY", "sk-test")]);
        let config = llm_client_config(&plan).unwrap().unwrap();
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.proxy, ProxyMode::System);
        assert_eq!(
            config.request_timeout,
            OpenAiCompatConfig::default().request_timeout
        );
        assert_eq!(config.retry, OpenAiCompatConfig::default().retry);
        // Construction only: no request is made.
        assert!(build_llm_client(&plan).unwrap().is_some());
    }

    #[test]
    fn key_and_base_url_are_both_used() {
        let plan = resolved(
            &[
                "--api-key",
                "sk-flag",
                "--base-url",
                "https://llm.example/v1",
            ],
            &[("OPENAI_API_KEY", "sk-env")],
        );
        let config = llm_client_config(&plan).unwrap().unwrap();
        assert_eq!(config.base_url, "https://llm.example/v1");
        assert_eq!(config.api_key.as_deref(), Some("sk-flag"));
    }

    #[test]
    fn invalid_base_url_is_a_usage_error() {
        let plan = resolved(&["--base-url", "not a url"], &[]);
        let err = build_llm_client(&plan).err().unwrap();
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
    }
}
