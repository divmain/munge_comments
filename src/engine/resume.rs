//! `--resume`: continuing a plan run from its existing artifact.
//!
//! A resumed run never recomputes what the original run fixed:
//! - the artifact's `root` (resolved against the artifact's directory) is
//!   reused, so carried `FilePlan.path`s and the stored snapshot `paths`
//!   stay meaningful, and every discovered file must lie under it
//!   ([`check_under_root`]);
//! - its `globBase` (resolved against that root) is the directory discovery
//!   matches `--include`/`--exclude` against, so the same globs select the
//!   same files from any resuming cwd;
//! - its config snapshot must match the newly resolved request on every
//!   field that shaped the carried work ([`compare_snapshot`]); only
//!   `concurrency` may differ, and the resumed artifact records the new
//!   value.
//!
//! [`reconcile`] then splits the work: `complete`/`skipped` entries whose
//! file is still discovered with an unchanged content hash are carried
//! (zero LLM calls), failed, changed and newly discovered files are
//! processed, and entries no longer discovered are dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::artifact::read::read_artifact;
use crate::artifact::types::{FilePlan, FileStatus, PlanArtifact, ValidatedArtifact};
use crate::artifact::validate::resolve_root;
use crate::config::PlanConfig;
use crate::discover::DiscoveredFile;
use crate::errors::Error;
use crate::util::paths::{normalize_posix, snapshot_path};

/// The snapshot fields `--resume` compares, by their artifact (camelCase)
/// names. Every `PlanConfig` field except `concurrency`.
pub const SNAPSHOT_FIELDS: [&str; 10] = [
    "mode",
    "model",
    "baseUrl",
    "allowAdd",
    "similarityThreshold",
    "maxFileChars",
    "offline",
    "paths",
    "include",
    "exclude",
];

/// Loads the artifact at absolute `out` for resuming: `None` when there is
/// no file there yet (the caller starts a fresh run).
///
/// # Errors
///
/// An unreadable, malformed or invalid artifact is the error
/// [`read_artifact`] reports ([`Error::Artifact`], exit 2, for anything
/// wrong with its contents); a failed existence check is [`Error::Fs`].
pub fn load_for_resume(out: &Path) -> Result<Option<ValidatedArtifact>, Error> {
    match fs::symlink_metadata(out) {
        Ok(_) => read_artifact(out).map(Some),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Fs {
            path: out.to_path_buf(),
            source,
        }),
    }
}

/// What a resumed run keeps from the existing artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeBase {
    /// The original run's `createdAt`, preserved.
    pub created_at: String,
    /// The stored `root`, verbatim (relative to the artifact's directory).
    pub root: String,
    /// `root` resolved against the artifact's directory and canonicalized.
    pub root_abs: PathBuf,
    /// The stored `globBase`, verbatim (relative to `root`).
    pub glob_base: String,
    /// `globBase` resolved against `root_abs` and canonicalized: the
    /// directory discovery matches include/exclude globs against.
    pub glob_base_abs: PathBuf,
    /// The existing artifact's file plans, to [`reconcile`].
    pub files: Vec<FilePlan>,
}

/// Prepares a resumed run from the `existing` artifact at absolute `out`:
/// resolves its root and glob base, then checks the `resolved` request
/// (paths as given, relative to the canonical `cwd`) against its snapshot.
///
/// # Errors
///
/// - [`Error::Artifact`] (exit 2): the stored root or glob base no longer
///   resolves to a directory;
/// - [`Error::Usage`] (exit 1): a snapshot mismatch ([`compare_snapshot`]).
pub fn resume_base(
    existing: PlanArtifact,
    out: &Path,
    cwd: &Path,
    resolved: &PlanConfig,
) -> Result<ResumeBase, Error> {
    let root_abs = resolve_root(out, &existing.root)?;
    let glob_base_abs = fs::canonicalize(root_abs.join(&existing.glob_base))
        .ok()
        .filter(|dir| dir.is_dir())
        .ok_or_else(|| {
            Error::Artifact(format!(
                "{}: glob base {:?} no longer resolves to a directory under the source root {}",
                out.display(),
                existing.glob_base,
                root_abs.display()
            ))
        })?;
    compare_snapshot(&existing.config, resolved, cwd, &root_abs).map_err(|err| match err {
        Error::Usage(diff) => Error::Usage(format!(
            "cannot resume {}: {diff}\nrerun with the original settings, or start a fresh plan \
             with another --out",
            out.display()
        )),
        other => other,
    })?;
    Ok(ResumeBase {
        created_at: existing.created_at,
        root: existing.root,
        root_abs,
        glob_base: existing.glob_base,
        glob_base_abs,
        files: existing.files,
    })
}

