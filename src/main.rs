//! Thin shim for the `munge-comments` binary.
//!
//! Builds the multi-threaded Tokio runtime, reads process state once via
//! [`Io::from_process`], installs the signal watcher (the only place signal
//! handlers are registered and the only place the process is hard-exited),
//! and maps [`cli::run`]'s exit code to the process exit status. Because
//! `std::process::exit` is never called outside the watcher's hard exits,
//! stdout always drains.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use munge_comments::cli::{self, Io};

fn main() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("failed to build tokio runtime: {err}");
            return ExitCode::from(7);
        }
    };
    let io = Io::from_process();
    rt.spawn(signal_watcher(
        io.cancel.clone(),
        Arc::clone(&io.cooperative_cancel),
    ));
    #[allow(clippy::disallowed_methods)] // the binary is the one sanctioned reader of argv
    let args = std::env::args_os();
    let code = rt.block_on(cli::run(args, io));
    ExitCode::from(code)
}

/// The signal watcher.
///
/// On the first signal (SIGINT via `ctrl_c`, or SIGTERM on Unix):
///
/// - during `plan` (cooperative cancellation armed by `cli::run`), the
///   cancellation token is cancelled — in-flight files finish and persist,
///   and the artifact stays valid-but-partial (continue with `--resume`) —
///   and a second signal hard-exits with 130;
/// - otherwise (`apply`, `report`, argument parsing), the process
///   hard-exits with 130 immediately, exactly like the OS default.
///   Registering the handler suppresses the OS default for every command,
///   which is why the non-cooperative path must exit explicitly.
async fn signal_watcher(cancel: CancellationToken, cooperative_cancel: Arc<AtomicBool>) {
    let mut signals = Signals::install();
    signals.wait().await;
    if !cooperative_cancel.load(Ordering::Relaxed) {
        hard_exit();
    }
    cancel.cancel();
    signals.wait().await;
    hard_exit();
}

/// Installed signal sources: SIGINT on every platform, plus SIGTERM on Unix.
///
/// The SIGTERM stream is created once, up front, so a signal delivered
/// between waits is still received by the next one.
enum Signals {
    #[cfg(unix)]
    Unix {
        sigterm: tokio::signal::unix::Signal,
    },
    #[cfg(not(unix))]
    Other,
}

impl Signals {
    /// Installs the handlers. A failure to register a signal handler is an
    /// internal invariant of the binary: report it and exit 7.
    #[allow(clippy::disallowed_methods)] // the binary owns signal handling
    fn install() -> Signals {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(sigterm) => Signals::Unix { sigterm },
                Err(err) => signal_setup_failed(&err),
            }
        }
        #[cfg(not(unix))]
        {
            Signals::Other
        }
    }

    /// Waits for the next signal.
    ///
    /// `futures::future::select` over `std::pin::pin!`-pinned futures rather
    /// than `tokio::select!`: the macro needs tokio's `macros` feature, which
    /// the pinned manifest deliberately omits from the runtime dependency
    /// set, and `select` itself requires `Unpin` futures.
    #[allow(clippy::disallowed_methods)] // the binary owns signal handling
    async fn wait(&mut self) {
        match self {
            #[cfg(unix)]
            Signals::Unix { sigterm } => {
                use futures::future::{Either, select};
                let ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
                let sigterm = std::pin::pin!(sigterm.recv());
                match select(ctrl_c, sigterm).await {
                    Either::Left((result, _)) => {
                        if let Err(err) = result {
                            signal_setup_failed(&err);
                        }
                    }
                    Either::Right(_) => {}
                }
            }
            #[cfg(not(unix))]
            Signals::Other => {
                if let Err(err) = tokio::signal::ctrl_c().await {
                    signal_setup_failed(&err);
                }
            }
        }
    }
}

/// Reports a signal-handler registration failure and exits 7 (internal
/// invariant).
fn signal_setup_failed(err: &std::io::Error) -> ! {
    eprintln!("internal error: failed to install a signal handler: {err}");
    #[allow(clippy::disallowed_methods)] // the binary's invariant hard exit
    std::process::exit(7)
}

/// Exit 130 (interrupted), as the OS default for SIGINT/SIGTERM would.
#[allow(clippy::disallowed_methods)] // the signal watcher owns the process's only hard exits
fn hard_exit() -> ! {
    std::process::exit(130)
}
