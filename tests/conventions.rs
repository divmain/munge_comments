//! Conventions test: guards the plan's structural rules that rustc and
//! clippy cannot see.
//!
//! - tree-sitter usage is confined to `src/lang/registry.rs`;
//! - adapters under `src/lang/adapters/` are data only (no registry mention);
//! - `.rs` file names under `src/` and `tests/` are snake_case;
//! - `Cargo.toml` dependencies stay within the plan's allow-list, grammar
//!   and tree-sitter crates stay exact-pinned, and no dependency is renamed;
//! - no default-path output artifact (`munge-comments-*.json` / `*.md`)
//!   leaks into the package root (the gitignore would hide it from
//!   `git status`, so the leak must fail a gate instead).
//!
//! Rust sources are matched only after `//` and `/* */` comments and string
//! literals are stripped, so prose (e.g. an adapter doc discussing the
//! tree-sitter ban) never trips a rule. Rules whose target files don't exist
//! yet pass vacuously.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// The only file allowed to reference the tree-sitter crates.
const TREE_SITTER_HOME: &str = "src/lang/registry.rs";

/// `[dependencies]` entries allowed by the plan's dependency table — the
/// complete allow-list, hard-coded here because the plan document is
/// gitignored and absent from clean checkouts. Adding a dependency to the
/// crate means changing this table and the plan together.
const ALLOWED_DEPENDENCIES: &[&str] = &[
    "tree-sitter",
    "tree-sitter-javascript",
    "tree-sitter-typescript",
    "tree-sitter-python",
    "tree-sitter-go",
    "tree-sitter-java",
    "tree-sitter-c",
    "tree-sitter-cpp",
    "tree-sitter-rust",
    "clap",
    "ignore",
    "globset",
    "serde",
    "serde_json",
    "schemars",
    "reqwest",
    "tokio",
    "tokio-util",
    "futures",
    "thiserror",
    "regex",
    "unicode-segmentation",
    "sha2",
    "tempfile",
];

/// `[dev-dependencies]` entries allowed by the plan's dependency table.
const ALLOWED_DEV_DEPENDENCIES: &[&str] = &["insta", "proptest", "wiremock", "tokio"];

static USE_TREE_SITTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:extern\s+crate|use)\s+tree_sitter\w*").expect("valid regex")
});
static TREE_SITTER_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\btree_sitter\w*\s*::").expect("valid regex"));
static REGISTRY_MENTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bregistry\b").expect("valid regex"));
static DEP_RENAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bpackage\s*=\s*""#).expect("valid regex"));
static DEP_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bversion\s*=\s*"([^"]*)""#).expect("valid regex"));

