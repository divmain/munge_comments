//! `munge_comments` — LLM-driven code comment pruning and regeneration,
//! across languages.
//!
//! All logic lives in this library crate; the `munge-comments` binary is a
//! thin shim over `cli::run` (behind the default `cli` feature). Every
//! module is `pub`; the re-exports below are the supported surface (the
//! crate is 0.x semver). [`run_plan`] is async and needs a Tokio runtime;
//! everything else, [`run_apply`] included, is synchronous.

#![cfg_attr(
    not(test),
    deny(
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::dbg_macro,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::exit,
    )
)]

pub mod apply;
pub mod artifact;
pub mod comments;
pub mod config;
pub mod discover;
pub mod engine;
pub mod errors;
pub mod lang;
pub mod llm;
pub mod log;
pub mod report;
pub mod util;

#[cfg(feature = "cli")]
pub mod cli;

pub use apply::run::{ApplyDeps, ApplyHooks, ApplyOutcome, ApplyRequest, run_apply};
pub use apply::types::{ApplyFileResult, ApplyResult, ApplyStatus};
pub use apply::verify::{FunctionalComment, functional_sequence, verify_semantic_equality};
pub use artifact::read::read_artifact;
pub use artifact::types::{
    CommentRecord, Decision, Difficulty, FilePlan, PlanArtifact, Summary, ValidatedArtifact,
};
pub use comments::inventory::{FileRef, Inventory, InventoryComment, inventory_file};
pub use config::{Mode, PlanConfig};
pub use engine::plan::{PlanDeps, PlanHooks, PlanOutcome, PlanRequest, run_plan};
pub use engine::{DraftRecord, Route, build_draft_records, finalize_record, route, similarity};
pub use errors::{Error, LlmErrorReason};
pub use lang::{extensions::language_for, ids::LanguageId};
pub use llm::client::{LlmClient, OpenAiCompatClient, OpenAiCompatConfig, ProxyMode, RetryPolicy};
pub use llm::fake::{FakeLlm, Scenario};
pub use log::{CapturingLog, Log, NullLog, WriterLog};
pub use report::markdown::render_markdown;
pub use report::stats::{compute_summary, convergence_pct};
/// `PlanDeps::cancel`'s type, re-exported so consumers need no direct
/// `tokio-util` dependency.
pub use tokio_util::sync::CancellationToken;
