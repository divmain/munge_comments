//! THE extension → language table, plus the default include globs derived
//! from it.
//!
//! This module is the single source of truth for language mapping by
//! extension: discovery, artifact validation, and the CLI's hand-written
//! `--include` help text (guarded by a test) all agree with this table.
//! `.h` maps to the C++ grammar per the design's language mapping (a C++
//! raw string in a header parses cleanly under cpp; under c it would hide
//! a phantom comment inside the string).

use std::path::Path;

use crate::lang::ids::LanguageId;

/// The extension table. Order is the canonical order of the default include
/// set (see [`default_include_globs`]); `.d.ts` maps to typescript and is
/// excluded separately, by discovery's built-in exclude defaults.
const EXTENSIONS: &[(&str, LanguageId)] = &[
    ("ts", LanguageId::Typescript),
    ("tsx", LanguageId::Tsx),
    ("mts", LanguageId::Typescript),
    ("cts", LanguageId::Typescript),
    ("js", LanguageId::Javascript),
    ("jsx", LanguageId::Javascript),
    ("mjs", LanguageId::Javascript),
    ("cjs", LanguageId::Javascript),
    ("py", LanguageId::Python),
    ("go", LanguageId::Go),
    ("java", LanguageId::Java),
    ("c", LanguageId::C),
    ("h", LanguageId::Cpp),
    ("cc", LanguageId::Cpp),
    ("cpp", LanguageId::Cpp),
    ("cxx", LanguageId::Cpp),
    ("hpp", LanguageId::Cpp),
    ("hh", LanguageId::Cpp),
    ("rs", LanguageId::Rust),
];

/// Maps a file name (or path; only the final extension matters) to its
/// language. `None` when the extension maps to no adapter — such files are
/// never discovered, and an explicit one is a usage error.
pub fn language_for(file_name: impl AsRef<Path>) -> Option<LanguageId> {
    let extension = file_name.as_ref().extension()?.to_str()?;
    EXTENSIONS
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, language)| *language)
}

/// The default include globs, derived from the extension table: one brace
/// glob over every supported extension, in table order.
///
/// The CLI's `--include` help string spells this list out by hand; a
/// guard test asserts the two stay equal.
pub fn default_include_globs() -> Vec<String> {
    vec![format!(
        "**/*.{{{}}}",
        EXTENSIONS
            .iter()
            .map(|(extension, _)| *extension)
            .collect::<Vec<_>>()
            .join(","),
    )]
}

#[cfg(all(test, feature = "cli"))]
mod cli_help_guard {
    use clap::Parser;

    use crate::cli::Cli;

    /// Renders `munge-comments plan --help` the way `run` would.
    fn plan_help() -> String {
        let err = Cli::try_parse_from(["munge-comments", "plan", "--help"])
            .expect_err("--help parses into a DisplayHelp error");
        err.render().to_string()
    }

    /// The hand-written default list in the `--include` help string equals
    /// [`default_include_globs`](super::default_include_globs): the
    /// extension table stays the single source of the default include set.
    #[test]
    fn include_help_string_matches_the_extension_table() {
        let help = plan_help();
        for glob in super::default_include_globs() {
            assert!(
                help.contains(&glob),
                "plan --help is missing the default include glob {glob}:\n{help}",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_extension_maps_to_its_language() {
        let cases = [
            ("a.ts", LanguageId::Typescript),
            ("a.tsx", LanguageId::Tsx),
            ("a.mts", LanguageId::Typescript),
            ("a.cts", LanguageId::Typescript),
            ("a.js", LanguageId::Javascript),
            ("a.jsx", LanguageId::Javascript),
            ("a.mjs", LanguageId::Javascript),
            ("a.cjs", LanguageId::Javascript),
            ("a.py", LanguageId::Python),
            ("a.go", LanguageId::Go),
            ("a.java", LanguageId::Java),
            ("a.c", LanguageId::C),
            ("a.h", LanguageId::Cpp),
            ("a.cc", LanguageId::Cpp),
            ("a.cpp", LanguageId::Cpp),
            ("a.cxx", LanguageId::Cpp),
            ("a.hpp", LanguageId::Cpp),
            ("a.hh", LanguageId::Cpp),
            ("a.rs", LanguageId::Rust),
        ];
        assert_eq!(cases.len(), 19, "the table has 19 extensions");
        for (file_name, expected) in cases {
            assert_eq!(language_for(file_name), Some(expected), "{file_name}");
        }
    }

    #[test]
    fn dotted_and_multi_suffix_names_map_by_final_extension() {
        assert_eq!(language_for("types.d.ts"), Some(LanguageId::Typescript));
        assert_eq!(language_for("src/lib/main.rs"), Some(LanguageId::Rust));
        assert_eq!(language_for("lib.min.js"), Some(LanguageId::Javascript));
        assert_eq!(language_for("spec.d.mts"), Some(LanguageId::Typescript));
    }

    #[test]
    fn h_maps_to_cpp() {
        assert_eq!(language_for("header.h"), Some(LanguageId::Cpp));
    }

    #[test]
    fn unknown_and_extensionless_names_map_to_none() {
        assert_eq!(language_for("main.kt"), None);
        assert_eq!(language_for("Makefile"), None);
        assert_eq!(language_for(".gitignore"), None);
        assert_eq!(language_for("archive.tar.gz"), None);
    }

    #[test]
    fn default_include_globs_covers_exactly_the_table_extensions() {
        let globs = default_include_globs();
        assert_eq!(globs.len(), 1, "one brace glob over the table");
        let extensions: Vec<_> = EXTENSIONS.iter().map(|(ext, _)| *ext).collect();
        assert_eq!(
            globs[0],
            format!("**/*.{{{}}}", extensions.join(",")),
            "the glob is derived from the table, in table order",
        );
        // every table extension appears; no duplicates
        for ext in &extensions {
            assert!(globs[0].contains(ext), "missing {ext}");
        }
        let unique: std::collections::HashSet<_> = extensions.iter().collect();
        assert_eq!(unique.len(), extensions.len(), "no duplicate extensions");
    }
}
