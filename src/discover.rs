//! File discovery: which files a plan run considers.
//!
//! Discovery is gitignore-aware (the `ignore` crate's walker), applies the
//! built-in exclude defaults on top of any `.gitignore`, maps files to
//! languages by extension, and honors the absolute-path rule: the `cwd`
//! (and `glob_base`) parameters are canonicalized once at entry, every
//! argument is joined onto the canonical cwd before any filesystem call,
//! and every recorded path is derived from canonical spellings, so a
//! non-canonical cwd (macOS `/var/…`) never leaks `..` into results.
//!
//! Ambient state is never read: everything arrives through
//! [`DiscoverOptions`].

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::errors::Error;
use crate::lang::extensions::{default_include_globs, language_for};
use crate::lang::ids::LanguageId;
use crate::log::Log;

/// Built-in exclude defaults, applied regardless of any `.gitignore`:
/// generated type/declaration files, minified scripts, and the canonical
/// vendored/build directories.
const DEFAULT_EXCLUDES: &[&str] = &[
    "**/*.{d.ts,d.mts,d.cts}",
    "**/*.pyi",
    "**/*.min.js",
    "**/node_modules/**",
    "**/vendor/**",
    "**/{dist,build,out}/**",
];

/// Directory names pruned during traversal for speed (the `**/name/**`
/// exclude globs remain the semantic source of truth).
const PRUNED_DIR_NAMES: &[&str] = &["node_modules", "vendor", "dist", "build", "out"];

/// Characters that make a non-existent argument a glob pattern.
const GLOB_CHARS: [char; 4] = ['*', '?', '[', '{'];

/// Discovery inputs. `include` replaces the built-in extension set;
/// `exclude` holds user extras (the built-in defaults always apply);
/// `glob_base` is the directory include/exclude globs are matched against
/// (defaults to the canonical cwd; `--resume` passes the stored base so a
/// resumed run selects the same files from any cwd).
pub struct DiscoverOptions<'a> {
    /// Include globs, or `None` for the extension table's default set.
    pub include: Option<&'a [String]>,
    /// User-supplied extra exclude globs.
    pub exclude: &'a [String],
    /// The directory relative paths resolve against (must exist).
    pub cwd: &'a Path,
    /// The directory globs are matched against; `None` = the cwd.
    pub glob_base: Option<&'a Path>,
    /// Where discovery warnings go.
    pub log: &'a dyn Log,
}

/// One discovered file: the POSIX path relative to the canonical cwd, the
/// canonical absolute path, and the language the extension maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// POSIX-style path relative to the canonical `cwd` (may contain `..`
    /// for files outside it; `run_plan` computes the artifact `root`
    /// later).
    pub path: String,
    /// Canonical absolute path.
    pub abs_path: PathBuf,
    /// The language the file's extension maps to.
    pub language: LanguageId,
}

/// The discovery result.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    /// Files to plan, sorted by `path` for deterministic order.
    pub files: Vec<DiscoveredFile>,
    /// How many non-fatal walk errors occurred (each was logged as a
    /// warning and the walk continued).
    pub walk_errors: usize,
}

/// A glob argument resolved against the canonical cwd: the literal
/// directory prefix to walk, and the absolute pattern files must match.
struct ResolvedGlob {
    /// Longest literal directory prefix of the pattern (the walk root).
    root: PathBuf,
    /// The full absolute pattern, compiled with `literal_separator(true)`.
    matcher: GlobMatcher,
}

/// Shared state for one `discover` call: the glob sets, the coordinate
/// roots, the log sink, the error count, and the files found so far (in
/// argument order, deduplicated at the end).
struct Collector<'a> {
    glob_base: &'a Path,
    include: GlobSet,
    default_excludes: GlobSet,
    user_excludes: GlobSet,
    log: &'a dyn Log,
    errors: usize,
    found: Vec<(PathBuf, LanguageId)>,
}

