//! The comment layer: the scanner (complete comment enumeration with
//! anchors), the classifier (directive rows, doc policies, tag flags and
//! positional rules), the sanitization primitives, and the shared comment
//! types, and the inventory pipeline (the single entrypoint through which
//! every consumer obtains a file's classified comments). Hygiene and
//! transforms land in later tickets.

pub mod classify;
pub mod inventory;
pub mod sanitize;
pub mod scan;
pub mod types;
