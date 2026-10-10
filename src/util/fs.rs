//! Filesystem write helpers shared by the artifact writer, apply's write
//! phase and the report writers.
//!
//! Every output file is written atomically: the bytes go to a temp file
//! created in the target's own directory (`NamedTempFile::new_in`, never the
//! ambient temp dir, so the final rename never crosses filesystems), which
//! is then renamed over the target. A reader therefore sees either the old
//! file or the new one, never a torn write.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::errors::Error;

/// Atomically replaces (or creates) the file at absolute `path` with
/// `bytes`.
///
/// When `path` already exists its permission bits are re-applied to the
/// replacement (an executable script stays executable). A new file gets
/// the permissions of an ordinary create (`0o666` minus the umask on Unix)
/// rather than the temp file's private `0o600`. The data is synced before
/// the rename.
///
/// A relative `path` is `Error::Invariant` (library code never resolves
/// against the process cwd); any I/O failure is `Error::Fs`.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::Invariant(format!(
            "atomic_write needs an absolute path, got {}",
            path.display()
        )));
    }
    let fs_err = |source: std::io::Error| Error::Fs {
        path: path.to_path_buf(),
        source,
    };
    let dir = path.parent().ok_or_else(|| {
        Error::Invariant(format!(
            "atomic_write target {} has no parent",
            path.display()
        ))
    })?;
    let existing = match fs::metadata(path) {
        Ok(meta) => Some(meta.permissions()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(fs_err(err)),
    };
    let mut temp = new_temp_in(dir).map_err(fs_err)?;
    temp.write_all(bytes).map_err(fs_err)?;
    temp.as_file().sync_all().map_err(fs_err)?;
    if let Some(permissions) = existing {
        fs::set_permissions(temp.path(), permissions).map_err(fs_err)?;
    }
    temp.persist(path).map_err(|err| fs_err(err.error))?;
    Ok(())
}

/// A same-directory temp file whose mode, on Unix, is that of an ordinary
/// file create (`0o666` minus the umask).
fn new_temp_in(dir: &Path) -> std::io::Result<NamedTempFile> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o666))
            .tempfile_in(dir)
    }
    #[cfg(not(unix))]
    {
        NamedTempFile::new_in(dir)
    }
}

/// Creates the parent directory of absolute output `path` (and any missing
/// ancestors), then returns that directory canonicalized.
///
/// Output paths (`--out`, `--report`) may name a directory that does not
/// exist yet; creating it first lets the caller canonicalize it, so later
/// relativization never mixes canonical and non-canonical spellings. A
/// relative `path` or one without a parent is `Error::Invariant`; a failure
/// to create or canonicalize the directory is `Error::Fs`.
pub fn ensure_parent_dir(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::Invariant(format!(
            "ensure_parent_dir needs an absolute path, got {}",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invariant(format!("output path {} has no parent", path.display())))?;
    let fs_err = |source: std::io::Error| Error::Fs {
        path: parent.to_path_buf(),
        source,
    };
    fs::create_dir_all(parent).map_err(fs_err)?;
    fs::canonicalize(parent).map_err(fs_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left behind");
    }

    #[test]
    fn atomic_write_rejects_relative_paths() {
        let err = atomic_write(Path::new("relative.json"), b"x").unwrap_err();
        assert_eq!(err.exit_code(), 7);
    }

    #[test]
    fn atomic_write_into_a_missing_directory_is_an_fs_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("out.json");
        let err = atomic_write(&path, b"x").unwrap_err();
        assert_eq!(err.exit_code(), 6);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write(&path, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_gives_new_files_ordinary_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        atomic_write(&path, b"{}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        // Whatever the umask, the owner can read and write, and the mode is
        // not pinned to the temp file's private 0o600 unless the umask says so.
        assert_eq!(mode & 0o600, 0o600);
        assert_eq!(mode & 0o111, 0, "never executable");
    }

    #[test]
    fn ensure_parent_dir_creates_missing_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a").join("b").join("c").join("plan.json");
        let parent = ensure_parent_dir(&out).unwrap();
        assert!(parent.is_dir());
        assert!(parent.is_absolute());
        assert_eq!(
            parent,
            fs::canonicalize(dir.path().join("a/b/c")).unwrap(),
            "the parent is returned canonicalized"
        );
        assert!(!out.exists(), "only the directory is created");
    }

    #[test]
    fn ensure_parent_dir_under_a_file_is_an_fs_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, b"").unwrap();
        let err = ensure_parent_dir(&blocker.join("sub").join("out.json")).unwrap_err();
        assert_eq!(err.exit_code(), 6);
    }
}
