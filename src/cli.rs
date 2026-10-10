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

use crate::config::resolve_plan;
use crate::errors::Error;
use crate::llm::client::LlmClient;

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
    /// Cancellation token, passed to `plan` by later phases.
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

    /// Builder: attach an LLM client (used by later phases and by tests).
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
/// The version string is `munge-comments <CARGO_PKG_VERSION>`, so it cannot
/// drift from `Cargo.toml`.
#[derive(Parser, Debug)]
#[command(
    name = "munge-comments",
    bin_name = "munge-comments",
    version,
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

/// `--mode` values (the clap-side mirror of [`Mode`]; kept out of
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
///
/// The Phase-1 handlers are wired stubs: they validate their inputs for real
/// (config resolution, artifact existence), then report `not implemented` on
/// the injected stderr and return 0. Later phases replace the bodies with
/// the real pipelines.
async fn dispatch(command: Command, io: &mut Io) -> Result<u8, Error> {
    match command {
        Command::Plan(args) => {
            // `plan` alone is cooperatively interruptible; the signal
            // watcher reads this flag on the first signal.
            io.cooperative_cancel.store(true, Ordering::Relaxed);
            let _resolved = resolve_plan(&args, &io.env)?;
            let _ = writeln!(io.stderr, "plan: not implemented");
            Ok(0)
        }
        Command::Apply(args) => {
            let _artifact = resolve_artifact(&args.artifact, io)?;
            let _ = writeln!(io.stderr, "apply: not implemented");
            Ok(0)
        }
        Command::Report(args) => {
            let _artifact = resolve_artifact(&args.artifact, io)?;
            let _ = writeln!(io.stderr, "report: not implemented");
            Ok(0)
        }
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

/// Joins the `[ARTIFACT]` positional onto the injected cwd before any
/// existence check (the process cwd is never consulted), and errors with
/// [`Error::Artifact`] when the argument is missing or names no file.
fn resolve_artifact(artifact: &Option<PathBuf>, io: &Io) -> Result<PathBuf, Error> {
    let Some(artifact) = artifact else {
        return Err(Error::Artifact(
            "missing [ARTIFACT] argument: expected a munge-comments plan artifact".to_string(),
        ));
    };
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
