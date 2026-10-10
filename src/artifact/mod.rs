//! The plan artifact: the durable, hand-editable JSON contract between
//! `plan` and `apply`.
//!
//! [`types`] holds the serde structs (the single source of truth for the
//! schema) and the validation refinements; [`read`] is the validation
//! boundary ([`read::read_artifact`] → [`types::ValidatedArtifact`]);
//! [`write`](mod@write) is the incremental, atomic
//! [`write::ArtifactWriter`];
//! [`validate`] holds hashing, decoding and root/path checks.

pub mod read;
pub mod types;
pub mod validate;
pub mod write;