/// Compares the snapshot fields ([`SNAPSHOT_FIELDS`]) of the `existing`
/// artifact's config with the newly `resolved` one. The resolved `paths`
/// are first put in the artifact's stored form ([`snapshot_path`] against
/// the canonical `cwd` and the reused absolute `root`), so equivalent
/// paths given from another cwd compare equal.
///
/// # Errors
///
/// [`Error::Usage`] with one `field: artifact <json> ≠ requested <json>`
/// line per differing field; [`snapshot_path`]'s errors for a request path
/// with no stored form.
pub fn compare_snapshot(
    existing: &PlanConfig,
    resolved: &PlanConfig,
    cwd: &Path,
    root: &Path,
) -> Result<(), Error> {
    let stored = PlanConfig {
        paths: stored_paths(&resolved.paths, cwd, root)?,
        ..resolved.clone()
    };
    let existing = snapshot_object(existing)?;
    let stored = snapshot_object(&stored)?;
    let diff: Vec<String> = SNAPSHOT_FIELDS
        .iter()
        .filter_map(|field| {
            let (was, now) = (&existing[*field], &stored[*field]);
            (was != now).then(|| format!("  {field}: artifact {was} ≠ requested {now}"))
        })
        .collect();
    if diff.is_empty() {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "the requested settings differ from the artifact's snapshot:\n{}",
            diff.join("\n")
        )))
    }
}

/// The request `paths` in the artifact's stored form (see
/// [`snapshot_path`]).
pub fn stored_paths(paths: &[String], cwd: &Path, root: &Path) -> Result<Vec<String>, Error> {
    paths
        .iter()
        .map(|arg| snapshot_path(arg, cwd, root))
        .collect()
}

/// A config as its artifact JSON object.
fn snapshot_object(config: &PlanConfig) -> Result<serde_json::Map<String, Value>, Error> {
    match serde_json::to_value(config) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(other) => Err(Error::Invariant(format!(
            "the config snapshot serialized to a non-object: {other}"
        ))),
        Err(err) => Err(Error::Invariant(format!(
            "serializing the config snapshot: {err}"
        ))),
    }
}

/// Requires every discovered file to lie under the reused canonical
/// `root`: carried paths and new ones must share one root.
///
/// # Errors
///
/// [`Error::Usage`] (exit 1) naming the first file outside it.
pub fn check_under_root(files: &[DiscoveredFile], root: &Path) -> Result<(), Error> {
    match files.iter().find(|file| !file.abs_path.starts_with(root)) {
        None => Ok(()),
        Some(file) => Err(Error::Usage(format!(
            "discovery escapes the resumed root; start a fresh plan ({} lies outside {})",
            file.abs_path.display(),
            root.display()
        ))),
    }
}

/// The root-relative paths (normalized) of the existing entries that may
/// be carried if their content hash still matches: `complete` and
/// `skipped` files. Only these need hashing before [`reconcile`].
pub fn carry_candidates(existing: &[FilePlan]) -> BTreeSet<String> {
    existing
        .iter()
        .filter(|file| carriable(file))
        .map(|file| normalize_posix(&file.path))
        .collect()
}

fn carriable(file: &FilePlan) -> bool {
    matches!(file.status, FileStatus::Complete | FileStatus::Skipped) && file.content_hash.is_some()
}

/// How a resumed run splits its work.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reconciled {
    /// Entries carried unchanged into the resumed artifact (their `path`
    /// spelled as discovered).
    pub carry: Vec<FilePlan>,
    /// Root-relative paths to (re)plan, in discovery order: failed,
    /// hash-changed (or unreadable) and newly discovered files.
    pub process: Vec<String>,
    /// Paths of existing entries no longer discovered (dropped from the
    /// artifact), in artifact order.
    pub dropped: Vec<String>,
}

