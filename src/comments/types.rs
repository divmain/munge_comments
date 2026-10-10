//! Shared comment-layer types: the serde vocabulary consumed by the
//! classifier, the artifact module and the wire schemas, plus the scanner's
//! working records.
//!
//! Every shared shape is defined exactly once here (the design's
//! single-source-of-truth rule): `CommentKind`, `Position`,
//! `Classification`, the 3-field `Anchor`, `FileRef`, `DeclInfo`, and the
//! `Edit` triple used by every edit-producing stage.

use serde::{Deserialize, Serialize};

use crate::lang::ids::LanguageId;

/// A comment's syntactic kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentKind {
    /// A `//`-style (or `#`-style) line comment.
    Line,
    /// A `/* … */`-style block comment.
    Block,
    /// A doc comment (detected per language).
    Doc,
}

/// Where a comment sits relative to the code it annotates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    /// Own line(s) above the code it annotates.
    Leading,
    /// Same line as code, after it.
    Trailing,
    /// Unattached: before a closing token or at end of file.
    Dangling,
}

/// The classification the firewall enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Classification {
    /// Candidate for remove/rewrite — the only editable class.
    Prose,
    /// A load-bearing directive; never shown to the LLM as editable, never
    /// modified.
    Functional,
    /// A policy-protected doc comment; forced `KEEP_PROTECTED`, flagged in
    /// the report, human-overridable in the artifact.
    DocProtected,
}

/// The code a comment annotates: tree-sitter node type, the declaration
/// name where the naming rule yields one, and a comment-free ~3-line
/// snippet. `end_of_file` is the language-neutral virtual anchor for
/// dangling comments at EOF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Anchor {
    /// Tree-sitter node type (e.g. `function_declaration`), or the virtual
    /// `end_of_file`.
    pub node_kind: String,
    /// The anchor declaration's name, where the naming rule yields one.
    pub name: Option<String>,
    /// The first 3 lines of the anchor's code, comments excised.
    pub snippet: String,
}

/// Identifies a file for scanning and everything downstream: the language
/// its extension maps to (always passed explicitly — from discovery at plan
/// time, from the artifact at apply time) and the file name, used only for
/// extension-dependent rules such as CommonJS export detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    /// The language (and grammar) the file is processed under.
    pub language: LanguageId,
    /// The file name as discovered (any spelling the caller holds).
    pub file_name: String,
}

/// A declaration, as pure serializable data: the shape remove-mode
/// alignment matches citations against and the shape classifier rules such
/// as `GoExampleOutput` reason over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclInfo {
    /// Tree-sitter node type of the declaration.
    pub node_kind: String,
    /// The declaration's name per the anchor naming rule, if any.
    pub name: Option<String>,
    /// Start byte offset.
    pub start: usize,
    /// End byte offset.
    pub end: usize,
}

/// One enumerated comment: everything later stages (classification,
/// grouping, ids, the artifact) need, as pure data — a complete, exact
/// inventory of the file's comments with positions, anchors and the token
/// context the classification rules are functions of.
///
/// Offsets are UTF-8 byte offsets into the original text; `line`/`column`
/// are 1-based human references (columns count Unicode scalar values).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedComment {
    /// Syntactic kind (`line`/`block`/`doc`).
    pub kind: CommentKind,
    /// Start byte offset (range-normalized: never contains the comment's
    /// own line terminator).
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
    /// 1-based line of `start`.
    pub line: u32,
    /// 1-based column of `start`, in Unicode scalar values.
    pub column: u32,
    /// Raw comment text including delimiters, own EOL trimmed.
    pub text: String,
    /// Leading / trailing / dangling (pinned scanner rules).
    pub position: Position,
    /// The code this comment annotates.
    pub anchor: Anchor,
    /// Whether the anchor declaration is exported (JavaScript's
    /// `JsModule` rule; false everywhere else).
    pub anchor_exported: bool,
    /// Whether this comment is the sole content of a `{/* … */}` JSX
    /// expression container (structurally detected; non-editable).
    pub jsx_container_sole: bool,
    /// End of the last non-comment token before this comment (`None` when
    /// there is none; the shebang is not a code token).
    pub prev_code_end: Option<usize>,
    /// Start of the first non-comment token after this comment
    /// (`text.len()` when there is none).
    pub next_code_start: usize,
    /// The root's named child strictly containing this comment, if any —
    /// keeps classifier rules pure functions of serializable data.
    pub enclosing_top_level: Option<DeclInfo>,
}

/// A text edit: replace `length` bytes starting at `start` (byte offsets
/// into the original text) with `replacement`. The single edit currency of
/// redaction, removal, rewrites and insertions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    /// Start byte offset.
    pub start: usize,
    /// Number of bytes to replace.
    pub length: usize,
    /// The replacement text (empty for a pure deletion).
    pub replacement: String,
}
