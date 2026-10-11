//! Shared helpers for the integration-test crates.
//!
//! Every `tests/*.rs` file is its own crate and uses only a subset of what
//! lives here, hence the crate-level `dead_code` allowance (`clippy
//! --all-targets -D warnings` would otherwise fail on the unused remainder).

#![allow(dead_code)]

pub mod noisy_scenario;
pub mod retention;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use munge_comments::artifact::types::{CommentRecord, DecidedBy, Decision, FilePlan, FileStatus};
use munge_comments::artifact::validate::content_hash;
use munge_comments::artifact::write::{ArtifactHeader, ArtifactWriter};
use munge_comments::comments::types::Classification;
use munge_comments::{
    FileRef, Inventory, InventoryComment, LanguageId, Mode, PlanConfig, compute_summary,
    inventory_file, language_for,
};
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

/// Builds a complete [`FilePlan`] for `text` (at artifact-relative `path`)
/// from its inventory: one scanned `CommentRecord` per inventory record,
/// every field filled from the inventory, `contentHash`/`byteLength`
/// computed from `text`, and the decision plus `finalComment` supplied by
/// `decide`.
///
/// The plan-time model fields (`regenerated`, `wouldYouCommentHere`,
/// `difficulty`, rationales, `missingContext`) are null, `confidence` is 1
/// and `decidedBy` is `human` — the shape of a hand-authored artifact.
/// `effectiveMode` is `remove` (so tests may append NEW-origin records);
/// tests needing redact mode set it on the result.
pub fn file_plan_from_inventory(
    path: &str,
    text: &str,
    inv: &Inventory,
    language: LanguageId,
    decide: impl Fn(&InventoryComment) -> (Decision, Option<String>),
) -> FilePlan {
    let mut comments: Vec<CommentRecord> = inv
        .comments
        .iter()
        .map(|comment| {
            let (decision, final_comment) = decide(comment);
            CommentRecord {
                id: comment.id.clone(),
                kind: comment.kind,
                start: comment.start,
                end: comment.end,
                line: comment.line,
                column: comment.column,
                position: comment.position,
                classification: comment.classification,
                tags: comment.tags.iter().cloned().collect(),
                original: Some(text[comment.start..comment.end].to_owned()),
                anchor: comment.anchor.clone(),
                regenerated: None,
                would_you_comment_here: None,
                difficulty: None,
                difficulty_rationale: None,
                missing_context: None,
                decision,
                final_comment,
                confidence: 1.0,
                decision_rationale: None,
                decided_by: DecidedBy::Human,
            }
        })
        .collect();
    comments.sort_by(|a, b| (a.start, &a.id).cmp(&(b.start, &b.id)));
    FilePlan {
        path: path.to_owned(),
        content_hash: Some(content_hash(text.as_bytes())),
        byte_length: text.len() as u64,
        language,
        effective_mode: Mode::Remove,
        status: FileStatus::Complete,
        failure_kind: None,
        skip_reason: None,
        error: None,
        warnings: Vec::new(),
        comments,
    }
}

/// The decision a plain "remove the prose" plan makes: `REMOVE` for every
/// prose record, `KEEP_PROTECTED` for everything else.
pub fn remove_prose(comment: &InventoryComment) -> (Decision, Option<String>) {
    match comment.classification {
        Classification::Prose => (Decision::Remove, None),
        _ => (Decision::KeepProtected, None),
    }
}

/// Plans the file at `root`/`path` as it is on disk now, through
/// [`file_plan_from_inventory`] (its language from the extension).
pub fn plan_on_disk(
    root: &Path,
    path: &str,
    decide: impl Fn(&InventoryComment) -> (Decision, Option<String>),
) -> FilePlan {
    let text = fs::read_to_string(root.join(path))
        .unwrap_or_else(|err| panic!("reading {path} to plan it: {err}"));
    let language = language_for(Path::new(path)).expect("a supported extension");
    let inventory = inventory_file(
        &text,
        FileRef {
            language,
            file_name: path.to_owned(),
        },
    )
    .unwrap_or_else(|err| panic!("scanning {path}: {err}"));
    file_plan_from_inventory(path, &text, &inventory, language, decide)
}

/// Writes a valid artifact holding `files` at absolute `artifact`, with
/// source root `root` (POSIX, relative to the artifact's directory),
/// through the real writer and `compute_summary`.
pub fn write_plan_artifact(artifact: &Path, root: &str, files: Vec<FilePlan>, allow_add: bool) {
    let header = ArtifactHeader {
        created_at: "2026-10-10T00:00:00Z".to_owned(),
        root: root.to_owned(),
        glob_base: ".".to_owned(),
        config: PlanConfig {
            mode: Mode::Remove,
            model: "test-model".to_owned(),
            base_url: None,
            allow_add,
            similarity_threshold: 0.6,
            max_file_chars: 120_000,
            concurrency: 1,
            offline: true,
            paths: vec![".".to_owned()],
            include: None,
            exclude: Vec::new(),
        },
    };
    ArtifactWriter::new(artifact, header, files, compute_summary)
        .and_then(ArtifactWriter::finish)
        .unwrap_or_else(|err| panic!("writing the artifact {}: {err}", artifact.display()));
}

/// Splits `command` into words by POSIX shell rules, using the real
/// `/bin/sh` (`eval "set -- …"`), so quoting is judged by a shell rather
/// than by a second implementation of the quoting rules.
#[cfg(unix)]
pub fn shell_words(command: &str) -> Vec<String> {
    let output = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"eval "set -- $1"; for word in "$@"; do printf '%s\0' "$word"; done"#)
        .arg("sh")
        .arg(command)
        .output()
        .expect("/bin/sh runs");
    assert!(
        output.status.success(),
        "/bin/sh could not split {command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 words");
    stdout.split_terminator('\0').map(str::to_owned).collect()
}

/// Golden-fixture comment edits the semantic verifier rejects although
/// the edit is correct: `(fixture key "language/file", record raw text,
/// rejection reason prefix)`. Shared by `tests/verifier.rs` and the
/// mutation-property suites (`tests/verifier_props.rs`), so the one list
/// of known grammar-induced false rejections is pinned in one place.
///
/// tree-sitter-typescript's automatic-semicolon scanner does not treat a
/// line break inside a block comment as a line terminator, so it parses
/// `return /*\n  */ x;` as `return x;`. ECMAScript inserts a semicolon
/// there, and every correct edit keeps that reading — the
/// line-break-preserving removal (`return\n  x;`) and the rewrite, whose
/// inline slot is followed by an EOL (`return /* … */\n  x;`) — but the
/// grammar then parses `return; x;`: the trees differ, so the verifier
/// conservatively rejects (the file is skipped, never mis-written).
pub const EXPECTED_VERIFIER_REJECTIONS: &[(&str, &str, &str)] = &[(
    "typescript/asi-hazard.ts",
    "/*\n  */",
    "tree-shape mismatch",
)];