/// Discovers the files a plan run should consider.
///
/// Positional arguments are stat-ed first: existing files are added
/// directly (bypassing gitignore, the include set and the default excludes;
/// user `--exclude` globs still apply, and the file must map to a language
/// or it is a usage error; a symlink argument is resolved to its real
/// path); existing directories are walked literally (glob characters in
/// directory names have no special meaning); a non-existent argument
/// containing glob metacharacters is a glob pattern, resolved lexically to
/// an absolute pattern whose longest literal directory prefix is walked
/// with the usual filters plus a final pattern match; anything else is a
/// usage error. Walk errors never abort discovery: each is logged as a
/// warning and counted.
pub fn discover(paths: &[String], opts: DiscoverOptions<'_>) -> Result<Discovered, Error> {
    let cwd = canonicalize_dir(opts.cwd)?;
    let glob_base = match opts.glob_base {
        Some(base) => canonicalize_dir(base)?,
        None => cwd.clone(),
    };
    let include_globs: Vec<String> = match opts.include {
        Some(globs) => globs.to_vec(),
        None => default_include_globs(),
    };
    let include = build_glob_set(include_globs.iter().map(String::as_str))?;
    let default_excludes = build_glob_set(DEFAULT_EXCLUDES.iter().copied())?;
    let user_excludes = build_glob_set(opts.exclude.iter().map(String::as_str))?;

    let mut collector = Collector {
        glob_base: &glob_base,
        include,
        default_excludes,
        user_excludes,
        log: opts.log,
        errors: 0,
        found: Vec::new(),
    };

    for arg in paths {
        let joined = cwd.join(arg);
        match fs::metadata(&joined) {
            Ok(meta) if meta.is_file() => {
                let abs = canonicalize_existing(&joined)?;
                collector.record_explicit(abs, arg)?;
            }
            Ok(meta) if meta.is_dir() => {
                let root = canonicalize_existing(&joined)?;
                collector.walk_root(&root, None);
            }
            Ok(_) => {
                return Err(Error::Usage(format!(
                    "{arg} is not a regular file or directory",
                )));
            }
            Err(_) => {
                if arg.contains(GLOB_CHARS) {
                    let resolved = resolve_glob_argument(&cwd, arg)?;
                    collector.walk_root(&resolved.root, Some(&resolved.matcher));
                } else {
                    return Err(Error::Usage(format!("path does not exist: {arg}")));
                }
            }
        }
    }

    // Deduplicate by canonical path, first occurrence wins (repeated
    // arguments, an explicit file also found by a walk, or a symlink
    // argument to an already-discovered file all yield one record), then
    // sort for deterministic order.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut files = Vec::new();
    for (abs, language) in collector.found {
        if seen.insert(abs.clone()) {
            files.push(DiscoveredFile {
                path: posix_relative(&cwd, &abs),
                abs_path: abs,
                language,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Discovered {
        files,
        walk_errors: collector.errors,
    })
}

impl Collector<'_> {
    /// Records an explicitly named file: it must map to a language (a usage
    /// error naming the argument otherwise) and survive the user's exclude
    /// globs; everything else is bypassed by design.
    fn record_explicit(&mut self, abs: PathBuf, arg: &str) -> Result<(), Error> {
        let language = language_for(&abs).ok_or_else(|| {
            Error::Usage(format!(
                "{arg} maps to no supported language; discovery covers the built-in extension set only",
            ))
        })?;
        if abs.to_str().is_none() {
            return Err(Error::Usage(format!("{arg} is not valid UTF-8")));
        }
        let rel = posix_relative(self.glob_base, &abs);
        if self.user_excludes.is_match(&rel) {
            return Ok(());
        }
        self.found.push((abs, language));
        Ok(())
    }

    /// Walks one root with the shared walker configuration, optionally
    /// keeping only files that also match a glob argument's absolute
    /// pattern.
    fn walk_root(&mut self, root: &Path, extra: Option<&GlobMatcher>) {
        for entry in build_walk(root) {
            match entry {
                Ok(entry) => self.consider_entry(entry, extra),
                Err(err) => {
                    self.errors += 1;
                    // ignore's error Display carries the offending path
                    // where one exists.
                    self.log.warn(&format!("discovery walk error: {err}"));
                }
            }
        }
    }

    /// Applies the filter chain to one walked entry.
    fn consider_entry(&mut self, entry: ignore::DirEntry, extra: Option<&GlobMatcher>) {
        // The walk root is a directory and never a file candidate.
        if entry.depth() == 0 {
            return;
        }
        let Some(file_type) = entry.file_type() else {
            return;
        };
        // Symlinked files found by traversal are never recorded; symlinked
        // directories are never followed (`follow_links(false)`).
        if file_type.is_symlink() {
            return;
        }
        if !file_type.is_file() {
            return;
        }
        let abs = entry.path();
        if abs.to_str().is_none() {
            self.log.warn(&format!(
                "skipping {}: path is not valid UTF-8",
                abs.display()
            ));
            return;
        }
        // A glob argument additionally requires the absolute POSIX path to
        // match the absolute pattern (identical from any cwd).
        if let Some(matcher) = extra
            && !matcher.is_match(posix_string(abs))
        {
            return;
        }
        // Include/exclude globs are matched against the POSIX path relative
        // to glob_base (the built-in excludes are `**/`-prefixed, so they
        // are base-independent).
        let rel = posix_relative(self.glob_base, abs);
        if !self.include.is_match(&rel) {
            return;
        }
        if self.default_excludes.is_match(&rel) {
            return;
        }
        if self.user_excludes.is_match(&rel) {
            return;
        }
        let Some(language) = language_for(abs) else {
            return;
        };
        self.found.push((abs.to_path_buf(), language));
    }
}

/// The shared walker configuration: skip hidden entries (naming one
/// explicitly is the bypass), never follow symlinks, honor `.gitignore`
/// (and `.git/info/exclude`) even outside a git repo and from parent
/// directories, ignore `.ignore` files, and prune the built-in excluded
/// directory names for speed. The walk root itself is exempt from pruning
/// and from the hidden filter (roots are always yielded).
fn build_walk(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .ignore(false)
        .parents(true)
        .require_git(false)
        .filter_entry(|entry| {
            !(entry.depth() > 0
                && entry.file_type().is_some_and(|kind| kind.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| PRUNED_DIR_NAMES.contains(&name)))
        })
        .build()
}

/// Canonicalizes a directory that must exist (the cwd / glob_base), after
/// checking the absolute-path rule.
fn canonicalize_dir(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::Invariant(format!(
            "discover requires an absolute cwd, got {}",
            path.display(),
        )));
    }
    fs::canonicalize(path).map_err(|source| Error::Fs {
        path: path.to_path_buf(),
        source,
    })
}