#[test]
fn tree_sitter_stays_inside_the_registry() {
    let mut violations = Vec::new();
    for (rel, path) in walked_rs_files() {
        if rel == TREE_SITTER_HOME {
            continue;
        }
        let stripped = strip_comments_and_strings(&read_source(&path));
        violations.extend(tree_sitter_violations(&rel, &stripped));
    }
    assert!(
        violations.is_empty(),
        "tree-sitter must only be used by {TREE_SITTER_HOME}:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn adapters_are_data_only() {
    let mut violations = Vec::new();
    for (rel, path) in walked_rs_files() {
        if !rel.starts_with("src/lang/adapters/") {
            continue;
        }
        let stripped = strip_comments_and_strings(&read_source(&path));
        violations.extend(registry_violations(&rel, &stripped));
    }
    assert!(
        violations.is_empty(),
        "adapter data modules must not mention the registry:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn rs_file_names_are_snake_case() {
    let mut violations = Vec::new();
    for (rel, path) in walked_rs_files() {
        let stem = path
            .file_stem()
            .expect("a walked file always has a stem")
            .to_string_lossy();
        if !is_snake_case(&stem) {
            violations.push(format!("{rel}: file name `{stem}` is not snake_case"));
        }
    }
    assert!(
        violations.is_empty(),
        "file names must be snake_case (kebab-case is reserved for the \
         serialized vocabulary and fixtures):\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn cargo_dependencies_stay_within_the_allow_list() {
    let cargo_toml = read_source(&Path::new(MANIFEST_DIR).join("Cargo.toml"));
    let violations = cargo_violations(&cargo_toml);
    assert!(
        violations.is_empty(),
        "Cargo.toml dependency conventions violated:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn no_default_output_artifacts_leak_into_the_package_root() {
    let mut leaks = Vec::new();
    for entry in fs::read_dir(MANIFEST_DIR).expect("the package root is readable") {
        let entry = entry.expect("the package root is listable");
        let name = entry.file_name();
        let Ok(name) = name.into_string() else {
            continue;
        };
        let is_leak = name.starts_with("munge-comments-")
            && (name.ends_with(".json") || name.ends_with(".md"))
            && entry
                .file_type()
                .expect("every dir entry has a type")
                .is_file();
        if is_leak {
            leaks.push(name);
        }
    }
    assert!(
        leaks.is_empty(),
        "a test wrote a default output path into the package root \
         (gitignore would hide it from git status): {leaks:?}"
    );
}

// ---------------------------------------------------------------------------
// Pattern checks (pure, so they are unit-testable on synthetic input)
// ---------------------------------------------------------------------------

fn tree_sitter_violations(rel: &str, stripped: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in USE_TREE_SITTER.find_iter(stripped) {
        out.push(format!(
            "{rel}: `{}` imports a tree-sitter crate; only {TREE_SITTER_HOME} may",
            m.as_str()
        ));
    }
    for m in TREE_SITTER_PATH.find_iter(stripped) {
        out.push(format!(
            "{rel}: `{}` names a tree-sitter path; only {TREE_SITTER_HOME} may",
            m.as_str()
        ));
    }
    out
}

fn registry_violations(rel: &str, stripped: &str) -> Vec<String> {
    REGISTRY_MENTION
        .find_iter(stripped)
        .map(|m| format!("{rel}: `{}` — adapter modules are data only", m.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// Cargo.toml dependency entries
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum DepSection {
    Dependencies,
    DevDependencies,
}

impl DepSection {
    fn allowed(self) -> &'static [&'static str] {
        match self {
            DepSection::Dependencies => ALLOWED_DEPENDENCIES,
            DepSection::DevDependencies => ALLOWED_DEV_DEPENDENCIES,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DepSection::Dependencies => "dependencies",
            DepSection::DevDependencies => "dev-dependencies",
        }
    }
}

struct DepEntry {
    name: String,
    /// The full `name = value` text (continuation lines joined with spaces,
    /// comments removed) — used for the rename and version checks.
    text: String,
}

fn cargo_violations(cargo_toml: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (section, entry) in parse_dep_entries(cargo_toml) {
        let allowed = section.allowed();
        if !allowed.contains(&entry.name.as_str()) {
            violations.push(format!(
                "[{}] `{}` is outside the plan's dependency allow-list",
                section.label(),
                entry.name
            ));
        }
        if entry.name.starts_with("tree-sitter") {
            match declared_version(&entry.text) {
                Some(v) if v.starts_with('=') => {}
                Some(v) => violations.push(format!(
                    "`{}` is a grammar/tree-sitter crate and must be exact-pinned, found `{v}`",
                    entry.name
                )),
                None => violations.push(format!(
                    "`{}` is a grammar/tree-sitter crate and must carry an exact `=` version pin",
                    entry.name
                )),
            }
        }
        if DEP_RENAME.is_match(&entry.text) {
            violations.push(format!(
                "`{}` is renamed via `package = …`; renamed dependencies are not allowed",
                entry.name
            ));
        }
    }
    violations
}

/// Parses every `[dependencies]`/`[dev-dependencies]` entry — including the
/// `[target.*.dependencies]` variants and `[dependencies.<name>]`
/// sub-tables — into `(section, entry)` records. Understands the TOML shapes
/// a Cargo.toml realistically uses for dependencies: bare versions, inline
/// tables (single- or multi-line), multi-line arrays and sub-tables.
/// Multi-line basic strings are out of scope (never used for dependencies).
fn parse_dep_entries(cargo_toml: &str) -> Vec<(DepSection, DepEntry)> {
    struct Pending {
        section: DepSection,
        name: String,
        text: String,
        /// A sub-table entry (`[dependencies.foo]`) stays open across its
        /// field lines; an inline-table entry closes when its brackets
        /// balance.
        subtable: bool,
    }

    let mut out: Vec<(DepSection, DepEntry)> = Vec::new();
    let mut section: Option<String> = None;
    let mut pending: Option<Pending> = None;
    let mut depth: i32 = 0;

    for raw in cargo_toml.lines() {
        let line = strip_trailing_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if depth > 0 {
            // continuation line of a multi-line value
            if let Some(p) = pending.as_mut() {
                p.text.push(' ');
                p.text.push_str(line);
            }
            depth += net_brackets(line);
            if depth <= 0 {
                depth = 0;
                if let Some(p) = pending.take() {
                    if !p.subtable {
                        out.push((
                            p.section,
                            DepEntry {
                                name: p.name,
                                text: p.text,
                            },
                        ));
                    } else {
                        pending = Some(p); // more fields may follow
                    }
                }
            }
            continue;
        }

        // a section header closes any open sub-table entry
        if let Some(label) = section_label(line) {
            if let Some(p) = pending.take() {
                out.push((
                    p.section,
                    DepEntry {
                        name: p.name,
                        text: p.text,
                    },
                ));
            }
            section = Some(label.to_string());
            if let Some((kind, name)) = dep_subtable(label) {
                pending = Some(Pending {
                    section: kind,
                    name,
                    text: String::new(),
                    subtable: true,
                });
            }
            continue;
        }

        // inside a sub-table entry: every key line is a field of the entry
        if let Some(p) = pending.as_mut() {
            if !p.text.is_empty() {
                p.text.push(' ');
            }
            p.text.push_str(line);
            depth = net_brackets(line).max(0);
            continue;
        }

        // a plain dependency entry inside a [dependencies]-shaped section
        let Some(label) = section.as_deref() else {
            continue;
        };
        let Some(kind) = dep_section_kind(label) else {
            continue;
        };
        let Some((name, _rest)) = split_key(line) else {
            continue;
        };
        let entry = DepEntry {
            name: name.to_string(),
            text: line.to_string(),
        };
        depth = net_brackets(line);
        if depth <= 0 {
            depth = 0;
            out.push((kind, entry));
        } else {
            pending = Some(Pending {
                section: kind,
                name: entry.name,
                text: entry.text,
                subtable: false,
            });
        }
    }
    if let Some(p) = pending.take() {
        out.push((
            p.section,
            DepEntry {
                name: p.name,
                text: p.text,
            },
        ));
    }
    out
}

/// `[label]` / `[[label]]` → `Some("label")` for header lines, `None`
/// otherwise.
fn section_label(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    Some(
        inner
            .strip_prefix('[')
            .unwrap_or(inner)
            .strip_suffix(']')
            .unwrap_or(inner),
    )
}

/// Which dependency table a section header opens (including
/// `[target.'…'.dependencies]` variants); `None` for everything else.
fn dep_section_kind(label: &str) -> Option<DepSection> {
    if label == "dev-dependencies" || label.ends_with(".dev-dependencies") {
        Some(DepSection::DevDependencies)
    } else if label == "dependencies" || label.ends_with(".dependencies") {
        Some(DepSection::Dependencies)
    } else {
        None
    }
}

/// A `[dependencies.<name>]` (or `[target.…dependencies.<name>]`) sub-table
/// header, read as the start of an entry named `<name>`.
fn dep_subtable(label: &str) -> Option<(DepSection, String)> {
    for (marker, kind) in [
        (".dev-dependencies.", DepSection::DevDependencies),
        (".dependencies.", DepSection::Dependencies),
    ] {
        if let Some(idx) = label.find(marker) {
            let name = &label[idx + marker.len()..];
            if !name.is_empty() {
                return Some((kind, name.to_string()));
            }
        }
    }
    if let Some(name) = label.strip_prefix("dev-dependencies.") {
        return Some((DepSection::DevDependencies, name.to_string()));
    }
    if let Some(name) = label.strip_prefix("dependencies.") {
        return Some((DepSection::Dependencies, name.to_string()));
    }
    None
}

/// `key = value` → `Some((key, trimmed value))` for TOML bare keys.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    let is_bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_bare {
        Some((key, rest.trim()))
    } else {
        None
    }
}

/// The version requirement an entry declares: the bare `name = "x"` form or
/// the `version = "x"` field of a table entry.
fn declared_version(entry_text: &str) -> Option<String> {
    if let Some((_, rest)) = split_key(entry_text)
        && let Some(v) = rest.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
    {
        return Some(v.to_string());
    }
    DEP_VERSION
        .captures(entry_text)
        .map(|caps| caps[1].to_string())
}

/// Cuts a trailing `#` comment that begins outside any quoted string.
fn strip_trailing_comment(line: &str) -> &str {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escaped = false;
    for (idx, c) in line.char_indices() {
        if in_basic {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_basic = false;
            }
        } else if in_literal {
            if c == '\'' {
                in_literal = false;
            }
        } else if c == '"' {
            in_basic = true;
        } else if c == '\'' {
            in_literal = true;
        } else if c == '#' {
            return &line[..idx];
        }
    }
    line
}

/// Net `{`/`[`-minus-`}`/`]` count outside quoted strings, for tracking
/// multi-line values.
fn net_brackets(s: &str) -> i32 {
    let mut depth = 0;
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_basic {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_basic = false;
            }
        } else if in_literal {
            if c == '\'' {
                in_literal = false;
            }
        } else {
            match c {
                '"' => in_basic = true,
                '\'' => in_literal = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
        }
    }
    depth
}

// ---------------------------------------------------------------------------
// Rust source walking and stripping
// ---------------------------------------------------------------------------

/// Every `.rs` file under `src/` and `tests/` as `(POSIX-relative path,
/// absolute path)`, excluding the `tests/fixtures/` subtree (fixture trees
/// hold .rs sources with deliberately unconventional names) and this test
/// file itself (it carries the very patterns it checks for).
///
/// A root that doesn't exist yet yields no files, so its rules pass
/// vacuously.
fn walked_rs_files() -> Vec<(String, PathBuf)> {
    let root = Path::new(MANIFEST_DIR);
    let mut out = Vec::new();
    walk(&root.join("src"), "src", &mut out);
    walk(&root.join("tests"), "tests", &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, rel_dir: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return; // the directory doesn't exist yet: its rules pass vacuously
    };
    let mut entries: Vec<_> = entries
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()));
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{rel_dir}/{name}");
        if rel == "tests/fixtures" {
            continue; // excluded wholesale
        }
        let path = entry.path();
        if entry
            .file_type()
            .unwrap_or_else(|err| panic!("stating {}: {err}", path.display()))
            .is_dir()
        {
            walk(&path, &rel, out);
        } else if rel.ends_with(".rs") && rel != "tests/conventions.rs" {
            out.push((rel, path));
        }
    }
}

