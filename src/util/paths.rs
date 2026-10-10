//! POSIX path helpers: lexical normalization, relativization, the
//! artifact's snapshot form of request paths, and shell quoting for
//! suggested command lines.
//!
//! Everything here is lexical — no filesystem access. Callers that need
//! symlink-correct answers canonicalize first (see
//! `artifact::validate::compute_root`), so relativization never mixes
//! canonical and non-canonical spellings of one directory.

use std::path::{Component, Path, PathBuf};

use crate::errors::Error;

/// Lexically normalizes a POSIX path string: empty and `.` segments are
/// dropped, `seg/..` pairs collapse, and leading `..` segments of a
/// relative path are kept (`..` above `/` stays at `/`). The empty result is
/// `"."`. Used, e.g., to decide that `a/../b.ts` and `b.ts` name one file.
pub fn normalize_posix(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(&last) if last != ".." => {
                    segments.pop();
                }
                _ if absolute => {}
                _ => segments.push(".."),
            },
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// Whether `path` is a relative POSIX path as the artifact stores them:
/// non-empty, not rooted (`/…`), no Windows drive prefix (`C:…`), and free
/// of backslashes and NUL bytes (so it means the same thing on every
/// platform).
pub fn is_relative_posix(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    !path.is_empty()
        && !path.starts_with('/')
        && !drive_prefix
        && !path.contains('\\')
        && !path.contains('\0')
}

/// Lexically normalizes a native path: `.` components are dropped and
/// `seg/..` pairs collapse (`..` directly under the root is dropped). Never
/// touches the filesystem, so a symlinked `seg` is not resolved.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// Renders a path with `/` separators. `None` for a non-UTF-8 component or
/// a Windows prefix (`C:`), which have no POSIX spelling. The empty path
/// renders as `"."`.
pub fn to_posix(path: &Path) -> Option<String> {
    let mut absolute = false;
    let mut segments: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => return None,
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => segments.push(".."),
            Component::Normal(name) => segments.push(name.to_str()?),
        }
    }
    let joined = segments.join("/");
    Some(match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    })
}