/// Canonicalizes a path whose existence was just confirmed (explicit file
/// and directory arguments); resolves symlinks to their real paths.
fn canonicalize_existing(path: &Path) -> Result<PathBuf, Error> {
    fs::canonicalize(path).map_err(|source| Error::Fs {
        path: path.to_path_buf(),
        source,
    })
}

/// Resolves a glob argument against the canonical cwd: joins, normalizes
/// lexically (`.` drops, `..` pops, `..` at the root clamps; glob
/// metacharacters preserved), splits at the first metacharacter-bearing
/// component into a literal walk prefix plus the full absolute pattern.
fn resolve_glob_argument(cwd: &Path, arg: &str) -> Result<ResolvedGlob, Error> {
    let joined = cwd.join(arg);
    let mut components: Vec<OsString> = Vec::new();
    let mut rooted = false;
    for component in joined.components() {
        match component {
            Component::RootDir => rooted = true,
            // Windows drive prefixes are out of scope for v1 (POSIX-only
            // acceptance); a prefixed argument still joins onto the cwd.
            Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(part) => components.push(part.to_os_string()),
        }
    }
    if !rooted {
        return Err(Error::Invariant(format!(
            "glob argument {arg:?} did not join onto an absolute cwd",
        )));
    }
    let first_meta = components
        .iter()
        .position(|part| part.to_str().is_some_and(|text| text.contains(GLOB_CHARS)));
    let literal = match first_meta {
        Some(index) => &components[..index],
        None => &components[..],
    };
    let mut root = PathBuf::from("/");
    for part in literal {
        root.push(part);
    }
    let pattern = format!(
        "/{}",
        posix_join(components.iter().map(OsString::as_os_str))
    );
    let matcher = GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map_err(|err| Error::Usage(format!("invalid glob {arg:?}: {err}")))?
        .compile_matcher();
    Ok(ResolvedGlob { root, matcher })
}