/// Reconciles the existing artifact's entries with this run's discovery.
///
/// `discovered` holds the root-relative POSIX paths discovery selected
/// (normalized, as the run computes them); `disk_hashes` maps a discovered
/// path to the content hash of its bytes on disk now (a path missing from
/// it — not hashed, or unreadable — is never carried). Existing paths are
/// matched after POSIX normalization (`a/../b.ts` ≡ `b.ts`).
///
/// A `complete` or `skipped` entry whose stored hash equals the disk hash
/// is carried; every other discovered file is processed; existing entries
/// not discovered are dropped.
pub fn reconcile(
    existing_files: Vec<FilePlan>,
    discovered: &[String],
    disk_hashes: &BTreeMap<String, String>,
) -> Reconciled {
    let wanted: BTreeSet<&str> = discovered.iter().map(String::as_str).collect();
    let mut reconciled = Reconciled::default();
    let mut carried: BTreeSet<String> = BTreeSet::new();
    for mut file in existing_files {
        let path = normalize_posix(&file.path);
        if !wanted.contains(path.as_str()) {
            reconciled.dropped.push(file.path);
            continue;
        }
        let unchanged = carriable(&file)
            && disk_hashes.get(&path).map(String::as_str) == file.content_hash.as_deref();
        if unchanged {
            file.path = path.clone();
            carried.insert(path);
            reconciled.carry.push(file);
        }
    }
    reconciled.process = discovered
        .iter()
        .filter(|path| !carried.contains(path.as_str()))
        .cloned()
        .collect();
    reconciled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::types::{FailureKind, SkipReason};
    use crate::config::Mode;
    use crate::lang::ids::LanguageId;

    fn config() -> PlanConfig {
        PlanConfig {
            mode: Mode::Redact,
            model: "gpt-4.1".to_owned(),
            base_url: None,
            allow_add: false,
            similarity_threshold: 0.6,
            max_file_chars: 120_000,
            concurrency: 4,
            offline: false,
            paths: vec![".".to_owned()],
            include: None,
            exclude: Vec::new(),
        }
    }

    fn entry(path: &str, status: FileStatus, hash: &str) -> FilePlan {
        FilePlan {
            path: path.to_owned(),
            content_hash: Some(hash.to_owned()),
            byte_length: 1,
            language: LanguageId::Typescript,
            effective_mode: Mode::Redact,
            status,
            failure_kind: (status == FileStatus::Failed).then_some(FailureKind::Llm),
            skip_reason: (status == FileStatus::Skipped).then_some(SkipReason::SyntaxErrors),
            error: None,
            warnings: Vec::new(),
            comments: Vec::new(),
        }
    }

    /// An absolute directory for the lexical snapshot comparisons (never
    /// touched on disk).
    fn abs_root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\repo" } else { "/repo" })
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn hashes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(path, hash)| ((*path).to_owned(), (*hash).to_owned()))
            .collect()
    }

    #[test]
    fn the_snapshot_fields_are_every_config_field_but_concurrency() {
        let mut keys: Vec<String> = snapshot_object(&config())
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.retain(|key| key != "concurrency");
        keys.sort();
        let mut fields = strings(&SNAPSHOT_FIELDS);
        fields.sort();
        assert_eq!(keys, fields);
    }

    #[test]
    fn only_concurrency_may_differ() {
        let root = abs_root();
        let resolved = PlanConfig {
            concurrency: 16,
            ..config()
        };
        compare_snapshot(&config(), &resolved, &root, &root).unwrap();
    }

    #[test]
    fn a_mismatch_is_a_usage_error_listing_every_differing_field() {
        let root = abs_root();
        let resolved = PlanConfig {
            allow_add: true,
            exclude: strings(&["tests/fixtures/**"]),
            ..config()
        };
        let err = compare_snapshot(&config(), &resolved, &root, &root).unwrap_err();
        let Error::Usage(message) = err else {
            panic!("expected a usage error, got {err:?}");
        };
        assert!(
            message.contains("  allowAdd: artifact false ≠ requested true"),
            "{message}"
        );
        assert!(
            message.contains(r#"  exclude: artifact [] ≠ requested ["tests/fixtures/**"]"#),
            "{message}"
        );
        assert_eq!(message.lines().count(), 3, "{message}");
    }

    #[test]
    fn paths_compare_in_stored_form() {
        let root = abs_root();
        let sub = root.join("sub");
        let resolved = PlanConfig {
            paths: strings(&[".."]),
            ..config()
        };
        // `..` from `<root>/sub` is the stored `.`.
        compare_snapshot(&config(), &resolved, &sub, &root).unwrap();
        let err = compare_snapshot(&config(), &resolved, &root, &root).unwrap_err();
        assert!(err.to_string().contains("paths:"), "{err}");
    }

    #[test]
    fn reconcile_carries_unchanged_work_and_processes_the_rest() {
        let existing = vec![
            entry("a.ts", FileStatus::Complete, "sha256:a"),
            entry("b.ts", FileStatus::Complete, "sha256:b"),
            entry("c.ts", FileStatus::Failed, "sha256:c"),
            entry("d.ts", FileStatus::Skipped, "sha256:d"),
            entry("e.ts", FileStatus::Skipped, "sha256:e"),
            entry("gone.ts", FileStatus::Complete, "sha256:g"),
            entry("x/../f.ts", FileStatus::Complete, "sha256:f"),
            entry("unread.ts", FileStatus::Complete, "sha256:u"),
        ];
        let discovered = strings(&[
            "a.ts",
            "b.ts",
            "c.ts",
            "d.ts",
            "e.ts",
            "f.ts",
            "new.ts",
            "unread.ts",
        ]);
        let disk = hashes(&[
            ("a.ts", "sha256:a"),
            ("b.ts", "sha256:CHANGED"),
            ("c.ts", "sha256:c"),
            ("d.ts", "sha256:d"),
            ("e.ts", "sha256:CHANGED"),
            ("f.ts", "sha256:f"),
        ]);
        let reconciled = reconcile(existing, &discovered, &disk);
        let carried: Vec<&str> = reconciled.carry.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(carried, ["a.ts", "d.ts", "f.ts"]);
        assert_eq!(
            reconciled.process,
            strings(&["b.ts", "c.ts", "e.ts", "new.ts", "unread.ts"])
        );
        assert_eq!(reconciled.dropped, strings(&["gone.ts"]));
    }

    #[test]
    fn carry_candidates_are_the_complete_and_skipped_entries() {
        let existing = vec![
            entry("a.ts", FileStatus::Complete, "sha256:a"),
            entry("c.ts", FileStatus::Failed, "sha256:c"),
            entry("x/../d.ts", FileStatus::Skipped, "sha256:d"),
        ];
        let candidates: Vec<String> = carry_candidates(&existing).into_iter().collect();
        assert_eq!(candidates, strings(&["a.ts", "d.ts"]));
    }
}
