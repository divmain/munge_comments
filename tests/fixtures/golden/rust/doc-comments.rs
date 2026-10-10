//! Crate-level inner doc comment: doc-protected.
//! A second inner doc line.

/// Outer doc comment on a function: doc-protected.
pub fn documented() {}

//// Four slashes: not a doc comment in Rust.
pub fn plain() {}

/** Outer block doc comment: doc-protected. */
pub fn block_documented() {}

pub mod inner {
    /*! Inner block doc comment: doc-protected. */

    // An ordinary comment: prose.
    pub fn f() {}
}