/// Builds a glob set with `literal_separator(true)`; an invalid glob is a
/// usage error naming it.
fn build_glob_set<'a>(globs: impl IntoIterator<Item = &'a str>) -> Result<GlobSet, Error> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        let compiled = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .map_err(|err| Error::Usage(format!("invalid glob {glob:?}: {err}")))?;
        builder.add(compiled);
    }
    builder
        .build()
        .map_err(|err| Error::Invariant(format!("building a glob set failed: {err}")))
}

/// The POSIX string for an absolute path: a leading `/` plus the normal
/// components. Callers have already checked the path is valid UTF-8, so
/// this is exact.
fn posix_string(path: &Path) -> String {
    let parts: Vec<&OsStr> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            // RootDir becomes the leading `/`; CurDir/ParentDir do not
            // occur in canonical paths.
            _ => None,
        })
        .collect();
    format!("/{}", posix_join(parts))
}

/// Joins components with `/` (no leading separator).
fn posix_join<'a>(parts: impl IntoIterator<Item = &'a OsStr>) -> String {
    parts
        .into_iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The POSIX path of `path` relative to `base`, both canonical and
/// absolute: their common prefix is dropped, remaining `base` components
/// become `..`. `.` when the paths are equal.
fn posix_relative(base: &Path, path: &Path) -> String {
    let base: Vec<_> = base.components().collect();
    let path: Vec<_> = path.components().collect();
    let mut common = 0;
    while common < base.len() && common < path.len() && base[common] == path[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..base.len() {
        parts.push("..".to_string());
    }
    for component in &path[common..] {
        parts.push(component.as_os_str().to_string_lossy().to_string());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, Permissions};
    use std::io::Write as _;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use crate::log::CapturingLog;

    fn write_file(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("tests may create dirs");
        }
        let mut file = File::create(&path).expect("tests may create files");
        file.write_all(b"// x\n").expect("tests may write");
    }

    fn discover_default(cwd: &Path) -> Discovered {
        discover(
            &[".".to_string()],
            DiscoverOptions {
                include: None,
                exclude: &[],
                cwd,
                glob_base: None,
                log: &crate::log::NullLog,
            },
        )
        .expect("default discovery succeeds")
    }

    fn discover_with(
        cwd: &Path,
        paths: &[&str],
        include: Option<&[String]>,
        exclude: &[&str],
        glob_base: Option<&Path>,
        log: &dyn Log,
    ) -> Discovered {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        let exclude: Vec<String> = exclude.iter().map(|p| (*p).to_string()).collect();
        discover(
            &paths,
            DiscoverOptions {
                include,
                exclude: &exclude,
                cwd,
                glob_base,
                log,
            },
        )
        .expect("discovery succeeds")
    }

    fn rel_paths(discovered: &Discovered) -> Vec<&str> {
        discovered.files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn nested_directories_are_discovered_in_sorted_order() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "a.ts");
        write_file(dir.path(), "src/deep/nested/b.rs");
        write_file(dir.path(), "src/c.go");
        let discovered = discover_default(dir.path());
        assert_eq!(
            rel_paths(&discovered),
            ["a.ts", "src/c.go", "src/deep/nested/b.rs"]
        );
        assert_eq!(discovered.walk_errors, 0);
    }

    #[test]
    fn gitignored_entries_are_excluded_but_an_explicit_gitignored_file_is_included() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), ".gitignore");
        std::fs::write(dir.path().join(".gitignore"), "ignored.ts\n").expect("tests may write");
        write_file(dir.path(), "ignored.ts");
        write_file(dir.path(), "kept.ts");
        let discovered = discover_default(dir.path());
        assert_eq!(rel_paths(&discovered), ["kept.ts"]);

        let explicit = discover_with(
            dir.path(),
            &["ignored.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&explicit), ["ignored.ts"]);
    }

    #[test]
    fn vendored_and_build_directories_are_excluded_without_any_gitignore() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "node_modules/pkg/index.js");
        write_file(dir.path(), "vendor/lib.go");
        write_file(dir.path(), "packages/x/dist/bundle.ts");
        write_file(dir.path(), "packages/x/src/real.ts");
        write_file(dir.path(), "app.rs");
        let discovered = discover_default(dir.path());
        assert_eq!(rel_paths(&discovered), ["app.rs", "packages/x/src/real.ts"]);
    }

    #[test]
    fn generated_type_and_declaration_files_are_excluded() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "types.d.ts");
        write_file(dir.path(), "types.d.mts");
        write_file(dir.path(), "types.d.cts");
        write_file(dir.path(), "stub.pyi");
        write_file(dir.path(), "app.min.js");
        write_file(dir.path(), "app.ts");
        let discovered = discover_default(dir.path());
        assert_eq!(rel_paths(&discovered), ["app.ts"]);
    }

    #[test]
    fn include_override_replaces_the_default_set() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "a.py");
        write_file(dir.path(), "b.ts");
        let include = vec!["**/*.py".to_string()];
        let discovered = discover_with(
            dir.path(),
            &["."],
            Some(&include),
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&discovered), ["a.py"]);
    }

    #[test]
    fn glob_characters_in_directory_names_have_no_special_meaning() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "app/[id]/route.ts");
        // Named explicitly (the directory exists, so it is walked
        // literally); the recorded path stays relative to the cwd, not the
        // walk root.
        let named = discover_with(
            dir.path(),
            &["app/[id]"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&named), ["app/[id]/route.ts"]);
        // And found by a plain walk of the tree.
        let walked = discover_default(dir.path());
        assert_eq!(rel_paths(&walked), ["app/[id]/route.ts"]);
    }

    #[test]
    fn dot_directories_are_skipped_by_traversal_but_included_when_named() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), ".cache/inner.ts");
        write_file(dir.path(), "visible.ts");
        let discovered = discover_default(dir.path());
        assert_eq!(rel_paths(&discovered), ["visible.ts"]);

        let named = discover_with(
            dir.path(),
            &[".cache"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&named), [".cache/inner.ts"]);
    }

    #[test]
    fn unknown_extensions_are_never_discovered_and_languages_map_by_extension() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "notes.txt");
        write_file(dir.path(), "mod.rs");
        write_file(dir.path(), "script.py");
        write_file(dir.path(), "main.go");
        let discovered = discover_default(dir.path());
        assert_eq!(rel_paths(&discovered), ["main.go", "mod.rs", "script.py"]);
        let languages: Vec<_> = discovered.files.iter().map(|f| f.language).collect();
        assert_eq!(
            languages,
            [LanguageId::Go, LanguageId::Rust, LanguageId::Python],
        );
    }

    #[test]
    fn a_nonexistent_non_glob_argument_is_a_usage_error() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        let err = discover(
            &["ghost.ts".to_string()],
            DiscoverOptions {
                include: None,
                exclude: &[],
                cwd: dir.path(),
                glob_base: None,
                log: &crate::log::NullLog,
            },
        )
        .expect_err("nonexistent non-glob argument is rejected");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("ghost.ts"));
    }

    #[test]
    fn an_explicit_file_with_an_unmapped_extension_is_a_usage_error() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "data.txt");
        let err = discover(
            &["data.txt".to_string()],
            DiscoverOptions {
                include: None,
                exclude: &[],
                cwd: dir.path(),
                glob_base: None,
                log: &crate::log::NullLog,
            },
        )
        .expect_err("unmapped explicit file is rejected");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("data.txt"));
    }

    #[test]
    fn glob_arguments_honor_gitignore_and_default_excludes() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        std::fs::write(dir.path().join(".gitignore"), "skipme.ts\n").expect("tests may write");
        write_file(dir.path(), "skipme.ts");
        write_file(dir.path(), "keep.ts");
        write_file(dir.path(), "gen.d.ts");
        write_file(dir.path(), "sub/nested.ts");
        let discovered = discover_with(
            dir.path(),
            &["**/*.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&discovered), ["keep.ts", "sub/nested.ts"]);
    }

    #[test]
    fn a_relative_glob_means_the_same_thing_from_any_cwd() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "x/a.ts");
        write_file(dir.path(), "x/sub/b.ts");
        write_file(dir.path(), "x/c.js");
        std::fs::create_dir_all(dir.path().join("sib")).expect("tests may create dirs");

        // From the sibling directory: ../x/**/*.ts
        let from_sibling = discover_with(
            dir.path().join("sib").as_path(),
            &["../x/**/*.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        // From the parent: x/**/*.ts
        let from_parent = discover_with(
            dir.path(),
            &["x/**/*.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );

        let abs_of = |d: &Discovered| -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = d.files.iter().map(|f| f.abs_path.clone()).collect();
            paths.sort();
            paths
        };
        assert_eq!(abs_of(&from_sibling), abs_of(&from_parent));
        assert_eq!(rel_paths(&from_sibling), ["../x/a.ts", "../x/sub/b.ts"]);
    }

    #[test]
    fn user_excludes_match_against_glob_base_not_cwd() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "sub/s.ts");
        write_file(dir.path(), "wd/w.ts");
        let exclude = ["sub/**"];

        // Without the exclude, the nested walk finds both files.
        let no_exclude = discover_with(
            dir.path().join("wd").as_path(),
            &[".."],
            None,
            &[],
            Some(dir.path()),
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&no_exclude), ["../sub/s.ts", "w.ts"]);

        // With "sub/**" matched against glob_base (dir), sub/s.ts is
        // excluded from the nested cwd exactly as it is from the base
        // itself — same file set either way.
        let from_nested = discover_with(
            dir.path().join("wd").as_path(),
            &[".."],
            None,
            &exclude,
            Some(dir.path()),
            &crate::log::NullLog,
        );
        let from_base = discover_with(
            dir.path(),
            &["."],
            None,
            &exclude,
            Some(dir.path()),
            &crate::log::NullLog,
        );

        assert_eq!(rel_paths(&from_nested), ["w.ts"]);
        assert_eq!(rel_paths(&from_base), ["wd/w.ts"]);
        // Same file sets (relative to their different cwds).
        let abs_of = |d: &Discovered| -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = d.files.iter().map(|f| f.abs_path.clone()).collect();
            paths.sort();
            paths
        };
        assert_eq!(abs_of(&from_nested), abs_of(&from_base));

        // With the default glob base (the nested cwd), the same exclude
        // pattern no longer reaches ../sub — relativity to glob_base is
        // what makes a resume from any cwd select the same files.
        let default_base = discover_with(
            dir.path().join("wd").as_path(),
            &[".."],
            None,
            &exclude,
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&default_base), ["../sub/s.ts", "w.ts"]);
    }

    #[test]
    fn an_explicit_file_excluded_by_the_user_is_skipped_but_defaults_do_not_apply() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "node_modules/pkg.ts");
        let exclude = ["node_modules/**"];
        // Default excludes do not apply to explicit files; user excludes do.
        let defaults_bypassed = discover_with(
            dir.path(),
            &["node_modules/pkg.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&defaults_bypassed), ["node_modules/pkg.ts"]);
        let user_excluded = discover_with(
            dir.path(),
            &["node_modules/pkg.ts"],
            None,
            &exclude,
            None,
            &crate::log::NullLog,
        );
        assert!(user_excluded.files.is_empty());
    }

    #[test]
    fn repeated_and_overlapping_arguments_yield_each_file_once() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "src/a.ts");
        write_file(dir.path(), "src/b.ts");
        let discovered = discover_with(
            dir.path(),
            &["src/", "src", ".", "src/a.ts", "./src/a.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&discovered), ["src/a.ts", "src/b.ts"]);
    }

    #[test]
    fn a_non_canonical_cwd_never_leaks_dotdot_into_paths() {
        // tempfile on macOS hands out /var/... (a symlink to /private/var).
        // Pass that spelling verbatim: canonicalization must keep every
        // recorded path clean.
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "a.ts");
        write_file(dir.path(), "sub/b.ts");
        let discovered = discover_default(dir.path());
        for file in &discovered.files {
            assert!(!file.path.contains(".."), "path leaked: {}", file.path);
            assert!(file.abs_path.is_absolute());
            assert!(
                file.abs_path
                    .starts_with(fs::canonicalize(dir.path()).unwrap()),
                "{} is not under the canonical cwd",
                file.abs_path.display(),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed_and_symlink_arguments_dedupe() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "real/r.ts");
        symlink("real", dir.path().join("link")).expect("tests may symlink");
        write_file(dir.path(), "target.ts");
        symlink("target.ts", dir.path().join("alias.ts")).expect("tests may symlink");

        // Traversal neither follows the symlinked directory nor records
        // the symlinked file.
        let discovered = discover_default(dir.path());
        assert_eq!(rel_paths(&discovered), ["real/r.ts", "target.ts"]);

        // A symlink argument resolves to its target and dedupes against it.
        let with_alias = discover_with(
            dir.path(),
            &[".", "alias.ts", "target.ts"],
            None,
            &[],
            None,
            &crate::log::NullLog,
        );
        assert_eq!(rel_paths(&with_alias), ["real/r.ts", "target.ts"]);
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_subdirectories_are_warned_counted_and_skipped() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "kept.ts");
        write_file(dir.path(), "locked/inner.ts");
        let locked = dir.path().join("locked");
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("tests may chmod");

        let log = CapturingLog::new();
        let discovered = discover(
            &[".".to_string()],
            DiscoverOptions {
                include: None,
                exclude: &[],
                cwd: dir.path(),
                glob_base: None,
                log: &log,
            },
        )
        .expect("discovery survives the unreadable directory");

        // Restore permissions so the temp dir cleans up.
        fs::set_permissions(&locked, Permissions::from_mode(0o755)).expect("tests may chmod");

        assert_eq!(discovered.walk_errors, 1, "warnings: {:?}", log.lines());
        assert_eq!(rel_paths(&discovered), ["kept.ts"]);
        assert!(
            log.lines().iter().any(|line| line.contains("locked")),
            "the warning names the offending path: {:?}",
            log.lines(),
        );
    }

    #[test]
    fn a_glob_whose_literal_prefix_is_missing_is_a_walk_error_not_a_crash() {
        let dir = tempfile::tempdir().expect("tests may create temp dirs");
        write_file(dir.path(), "a.ts");
        let log = CapturingLog::new();
        let discovered = discover(
            &["ghost*/x*.ts".to_string()],
            DiscoverOptions {
                include: None,
                exclude: &[],
                cwd: dir.path(),
                glob_base: None,
                log: &log,
            },
        )
        .expect("a missing glob prefix is not fatal");
        // The literal prefix here is the cwd itself (the first component
        // carries a metacharacter), so nothing is walked and nothing fails;
        // a pattern with a deeper missing literal prefix reports a walk
        // error instead of aborting.
        let deeper = discover(
            &["ghost/x*.ts".to_string()],
            DiscoverOptions {
                include: None,
                exclude: &[],
                cwd: dir.path(),
                glob_base: None,
                log: &log,
            },
        )
        .expect("a missing glob prefix is not fatal");
        assert_eq!(deeper.walk_errors, 1);
        assert!(deeper.files.is_empty());
        assert!(discovered.files.is_empty());
    }
}
