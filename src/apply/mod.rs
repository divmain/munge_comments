//! The write path: replacement rendering, edit computation and
//! application, unified-diff rendering, semantic verification, and the
//! two-phase apply driver ([`run::run_apply`]) with its firewall.

pub mod diff;
pub mod edits;
pub mod render;
pub mod run;
pub mod stage;
pub mod types;
pub mod verify;
