//! Reporting: run statistics and the markdown report.
//!
//! [`stats`] recomputes every number from the records (the artifact's
//! stored `summary` is informational and goes stale after hand edits) and
//! implements the single convergence metric; [`markdown`] renders an
//! artifact — optionally with an [`ApplyResult`](crate::apply::types::ApplyResult)
//! — to a self-contained markdown report. Neither needs the source tree.

pub mod markdown;
pub mod stats;
