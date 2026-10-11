//! The decision engine: turns the inventory, the model's output and the
//! deterministic heuristics into final per-comment decisions.
//!
//! [`records`] holds the single record builder ([`build_draft_records`]),
//! the in-flight [`DraftRecord`] and its finalization into an artifact
//! [`CommentRecord`](crate::artifact::types::CommentRecord); [`heuristics`]
//! the similarity measure and the decision matrix's short-circuit layer
//! ([`pre_llm_decision`], [`route`]); [`align`] remove-mode anchor
//! resolution, citation ↔ original pairing and NEW-candidate validation
//! ([`align_remove_mode`]); [`regenerate`] the orchestration of call #1
//! for one file ([`regenerate_file`]); [`judge`] the orchestration of
//! call #2 and the post-judge rails ([`judge_file`]).

pub mod align;
pub mod heuristics;
pub mod judge;
pub mod records;
pub mod regenerate;

pub use align::{Alignment, NewCandidate, align_remove_mode};
pub use heuristics::{Route, pre_llm_decision, route, similarity, word_tokens};
pub use judge::{JudgeOutput, judge_file};
pub use records::{DraftRecord, build_draft_records, finalize_record};
pub use regenerate::{FileInput, MAX_SPLIT_DEPTH, RegenOutput, regenerate_file};
