//! `LanguageId` — the dependency-free language identifier.
//!
//! Serde names are the adapter ids (`typescript`, `tsx`, `javascript`,
//! `python`, `go`, `java`, `c`, `cpp`, `rust`), which is what the artifact
//! stores and what `report` renders. The enum carries no parser state: the
//! grammar registry (a later module) maps ids to grammars, so the artifact
//! schema and reporting never depend on it.

use serde::{Deserialize, Serialize};

/// One supported language. Variant order is significant: it defines
/// [`ALL`](LanguageId::ALL) and the index used for per-language compiled
/// caches (guarded by a unit test).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum LanguageId {
    /// TypeScript (`.ts`, `.mts`, `.cts`).
    Typescript,
    /// TSX (`.tsx`).
    Tsx,
    /// JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`).
    Javascript,
    /// Python (`.py`).
    Python,
    /// Go (`.go`).
    Go,
    /// Java (`.java`).
    Java,
    /// C (`.c`).
    C,
    /// C++ (`.h`, `.cc`, `.cpp`, `.cxx`, `.hpp`, `.hh` — and `.cpp` itself).
    Cpp,
    /// Rust (`.rs`).
    Rust,
}

impl LanguageId {
    /// Every supported language, in variant order.
    pub const ALL: [LanguageId; 9] = [
        LanguageId::Typescript,
        LanguageId::Tsx,
        LanguageId::Javascript,
        LanguageId::Python,
        LanguageId::Go,
        LanguageId::Java,
        LanguageId::C,
        LanguageId::Cpp,
        LanguageId::Rust,
    ];

    /// The adapter id / serde name of this language.
    pub const fn as_str(self) -> &'static str {
        match self {
            LanguageId::Typescript => "typescript",
            LanguageId::Tsx => "tsx",
            LanguageId::Javascript => "javascript",
            LanguageId::Python => "python",
            LanguageId::Go => "go",
            LanguageId::Java => "java",
            LanguageId::C => "c",
            LanguageId::Cpp => "cpp",
            LanguageId::Rust => "rust",
        }
    }

    /// This language's position in [`ALL`](LanguageId::ALL). Also the index
    /// into per-language slot arrays (compiled patterns); a unit test pins
    /// the two to each other.
    pub const fn index(self) -> usize {
        self as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_names_are_the_adapter_ids() {
        assert_eq!(
            serde_json::to_string(&LanguageId::Typescript).unwrap(),
            "\"typescript\""
        );
        assert_eq!(serde_json::to_string(&LanguageId::Tsx).unwrap(), "\"tsx\"");
        assert_eq!(
            serde_json::to_string(&LanguageId::Javascript).unwrap(),
            "\"javascript\""
        );
        assert_eq!(
            serde_json::to_string(&LanguageId::Python).unwrap(),
            "\"python\""
        );
        assert_eq!(serde_json::to_string(&LanguageId::Go).unwrap(), "\"go\"");
        assert_eq!(
            serde_json::to_string(&LanguageId::Java).unwrap(),
            "\"java\""
        );
        assert_eq!(serde_json::to_string(&LanguageId::C).unwrap(), "\"c\"");
        assert_eq!(serde_json::to_string(&LanguageId::Cpp).unwrap(), "\"cpp\"");
        assert_eq!(
            serde_json::to_string(&LanguageId::Rust).unwrap(),
            "\"rust\""
        );
        let unknown: Result<LanguageId, _> = serde_json::from_str("\"kotlin\"");
        assert!(unknown.is_err(), "unknown language ids must be rejected");
    }

    #[test]
    fn index_matches_all_order() {
        for (position, id) in LanguageId::ALL.iter().enumerate() {
            assert_eq!(id.index(), position);
        }
        assert_eq!(LanguageId::ALL.len(), 9);
    }

    #[test]
    fn as_str_round_trips_through_serde() {
        for id in LanguageId::ALL {
            assert_eq!(
                serde_json::to_string(&id).unwrap(),
                format!("\"{}\"", id.as_str()),
            );
        }
    }
}
