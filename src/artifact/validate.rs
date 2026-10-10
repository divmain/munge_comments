//! Content hashing, strict decoding, and the source-root / file-path checks
//! shared by `plan` (root computation) and `apply` (root resolution and
//! per-file path validation).

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::errors::Error;
use crate::util::paths::relative_posix;

/// `"sha256:<lowercase hex>"` over the raw bytes (CRLF and BOM included,
/// so any drift is detected, not just logical change).
pub fn content_hash(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity("sha256:".len() + 2 * digest.len());
    out.push_str("sha256:");
    for byte in digest.iter() {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Strict UTF-8 decoding: never lossy, and a BOM is kept as ordinary text
/// at offset 0. `None` for any invalid byte sequence.
pub fn decode_utf8(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

/// Resolves an artifact's stored `root` against the directory of the
/// absolute `artifact_path` and canonicalizes it.
///
/// A relative `artifact_path` is `Error::Invariant`. A root that does not
/// resolve to an existing directory (e.g. the artifact was moved without
/// its sources) is `Error::Artifact`.
pub fn resolve_root(artifact_path: &Path, root: &str) -> Result<PathBuf, Error> {
    if !artifact_path.is_absolute() {
        return Err(Error::Invariant(format!(
            "resolve_root needs an absolute artifact path, got {}",
            artifact_path.display()
        )));
    }
    let dir = artifact_path.parent().ok_or_else(|| {
        Error::Invariant(format!(
            "artifact path {} has no parent directory",
            artifact_path.display()
        ))
    })?;
    let joined = dir.join(root);
    let unresolvable = |detail: String| {
        Error::Artifact(format!(
            "{}: source root {root:?} (resolved to {}) {detail}",
            artifact_path.display(),
            joined.display()
        ))
    };
    let canonical = fs::canonicalize(&joined)
        .map_err(|err| unresolvable(format!("cannot be resolved: {err}")))?;
    if !canonical.is_dir() {
        return Err(unresolvable("is not a directory".to_owned()));
    }
    Ok(canonical)
}

/// Validates an artifact file path against the canonical `root` and
/// returns the path to operate on: the file name joined onto its
/// canonicalized parent directory (the final component is deliberately not
/// resolved — a symlink there is drift, caught by the link checks).
///
/// Rejected with `Error::Artifact` (exit 2): an absolute or empty path, a
/// path naming no file (`.`, `a/..`), a segment that is not a single plain
/// path component on this platform (NUL bytes, `\` or drive prefixes on
/// Windows), a `..` that escapes `root`, and a path whose canonicalized
/// parent directory lies outside `root` (a symlinked intermediate
/// directory). A parent directory that does not exist passes (nothing on
/// disk to escape through; reading the file then reports it missing); any
/// other failure to canonicalize it is `Error::Fs`. A relative `root` is
/// `Error::Invariant`.
pub fn check_file_path(root: &Path, path: &str) -> Result<PathBuf, Error> {
    if !root.is_absolute() {
        return Err(Error::Invariant(format!(
            "check_file_path needs an absolute root, got {}",
            root.display()
        )));
    }
    let reject = |detail: &str| Err(Error::Artifact(format!("file path {path:?} {detail}")));
    if path.is_empty() {
        return reject("is empty");
    }
    if path.starts_with('/') || Path::new(path).has_root() || Path::new(path).is_absolute() {
        return reject("must be relative to the artifact's root");
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return reject("escapes the artifact's root");
                }
            }
            plain => {
                let mut components = Path::new(plain).components();
                let single = matches!(
                    (components.next(), components.next()),
                    (Some(Component::Normal(name)), None) if name == plain
                );
                if !single || plain.contains('\0') {
                    return reject("contains a segment that is not a plain file name");
                }
                segments.push(plain);
            }
        }
    }
    let Some((file_name, parents)) = segments.split_last() else {
        return reject("does not name a file");
    };
    let parent = parents
        .iter()
        .fold(root.to_path_buf(), |dir, seg| dir.join(seg));
    let canonical_parent = match fs::canonicalize(&parent) {
        Ok(canonical) => canonical,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(parent.join(file_name)),
        Err(source) => {
            return Err(Error::Fs {
                path: parent,
                source,
            });
        }
    };
    if !canonical_parent.starts_with(root) {
        return reject(&format!(
            "resolves through a symlinked directory to {}, outside the root {}",
            canonical_parent.display(),
            root.display()
        ));
    }
    Ok(canonical_parent.join(file_name))
}