fn read_source(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

fn is_snake_case(stem: &str) -> bool {
    !stem.is_empty()
        && !stem.starts_with(|c: char| c.is_ascii_digit())
        && stem
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Strips `//` and `/* */` comments (nesting-aware) and string literals —
/// basic, byte, raw and char literals — from Rust source, replacing each
/// stripped span with a single space. Doc comments are comments; lifetimes
/// and labels are left alone.
fn strip_comments_and_strings(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        let prev_is_ident = i > 0 && is_ident_char(chars[i - 1]);

        // line comment (incl. /// and //!)
        if chars[i] == '/' && next(&chars, i) == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }

        // block comment (incl. /** and /*!), nesting-aware
        if chars[i] == '/' && next(&chars, i) == Some('*') {
            let mut depth = 1;
            i += 2;
            while i < chars.len() && depth > 0 {
                if chars[i] == '/' && next(&chars, i) == Some('*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && next(&chars, i) == Some('/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }

        // raw string `r"…"` / `r#"…"#` / byte-raw `br"…"`, but only when the
        // `r` (or `br`) is not the tail of a longer identifier
        let raw_content = if chars[i] == 'r' && !prev_is_ident {
            Some(i + 1)
        } else if chars[i] == 'b' && next(&chars, i) == Some('r') && !prev_is_ident {
            Some(i + 2)
        } else {
            None
        };
        if let Some(start) = raw_content
            && let Some(end) = raw_string_end(&chars, start)
        {
            i = end;
            out.push(' ');
            continue;
        }

        // basic string `"…"` (the `b` of a byte string is ordinary text)
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                } else if chars[i] == '"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }

        // char literal `'x'` / `'\n'` / `'\u{1F}'`; lifetimes and labels are
        // left alone
        if chars[i] == '\''
            && let Some(end) = char_literal_end(&chars, i)
        {
            i = end;
            out.push(' ');
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn next(chars: &[char], i: usize) -> Option<char> {
    chars.get(i + 1).copied()
}

/// If a raw string body starts at `start` (just past the `r`), returns the
/// index just past its closing `"###`; `None` when no raw string starts
/// there. An unterminated raw string strips to end of input.
fn raw_string_end(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    let mut hashes = 0;
    while i < chars.len() && chars[i] == '#' {
        hashes += 1;
        i += 1;
    }
    if chars.get(i) != Some(&'"') {
        return None;
    }
    i += 1;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut j = i + 1;
            let mut seen = 0;
            while seen < hashes && chars.get(j) == Some(&'#') {
                seen += 1;
                j += 1;
            }
            if seen == hashes {
                return Some(j);
            }
        }
        i += 1;
    }
    Some(chars.len())
}

/// If a char literal starts at `i`, returns the index just past its closing
/// quote; `None` for lifetimes (`'a`), labels (`'outer:`) and anything else
/// that is not a char literal.
fn char_literal_end(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    match chars.get(j) {
        Some('\\') => {
            j += 1;
            match chars.get(j) {
                Some('x') => j += 3, // `x` + two hex digits
                Some('u') => {
                    if chars.get(j + 1) != Some(&'{') {
                        return None;
                    }
                    j += 2;
                    while j < chars.len() && chars[j] != '}' {
                        j += 1;
                    }
                    j += 1; // past '}'
                }
                Some(_) => j += 1, // single-character escape
                None => return None,
            }
        }
        Some(_) => j += 1, // one scalar value
        None => return None,
    }
    if chars.get(j) == Some(&'\'') {
        Some(j + 1)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the stripping and parsing machinery itself
// ---------------------------------------------------------------------------

#[test]
fn stripping_removes_comments_and_string_literals() {
    let src = r##"fn f<'a>() {
    let a = 1; // use tree_sitter::Parser
    /// doc prose: tree_sitter, registry
    /* block /* nested */ use crate::lang::registry */
    let s = "tree_sitter::Parser";
    let raw = r#"tree_sitter registry"#;
    let byte = b"tree_sitter";
    let ch = 'x';
    let esc = '\u{1F600}';
    let quote = '\'';
    let lt: &'a str = s;
    let keep = reuse + vr;
}"##;
    let stripped = strip_comments_and_strings(src);
    assert!(!stripped.contains("tree_sitter"), "stripped: {stripped}");
    assert!(!stripped.contains("registry"), "stripped: {stripped}");
    assert!(stripped.contains("'a"), "lifetimes survive: {stripped}");
    assert!(
        stripped.contains("let keep = reuse + vr"),
        "code survives: {stripped}"
    );
}

#[test]
fn tree_sitter_detection_catches_use_and_paths() {
    for src in [
        "use tree_sitter::Parser;",
        "pub use tree_sitter as ts;",
        "extern crate tree_sitter_javascript;",
        "fn f() { tree_sitter::Parser::new(); }",
        "use\n    tree_sitter::Query;",
    ] {
        assert!(
            !tree_sitter_violations("src/probe.rs", src).is_empty(),
            "expected a violation for {src:?}"
        );
    }
    let clean = "let x = my_tree_sitter_thing; use other::Path; let r = registry_like();";
    assert!(tree_sitter_violations("src/probe.rs", clean).is_empty());
}

#[test]
fn registry_detection_catches_mentions_only_in_adapter_files() {
    for src in ["use crate::lang::registry;", "let r = Registry::new();"] {
        assert!(
            !registry_violations("src/lang/adapters/probe.rs", src).is_empty(),
            "expected a violation for {src:?}"
        );
    }
    assert!(registry_violations("src/probe.rs", "let reg = registrar::new();").is_empty());
}

#[test]
fn dependency_parser_handles_the_toml_shapes_that_matter() {
    let toml = r#"
[package]
name = "probe"

[dependencies]
tree-sitter = "=0.27.1"
serde = { version = "1", features = ["derive"] }
anyhow = { version = "1",
    default-features = false }

[dependencies.sneaky]
version = "1"

[target.'cfg(unix)'.dev-dependencies]
extra = "1"

[dev-dependencies]
tokio = { version = "1", features = ["macros"] }
"#;
    let entries = parse_dep_entries(toml);
    let names: Vec<&str> = entries.iter().map(|(_, e)| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["tree-sitter", "serde", "anyhow", "sneaky", "extra", "tokio"]
    );
    let sections: Vec<&str> = entries.iter().map(|(k, _)| k.label()).collect();
    assert_eq!(
        sections,
        [
            "dependencies",
            "dependencies",
            "dependencies",
            "dependencies",
            "dev-dependencies",
            "dev-dependencies",
        ]
    );
    // the multi-line inline table and the sub-table kept their full text
    assert!(entries[2].1.text.contains("default-features = false"));
    assert_eq!(entries[3].1.text, "version = \"1\"");
    let violations = cargo_violations(toml);
    for expected in [
        "[dependencies] `anyhow` is outside the plan's dependency allow-list",
        "`sneaky` is outside", // substring: section label varies
        "`extra` is outside",
    ] {
        assert!(
            violations.iter().any(|v| v.contains(expected)),
            "expected a violation containing {expected:?}, got {violations:?}"
        );
    }
    // tree-sitter's exact pin and allowed entries produce no violations
    assert!(!violations.iter().any(|v| v.contains("tree-sitter")));
    assert!(!violations.iter().any(|v| v.contains("`serde`")));
    assert!(!violations.iter().any(|v| v.contains("`tokio`")));
}

#[test]
fn unpinned_grammar_crates_and_renames_are_rejected() {
    let toml = r#"
[dependencies]
tree-sitter-rust = "0.24"
tree-sitter-go = { version = "0.25" }
renamed = { version = "1", package = "real-name" }
"#;
    let violations = cargo_violations(toml);
    assert!(
        violations
            .iter()
            .any(|v| v.contains("must be exact-pinned, found `0.24`"))
    );
    assert!(
        violations
            .iter()
            .any(|v| v.contains("must be exact-pinned, found `0.25`"))
    );
    assert!(
        violations
            .iter()
            .any(|v| v.contains("`renamed` is renamed"))
    );
    assert!(
        violations
            .iter()
            .any(|v| v.contains("`renamed` is outside"))
    );
}
