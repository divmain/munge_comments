//! Shared helpers for the integration-test crates.
//!
//! Every `tests/*.rs` file is its own crate and uses only a subset of what
//! lives here, hence the crate-level `dead_code` allowance (`clippy
//! --all-targets -D warnings` would otherwise fail on the unused remainder).

#![allow(dead_code)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use proptest::test_runner::{Config, RngSeed};
use tempfile::TempDir;

/// Seeds pinned once so every proptest suite is deterministic: the default
/// config uses [`PROPTEST_SEEDS[0]`](PROPTEST_SEEDS), and suites that need a
/// second or third independent stream (e.g. paired generator/consumer runs)
/// use the others via [`proptest_config_with_seed`]. Nothing here is random
/// across runs, so failures always reproduce.
pub const PROPTEST_SEEDS: [u64; 3] = [0x5eed_1a2b, 0x5eed_3c4d, 0x5eed_5e6f];

/// Creates a fresh temp directory.
///
/// A named helper rather than a bare `tempfile::tempdir()` call so every
/// creation site fails with the same, obvious message.
pub fn tempdir() -> TempDir {
    tempfile::tempdir().expect("tests may create temp directories")
}

/// Absolute path of a fixture, `rel` being relative to `tests/fixtures/`
/// (e.g. `"golden/js/strings.js"`).
///
/// `env!("CARGO_MANIFEST_DIR")` is baked in at compile time, so this works
/// no matter which directory a test invocation runs from.
pub fn fixture_path(rel: impl AsRef<Path>) -> PathBuf {
    fixtures_root().join(rel)
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Copies a fixture tree (`rel` relative to `tests/fixtures/`, e.g.
/// `"noisy-repo"`) into a fresh [`TempDir`], recursively and byte-exact, and
/// returns the copy.
///
/// Tests always operate on a copy — fixture trees are never mutated in place.
pub fn copy_fixture_tree(rel: impl AsRef<Path>) -> TempDir {
    let src = fixture_path(rel);
    let dst = tempdir();
    copy_tree(&src, dst.path())
        .unwrap_or_else(|err| panic!("copying fixture tree {}: {err}", src.display()));
    dst
}

/// Recursive copy preserving file bytes (and, via `fs::copy`, permission
/// bits). Fixture trees contain only plain files and directories today; a
/// future fixture that needs symlink semantics extends this helper.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// A cloneable shared byte buffer that can be injected as an `io::Write` sink
/// (captured stdout/stderr for `cli::run` tests) while the test keeps a
/// clone to read back what was written.
///
/// Cheap to clone (`Arc` inside); `Send + Sync`, so it crosses the streams'
/// `Box<dyn Write + Send>` boundary.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// A new, empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written so far, decoded as UTF-8.
    ///
    /// Panics on invalid UTF-8: everything the CLI writes is UTF-8 by
    /// construction, and a test helper should surface an encoding bug, not
    /// paper over it.
    pub fn contents(&self) -> String {
        let bytes = self.lock();
        String::from_utf8(bytes.clone()).expect("captured output is valid UTF-8")
    }

    fn lock(&self) -> MutexGuard<'_, Vec<u8>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The default proptest config: [`Config::default()`] with the seed pinned to
/// [`PROPTEST_SEEDS[0]`](PROPTEST_SEEDS) and failure persistence disabled.
///
/// Starting from `Config::default()` keeps every other policy — notably the
/// case count honoring `PROPTEST_CASES` (proptest's own environment read) —
/// at proptest's defaults, so acceptance runs can raise the count explicitly
/// where a ticket says so. `failure_persistence: None` means no
/// `proptest-regressions/` files are ever created.
pub fn proptest_config() -> Config {
    proptest_config_with_seed(PROPTEST_SEEDS[0])
}

/// [`proptest_config`] with an explicit seed (normally one of [`PROPTEST_SEEDS`]).
pub fn proptest_config_with_seed(seed: u64) -> Config {
    Config {
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    }
}