/// Computes the artifact's `root`: `cwd` if every file lies under it,
/// otherwise the deepest common ancestor directory of all files —
/// returned as a POSIX path relative to `artifact_dir` (`"."` when equal,
/// possibly starting with `..`).
///
/// All inputs must be absolute and canonical; this is asserted against the
/// filesystem (`Error::Invariant` on a mismatch, `Error::Fs` when a path
/// cannot be canonicalized), so a lexical `/var` vs `/private/var` mix can
/// never produce a wrong `..` path. Files sharing no common ancestor, or a
/// root with no POSIX spelling relative to `artifact_dir` (different
/// Windows drives), are `Error::Usage`.
pub fn compute_root(
    cwd: &Path,
    abs_paths: &[PathBuf],
    artifact_dir: &Path,
) -> Result<String, Error> {
    assert_canonical(cwd)?;
    assert_canonical(artifact_dir)?;
    for path in abs_paths {
        assert_canonical(path)?;
    }
    let root = if abs_paths.iter().all(|path| path.starts_with(cwd)) {
        cwd.to_path_buf()
    } else {
        common_ancestor(abs_paths)?
    };
    relative_posix(artifact_dir, &root).ok_or_else(|| {
        Error::Usage(format!(
            "the source root {} cannot be expressed relative to the artifact directory {}",
            root.display(),
            artifact_dir.display()
        ))
    })
}

/// The deepest directory containing every path (each path's parent at the
/// most). Callers pass at least one path.
fn common_ancestor(abs_paths: &[PathBuf]) -> Result<PathBuf, Error> {
    let no_ancestor =
        || Error::Usage("the discovered files share no common ancestor directory".to_owned());
    let mut paths = abs_paths.iter();
    let first = paths.next().ok_or_else(no_ancestor)?;
    let mut ancestor = first.parent().ok_or_else(no_ancestor)?;
    for path in paths {
        while !path.starts_with(ancestor) {
            ancestor = ancestor.parent().ok_or_else(no_ancestor)?;
        }
    }
    Ok(ancestor.to_path_buf())
}

