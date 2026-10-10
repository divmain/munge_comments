//! The comment layer: the scanner (complete comment enumeration with
//! anchors), the classifier (directive rows, doc policies, tag flags and
//! positional rules), the sanitization primitives, and the shared comment
//! types, and the inventory pipeline (the single entrypoint through which
//! every consumer obtains a file's classified comments), the hiding
//! transforms (the blind and clean texts the LLM sees), and the shared
//! removal/line-hygiene engine.

pub mod classify;
pub mod hygiene;
pub mod inventory;
pub mod redact;
pub mod sanitize;
pub mod scan;
pub mod types;
