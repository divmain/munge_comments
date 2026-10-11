//! The language layer: the extension table, plain tree data, adapter types,
//! the per-language adapter data modules, and the grammar registry.
//!
//! The registry (the single tree-sitter user in the crate) validates the
//! adapters against the linked grammars at initialization and turns source
//! text into [`PlainTree`](types::PlainTree) data via the re-exported
//! [`parse`] entry point.

pub mod adapter;
pub mod adapters;
pub mod extensions;
pub mod ids;
pub mod registry;
pub mod types;

pub use registry::parse;