/// Asserts `path` is absolute and equal to its own canonical form.
fn assert_canonical(path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::Invariant(format!(
            "expected an absolute canonical path, got {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|source| Error::Fs {
        path: path.to_path_buf(),
        source,
    })?;
    if canonical != path {
        return Err(Error::Invariant(format!(
            "expected a canonical path, got {} (canonical form {})",
            path.display(),
            canonical.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(dir.path()).unwrap();
        (dir, canonical)
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn content_hash_matches_known_sha256_vectors() {
        assert_eq!(
            content_hash(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            content_hash(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn decode_utf8_is_strict_and_keeps_the_bom() {
        assert_eq!(decode_utf8(b"\xEF\xBB\xBFx"), Some("\u{feff}x"));
        assert_eq!(decode_utf8(b"ok"), Some("ok"));
        assert_eq!(decode_utf8(b"\xff"), None);
    }

    #[test]
    fn resolve_root_joins_onto_the_artifact_directory() {
        let (_dir, base) = canonical_tempdir();
        fs::create_dir_all(base.join("src")).unwrap();
        fs::create_dir_all(base.join("out")).unwrap();
        let artifact = base.join("out").join("plan.json");
        assert_eq!(resolve_root(&artifact, "../src").unwrap(), base.join("src"));
        assert_eq!(resolve_root(&artifact, ".").unwrap(), base.join("out"));
        let missing = resolve_root(&artifact, "../gone").unwrap_err();
        assert_eq!(missing.exit_code(), 2);
        let relative = resolve_root(Path::new("plan.json"), ".").unwrap_err();
        assert_eq!(relative.exit_code(), 7);
    }

    #[test]
    fn check_file_path_accepts_paths_under_root() {
        let (_dir, root) = canonical_tempdir();
        touch(&root.join("src/a.ts"));
        assert_eq!(
            check_file_path(&root, "src/a.ts").unwrap(),
            root.join("src/a.ts")
        );
        assert_eq!(
            check_file_path(&root, "./src/x/../a.ts").unwrap(),
            root.join("src/a.ts")
        );
        assert_eq!(
            check_file_path(&root, "top.ts").unwrap(),
            root.join("top.ts")
        );
        // A missing parent passes: reading the file reports it missing.
        assert_eq!(
            check_file_path(&root, "gone/b.ts").unwrap(),
            root.join("gone/b.ts")
        );
    }

    #[test]
    fn check_file_path_rejects_absolute_escaping_and_empty_paths() {
        let (_dir, root) = canonical_tempdir();
        for bad in [
            "/etc/passwd",
            "../outside.ts",
            "a/../../outside.ts",
            "",
            ".",
            "a/..",
        ] {
            let err = check_file_path(&root, bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?} must be rejected");
        }
        let err = check_file_path(&root, "a\0b.ts").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn check_file_path_rejects_a_symlinked_directory_outside_root() {
        let (_dir, base) = canonical_tempdir();
        let root = base.join("root");
        let outside = base.join("outside");
        touch(&outside.join("secret.ts"));
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let err = check_file_path(&root, "link/secret.ts").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("symlinked directory"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn check_file_path_allows_a_symlinked_directory_inside_root() {
        let (_dir, root) = canonical_tempdir();
        touch(&root.join("real/a.ts"));
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        assert_eq!(
            check_file_path(&root, "alias/a.ts").unwrap(),
            root.join("real/a.ts")
        );
    }

    #[test]
    fn check_file_path_requires_an_absolute_root() {
        let err = check_file_path(Path::new("rel"), "a.ts").unwrap_err();
        assert_eq!(err.exit_code(), 7);
    }

    #[test]
    fn compute_root_is_the_cwd_when_it_contains_every_file() {
        let (_dir, base) = canonical_tempdir();
        touch(&base.join("src/a.ts"));
        touch(&base.join("src/deep/b.ts"));
        let files = vec![base.join("src/a.ts"), base.join("src/deep/b.ts")];
        assert_eq!(compute_root(&base, &files, &base).unwrap(), ".");
        assert_eq!(compute_root(&base, &[], &base).unwrap(), ".");
        let out = base.join("out");
        fs::create_dir_all(&out).unwrap();
        assert_eq!(compute_root(&base, &files, &out).unwrap(), "..");
    }

    #[test]
    fn compute_root_falls_back_to_the_deepest_common_ancestor() {
        let (_dir, base) = canonical_tempdir();
        touch(&base.join("a/x/one.ts"));
        touch(&base.join("a/y/two.ts"));
        fs::create_dir_all(base.join("cwd")).unwrap();
        let files = vec![base.join("a/x/one.ts"), base.join("a/y/two.ts")];
        assert_eq!(compute_root(&base.join("cwd"), &files, &base).unwrap(), "a");
        let single = vec![base.join("a/x/one.ts")];
        assert_eq!(
            compute_root(&base.join("cwd"), &single, &base).unwrap(),
            "a/x"
        );
    }

    #[test]
    fn compute_root_with_an_artifact_outside_the_tree_resolves_back() {
        let (_dir, base) = canonical_tempdir();
        let tree = base.join("project");
        touch(&tree.join("src/main.rs"));
        let artifacts = base.join("elsewhere/plans");
        fs::create_dir_all(&artifacts).unwrap();
        let files = vec![tree.join("src/main.rs")];
        let root = compute_root(&tree, &files, &artifacts).unwrap();
        assert_eq!(root, "../../project");
        let resolved = resolve_root(&artifacts.join("plan.json"), &root).unwrap();
        assert_eq!(resolved, tree);
    }

    #[cfg(unix)]
    #[test]
    fn compute_root_rejects_non_canonical_inputs() {
        let (_dir, base) = canonical_tempdir();
        touch(&base.join("real/a.ts"));
        std::os::unix::fs::symlink(base.join("real"), base.join("alias")).unwrap();
        let files = vec![base.join("real/a.ts")];
        let err = compute_root(&base.join("alias"), &files, &base).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        let err = compute_root(&base, &[base.join("alias/a.ts")], &base).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        let err = compute_root(&base, &files, &base.join("./real/..")).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        let err = compute_root(Path::new("rel"), &files, &base).unwrap_err();
        assert_eq!(err.exit_code(), 7);
    }
}