/// Expresses absolute `target` relative to absolute directory `base`, as a
/// POSIX string (`"."` when equal; may start with `..`). Both are
/// lexically normalized first. `None` when either is relative, when they
/// sit under different Windows prefixes, or when the result has no POSIX
/// spelling.
pub fn relative_posix(base: &Path, target: &Path) -> Option<String> {
    if !base.is_absolute() || !target.is_absolute() {
        return None;
    }
    let base = lexical_normalize(base);
    let target = lexical_normalize(target);
    let base: Vec<Component<'_>> = base.components().collect();
    let target: Vec<Component<'_>> = target.components().collect();
    // Absolute paths share their root (and, on Windows, must share the
    // prefix); everything after is the walk.
    let shared = base
        .iter()
        .zip(target.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let rooted = |c: &Component<'_>| matches!(c, Component::Prefix(_) | Component::RootDir);
    let roots_match = base
        .iter()
        .filter(|c| rooted(c))
        .eq(target.iter().filter(|c| rooted(c)));
    if !roots_match {
        return None;
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    to_posix(&relative)
}

/// The artifact's stored form of one request path: `cwd.join(arg)`
/// lexically normalized (glob metacharacters preserved verbatim), then
/// expressed relative to `root` as POSIX — possibly with leading `..`.
///
/// `cwd` and `root` must be absolute (`Error::Invariant` otherwise); an
/// argument with no POSIX spelling relative to `root` (a different Windows
/// drive, a non-UTF-8 path) is `Error::Usage`.
pub fn snapshot_path(arg: &str, cwd: &Path, root: &Path) -> Result<String, Error> {
    if !cwd.is_absolute() || !root.is_absolute() {
        return Err(Error::Invariant(format!(
            "snapshot_path needs absolute cwd and root, got {} and {}",
            cwd.display(),
            root.display()
        )));
    }
    let joined = lexical_normalize(&cwd.join(arg));
    relative_posix(root, &joined).ok_or_else(|| {
        Error::Usage(format!(
            "path {arg:?} cannot be expressed relative to the source root {}",
            root.display()
        ))
    })
}

/// POSIX single-quotes `arg` for display in a suggested shell command when
/// it contains anything outside `[A-Za-z0-9_./-]` (or is empty); embedded
/// single quotes become `'\''`.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_posix_collapses_dots_and_parents() {
        assert_eq!(normalize_posix("a/../b.ts"), "b.ts");
        assert_eq!(normalize_posix("./a//b/./c.ts"), "a/b/c.ts");
        assert_eq!(normalize_posix("../x/../../y"), "../../y");
        assert_eq!(normalize_posix("a/.."), ".");
        assert_eq!(normalize_posix(""), ".");
        assert_eq!(normalize_posix("/../a/./b/"), "/a/b");
        assert_eq!(normalize_posix("src/**/*.{ts,js}"), "src/**/*.{ts,js}");
    }

    #[test]
    fn relative_posix_paths_are_recognized() {
        assert!(is_relative_posix("."));
        assert!(is_relative_posix("../src"));
        assert!(is_relative_posix("a/b.ts"));
        assert!(!is_relative_posix(""));
        assert!(!is_relative_posix("/abs"));
        assert!(!is_relative_posix("C:/x"));
        assert!(!is_relative_posix(r"a\b"));
    }

    #[test]
    fn shell_quote_leaves_plain_args_alone() {
        assert_eq!(shell_quote("src/main.rs"), "src/main.rs");
        assert_eq!(shell_quote("a-b_c.D/0"), "a-b_c.D/0");
    }

    #[test]
    fn shell_quote_single_quotes_everything_else() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("my file.ts"), "'my file.ts'");
        assert_eq!(shell_quote("**/*.ts"), "'**/*.ts'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("naïve"), "'naïve'");
    }

    #[cfg(unix)]
    mod unix {
        use super::super::*;

        #[test]
        fn lexical_normalize_never_climbs_above_root() {
            assert_eq!(
                lexical_normalize(Path::new("/a/./b/../c")),
                Path::new("/a/c")
            );
            assert_eq!(lexical_normalize(Path::new("/../..")), Path::new("/"));
            assert_eq!(lexical_normalize(Path::new("../a/..")), Path::new(".."));
            assert_eq!(lexical_normalize(Path::new("./")), Path::new("."));
        }

        #[test]
        fn to_posix_spells_components_with_slashes() {
            assert_eq!(to_posix(Path::new("a/b/../c")).as_deref(), Some("a/b/../c"));
            assert_eq!(to_posix(Path::new("/x/y")).as_deref(), Some("/x/y"));
            assert_eq!(to_posix(Path::new("")).as_deref(), Some("."));
        }

        #[test]
        fn to_posix_rejects_non_utf8() {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let bad = Path::new(OsStr::from_bytes(b"a/\xff.ts"));
            assert_eq!(to_posix(bad), None);
        }

        #[test]
        fn relative_posix_walks_up_and_down() {
            let rel = |a: &str, b: &str| relative_posix(Path::new(a), Path::new(b));
            assert_eq!(rel("/r", "/r").as_deref(), Some("."));
            assert_eq!(rel("/r", "/r/src/a.ts").as_deref(), Some("src/a.ts"));
            assert_eq!(rel("/r/out", "/r/src").as_deref(), Some("../src"));
            assert_eq!(
                rel("/tmp/x", "/home/u/p").as_deref(),
                Some("../../home/u/p")
            );
            assert_eq!(rel("/", "/a").as_deref(), Some("a"));
            assert_eq!(rel("/a/b", "/").as_deref(), Some("../.."));
            assert_eq!(rel("/r/./x/..", "/r/y").as_deref(), Some("y"));
            assert_eq!(rel("r", "/r"), None);
        }

        #[test]
        fn snapshot_path_is_root_relative_and_keeps_globs() {
            let cwd = Path::new("/repo/pkg");
            let root = Path::new("/repo");
            let snap = |arg: &str| snapshot_path(arg, cwd, root).unwrap();
            assert_eq!(snap("."), "pkg");
            assert_eq!(snap("src/**/*.ts"), "pkg/src/**/*.ts");
            assert_eq!(snap("./a/../b.ts"), "pkg/b.ts");
            assert_eq!(snap(".."), ".");
            assert_eq!(snap("/elsewhere/x.ts"), "../elsewhere/x.ts");
            assert_eq!(snap("../other/{a,b}.js"), "other/{a,b}.js");
        }

        #[test]
        fn snapshot_path_requires_absolute_inputs() {
            let err = snapshot_path("a", Path::new("rel"), Path::new("/r")).unwrap_err();
            assert_eq!(err.exit_code(), 7);
            let err = snapshot_path("a", Path::new("/c"), Path::new("r")).unwrap_err();
            assert_eq!(err.exit_code(), 7);
        }
    }
}
