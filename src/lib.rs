//! `munge_comments` — LLM-driven code comment pruning and regeneration,
//! across languages.
//!
//! All logic lives in this library crate; the `munge-comments` binary is a
//! thin shim over [`cli::run`]. Later phases add their own `pub mod` lines
//! and extend the supported-surface re-exports at the crate root.

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

pub mod comments;
pub mod config;
pub mod discover;
pub mod errors;
pub mod lang;
pub mod llm;
pub mod log;

#[cfg(feature = "cli")]
pub mod cli;

pub use comments::inventory::{FileRef, Inventory, InventoryComment, inventory_file};
pub use config::{Mode, PlanConfig};
pub use errors::{Error, LlmErrorReason};
pub use lang::{extensions::language_for, ids::LanguageId};
pub use log::{CapturingLog, Log, NullLog, WriterLog};
