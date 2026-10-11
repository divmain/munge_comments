//! Golden scanner fixtures: the comment layer's regression backbone.
//!
//! Every file under `tests/fixtures/golden/<language>/` is one lexical
//! trap. For each fixture this suite
//!
//! - snapshots the complete [`inventory_file`] output as JSON (insta), so
//!   every field — ranges, positions, anchors, tags, groups, ids, style
//!   statistics — is pinned;
//! - cross-checks the inventory against a hand-written expectation table
//!   (the ordered raw texts + classification per fixture), so the expected
//!   records can't just snapshot whatever the code emits;
//! - asserts the file parses cleanly under its language's grammar (golden
//!   files are inputs every later stage — redaction, removal, the verifier
//!   — must be able to process) and contains no `⟦C<n>⟧`-shaped text (so
//!   golden runs never take the placeholder-collision fallback).
//!
//! Files that must trip `has_error` live in `tests/fixtures/syntax-errors/`,
//! outside `golden/`, so suites over `golden/` never see them.
//!
//! A fixture's language is its extension's (`language_for`), and must equal
//! the name of the directory it lives in — the directory layout is the
//! corpus's per-language index.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use munge_comments::comments::types::Classification;
use munge_comments::{FileRef, Inventory, LanguageId, inventory_file, language_for};
use regex::Regex;

/// Reads a fixture byte-exactly as UTF-8 (fixtures are never normalized:
/// CRLF, BOM and lone-CR cases are the point).
fn read_fixture(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    String::from_utf8(bytes)
        .unwrap_or_else(|err| panic!("fixture {} is not UTF-8: {err}", path.display()))
}

/// The fixture's path relative to `tests/fixtures/golden/`, `/`-separated
/// (the expectation table's key).
fn golden_key(path: &Path) -> String {
    let root = common::fixture_path("golden");
    path.strip_prefix(&root)
        .unwrap_or_else(|_| panic!("{} is not under {}", path.display(), root.display()))
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The fixture's language: its extension's, which must match its
/// directory.
fn golden_language(path: &Path) -> LanguageId {
    let language = language_for(path)
        .unwrap_or_else(|| panic!("{} has no source-file extension", path.display()));
    let directory = path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert_eq!(
        language.as_str(),
        directory,
        "{}: the extension maps to `{}`, but the fixture lives in `golden/{directory}/`",
        path.display(),
        language.as_str(),
    );
    language
}

/// Every golden fixture file, sorted.
fn golden_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let root = common::fixture_path("golden");
    for directory in fs::read_dir(&root).expect("tests/fixtures/golden is readable") {
        let directory = directory.expect("golden/ is listable").path();
        assert!(
            directory.is_dir(),
            "{}: golden/ holds only per-language directories",
            directory.display()
        );
        for file in fs::read_dir(&directory).expect("a golden language directory is readable") {
            let file = file
                .expect("a golden language directory is listable")
                .path();
            assert!(
                file.is_file(),
                "{}: golden language directories hold only fixture files",
                file.display()
            );
            files.push(file);
        }
    }
    files.sort();
    files
}

fn inventory(path: &Path, text: &str, language: LanguageId) -> Inventory {
    let file_name = path
        .file_name()
        .expect("a fixture has a file name")
        .to_string_lossy()
        .into_owned();
    inventory_file(
        text,
        FileRef {
            language,
            file_name,
        },
    )
    .unwrap_or_else(|err| panic!("inventorying {}: {err}", path.display()))
}

/// The inventory as the expectation table spells it: ordered
/// `(raw text, classification)` pairs.
fn observed(inventory: &Inventory) -> Vec<(String, Classification)> {
    inventory
        .comments
        .iter()
        .map(|comment| (comment.text.clone(), comment.classification))
        .collect()
}

/// Renders an observed list as a table entry, so a mismatch report shows
/// the actual records in the table's own syntax.
fn render_entry(key: &str, records: &[(String, Classification)]) -> String {
    let mut out = format!("    (\n        {key:?},\n        &[\n");
    for (text, classification) in records {
        let class = match classification {
            Classification::Prose => "P",
            Classification::Functional => "F",
            Classification::DocProtected => "D",
        };
        out.push_str(&format!("            ({text:?}, {class}),\n"));
    }
    out.push_str("        ],\n    ),\n");
    out
}

#[test]
fn golden_inventories_match_their_snapshots() {
    insta::glob!("fixtures/golden/**/*", |path| {
        if path.is_dir() {
            return;
        }
        let language = golden_language(path);
        let text = read_fixture(path);
        insta::assert_json_snapshot!(inventory(path, &text, language));
    });
}

/// Offset-contract violations: every record's range must slice the
/// source to exactly its text, with 1-based `line`/`column` (Unicode
/// scalars, line 1 starting after a leading BOM) agreeing with `start`, and
/// every member range must lie inside the record, in order, without
/// overlap. Independent of the scanner's own arithmetic, so the table's
/// texts pin the offsets too.
fn range_violations(key: &str, text: &str, inventory: &Inventory) -> Vec<String> {
    let mut out = Vec::new();
    let mut previous_end = 0;
    for comment in &inventory.comments {
        let id = &comment.id;
        if text.get(comment.start..comment.end) != Some(comment.text.as_str()) {
            out.push(format!(
                "{key} {id}: source[start..end] is not the record text"
            ));
            continue;
        }
        if comment.start < previous_end {
            out.push(format!(
                "{key} {id}: overlaps or precedes the previous record"
            ));
        }
        previous_end = comment.end;
        let before = &text[..comment.start];
        let line = 1 + before.matches('\n').count();
        let line_text = match before.rfind('\n') {
            Some(at) => &before[at + 1..],
            None => before.strip_prefix('\u{FEFF}').unwrap_or(before),
        };
        let column = 1 + line_text.chars().count();
        if (comment.line as usize, comment.column as usize) != (line, column) {
            out.push(format!(
                "{key} {id}: line/column {}:{} but the source says {line}:{column}",
                comment.line, comment.column
            ));
        }
        let mut cursor = comment.start;
        for member in &comment.members {
            if member.start < cursor || member.end > comment.end || member.start >= member.end {
                out.push(format!(
                    "{key} {id}: member {member:?} out of order or bounds"
                ));
            }
            cursor = member.end;
        }
        let first = comment.members.first().map(|member| member.start);
        let last = comment.members.last().map(|member| member.end);
        if (first, last) != (Some(comment.start), Some(comment.end)) {
            out.push(format!("{key} {id}: members do not span the record"));
        }
    }
    out
}

#[test]
fn golden_inventories_match_the_expectation_table() {
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    for path in golden_files() {
        let key = golden_key(&path);
        let language = golden_language(&path);
        let text = read_fixture(&path);
        let inventory = inventory(&path, &text, language);
        failures.extend(range_violations(&key, &text, &inventory));
        let actual = observed(&inventory);
        seen.insert(key.clone());
        let Some((_, expected)) = EXPECTATIONS.iter().find(|(name, _)| *name == key) else {
            failures.push(format!(
                "{key}: no expectation-table entry; observed:\n{}",
                render_entry(&key, &actual)
            ));
            continue;
        };
        let expected: Vec<(String, Classification)> = expected
            .iter()
            .map(|(text, classification)| ((*text).to_string(), *classification))
            .collect();
        if actual != expected {
            failures.push(format!(
                "{key}: inventory differs from the expectation table; observed:\n{}",
                render_entry(&key, &actual)
            ));
        }
    }
    for (key, _) in EXPECTATIONS {
        if !seen.contains(*key) {
            failures.push(format!("{key}: table entry without a fixture file"));
        }
    }
    let mut keys: Vec<&str> = EXPECTATIONS.iter().map(|(key, _)| *key).collect();
    let total = keys.len();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() != total {
        failures.push("the expectation table has duplicate keys".to_string());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn golden_fixtures_parse_cleanly() {
    let mut dirty = Vec::new();
    for path in golden_files() {
        let language = golden_language(&path);
        let tree = munge_comments::lang::parse(&read_fixture(&path), language)
            .unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()));
        if tree.has_error || tree.too_deep {
            dirty.push(golden_key(&path));
        }
    }
    assert!(
        dirty.is_empty(),
        "golden fixtures must parse cleanly: {dirty:?}"
    );
}

#[test]
fn golden_fixtures_contain_no_placeholder_shaped_text() {
    let placeholder = Regex::new("⟦C[0-9]+⟧").expect("the placeholder pattern compiles");
    let offenders: Vec<String> = golden_files()
        .iter()
        .filter(|path| placeholder.is_match(&read_fixture(path)))
        .map(|path| golden_key(path))
        .collect();
    assert!(
        offenders.is_empty(),
        "golden fixtures must not contain placeholder-shaped text: {offenders:?}"
    );
}

#[test]
fn every_tier1_language_has_golden_fixtures() {
    let covered: BTreeSet<LanguageId> = golden_files()
        .iter()
        .map(|path| golden_language(path))
        .collect();
    for language in LanguageId::ALL {
        assert!(
            covered.contains(&language),
            "no golden fixtures for {}",
            language.as_str()
        );
    }
}

#[test]
fn syntax_error_fixtures_trip_has_error() {
    let root = common::fixture_path("syntax-errors");
    let mut files: Vec<PathBuf> = fs::read_dir(&root)
        .expect("tests/fixtures/syntax-errors is readable")
        .map(|entry| entry.expect("syntax-errors/ is listable").path())
        .collect();
    files.sort();
    let names: Vec<String> = files
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for required in [
        "asi-multiline-comment.js",
        "go-multiline-comment-newline.go",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "syntax-errors/{required} is missing"
        );
    }
    for path in files {
        let language = language_for(&path)
            .unwrap_or_else(|| panic!("{} has no source-file extension", path.display()));
        let tree = munge_comments::lang::parse(&read_fixture(&path), language)
            .unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()));
        assert!(
            tree.has_error,
            "{} must trip has_error under {}",
            path.display(),
            language.as_str()
        );
    }
}

// The table below is long enough that spelling out `Classification::Prose` on every record would drown the texts it exists to pin, so the variants get single-letter aliases here; they also match the letters `render_entry` prints, which keeps a failure report readable against the table it quotes. The table itself is written by hand: per fixture (keyed by its path under `golden/`), the ordered raw texts of its records and their classifications. A logical group of line comments is one record whose text spans its members.

use Classification::{DocProtected as D, Functional as F, Prose as P};

/// `(fixture key, ordered (raw text, classification) records)`.
type Entry = (&'static str, &'static [(&'static str, Classification)]);

const EXPECTATIONS: &[Entry] = &[
    (
        "typescript/strings-and-urls.ts",
        &[
            ("// Endpoints the client talks to.", P),
            ("// trailing after a URL string", P),
            ("/* A block comment after strings. */", P),
            ("// concatenated slashes", P),
        ],
    ),
    (
        "typescript/template-literals.ts",
        &[
            ("// A template with a comment-looking literal part.", P),
            ("/* a real comment */", P),
            ("/* a comment inside a substitution */", P),
        ],
    ),
    (
        "typescript/regex-literals.ts",
        &[
            ("// Patterns used below.", P),
            ("// a slash inside a character class", P),
            ("/* trailing block */", P),
        ],
    ),
    (
        "tsx/jsx-text.tsx",
        &[("// JSX text that looks like comments is not a comment.", P)],
    ),
    (
        "tsx/jsx-expression.tsx",
        &[
            ("/* The sole content of a container. */", P),
            ("// A line comment alone in a container.", P),
            ("/* beside an expression */", P),
        ],
    ),
    (
        "tsx/generic-arrow.tsx",
        &[
            ("// A generic arrow needs the trailing comma in .tsx.", P),
            ("// two type parameters", P),
            ("/* Uses both helpers. */", P),
        ],
    ),
    (
        "javascript/shebang.js",
        &[
            ("// Copyright 2026 Example Corp.", F),
            ("// Licensed under the MIT license; see LICENSE.", F),
            ("// keyword-free continuation of the header", F),
            ("// Entry point: print a greeting.", P),
        ],
    ),
    (
        "typescript/legal-header.ts",
        &[
            (
                "/*!\n * widget-kit v1.2.3\n * Copyright (c) 2026 Example Corp.\n */",
                F,
            ),
            ("// Copyright 2026 Example Corp.", F),
            ("// SPDX-License-Identifier: Apache-2.0", F),
            ("// No keyword on this continuation line.", F),
            (
                "// Mentions copyright, but code precedes it: ordinary prose.",
                P,
            ),
            (
                "/*! Preserved legal comment in the middle of the file. */",
                F,
            ),
        ],
    ),
    (
        "typescript/directives.ts",
        &[
            ("/// <reference types=\"node\" />", F),
            ("/// <amd-module name=\"directives\" />", F),
            ("/** @jest-environment node */", F),
            ("/** @jsx h */", F),
            ("/* eslint-disable no-console */", F),
            ("/* global window, document */", F),
            ("/* eslint eqeqeq: \"off\" */", F),
            ("// @ts-nocheck", F),
            ("// @flow", F),
            ("// @ts-ignore", F),
            ("// @ts-expect-error", F),
            ("// eslint-disable-next-line no-unused-vars", F),
            ("// tslint:disable-next-line:no-any", F),
            ("// biome-ignore lint/style/useConst: reassigned below", F),
            ("// deno-lint-ignore no-explicit-any", F),
            ("// prettier-ignore", F),
            ("// $FlowFixMe", F),
            ("/* istanbul ignore next */", F),
            ("/* webpackChunkName: \"chunk\" */", F),
            ("/*#__PURE__*/", F),
            ("/** @license MIT */", F),
            ("/** @preserve */", F),
            ("// #region Helpers", F),
            ("/** @internal */", F),
            ("// #endregion", F),
            ("// NOSONAR", F),
            ("/* eslint-enable */", F),
            ("// vim: set ts=2 sw=2:", F),
        ],
    ),
    (
        "typescript/directive-adjacent.ts",
        &[
            ("// @ts-expect-error", F),
            ("/** The legacy alias is still accepted here. */", F),
            ("// eslint-disable-next-line no-var", F),
            (
                "// This explanation sits between the directive and its target.",
                F,
            ),
            ("/* c8 ignore next 2 */", F),
            ("// Line 2 of the c8 window: protected.", F),
            ("// Line 4, past the window: ordinary prose.", P),
        ],
    ),
    (
        "typescript/empty-comments.ts",
        &[
            ("//", P),
            ("/**/", P),
            ("/* */", P),
            ("/***/", P),
            ("//", P),
            ("//", P),
        ],
    ),
    (
        "typescript/nested-blocks.ts",
        &[
            ("/* outer /* inner */", P),
            ("/* first */", P),
            ("/* second */", P),
            (
                "/*\n * A multi-line block with a stray /* opener inside.\n */",
                P,
            ),
            ("/* /* */", P),
        ],
    ),
    (
        "typescript/line-groups.ts",
        &[
            (
                "// A group of three\n// consecutive line comments\n// at the same indent.",
                P,
            ),
            ("// An indented group,\n  // two lines long.", P),
            ("// Indent changes", P),
            ("// split the group here.", P),
            ("// Before the directive.", P),
            ("// eslint-disable-next-line no-console", F),
            ("// After the directive (adjacent, protected).", F),
            ("// A blank line ends a group.", P),
            ("// So this starts another.", P),
            ("// a trailing comment", P),
            ("// never joins a group.", P),
        ],
    ),
    (
        "typescript/crlf.ts",
        &[
            (
                "// Line endings are CRLF throughout.\r\n// A CRLF group spans\r\n// two lines.",
                P,
            ),
            ("// trailing before CRLF", P),
            ("/*\r\n * A block spanning CRLF lines.\r\n */", P),
            ("/** Doc at EOF. */", P),
        ],
    ),
    (
        "typescript/bom.ts",
        &[
            ("// The file starts with a byte-order mark.", P),
            ("/* after the BOM line */", P),
        ],
    ),
    (
        "typescript/dangling-eof.ts",
        &[
            ("// Dangling at end of file.", P),
            ("/* And a block after it. */", P),
        ],
    ),
    (
        "typescript/doc-eof.ts",
        &[
            ("/** trailing doc */", P),
            ("// and a line comment", P),
            ("/** A doc comment with nothing after it. */", P),
        ],
    ),
    (
        "typescript/dangling-in-block.ts",
        &[
            ("// Nothing here yet.", P),
            ("// After the final statement, before the closing brace.", P),
            ("// Before the closing bracket.", P),
            ("/* before the closing paren */", P),
        ],
    ),
    ("typescript/asi-hazard.ts", &[("// c", P), ("/*\n  */", P)]),
    (
        "typescript/inline-block.ts",
        &[("/* c */", P), ("/* c */", P), ("/**/", P)],
    ),
    (
        "typescript/unicode.ts",
        &[
            (
                "// Größe: identifiers and strings with multi-byte characters.",
                P,
            ),
            ("// after CJK text", P),
            ("/* after emoji */", P),
            ("// 🎉 an emoji inside the comment", P),
            ("/* Ελληνικά in a block comment */", P),
        ],
    ),
    (
        "typescript/module-format.mts",
        &[
            ("// An ES module in TypeScript's .mts extension.", P),
            ("/** Reads the configuration file. */", P),
            ("// resolved relative to cwd", P),
        ],
    ),
    ("javascript/html-comment.js", &[]),
    (
        "javascript/magic-comments.js",
        &[
            ("// Lazy-loaded routes.", P),
            ("/* webpackChunkName: \"home\" */", F),
            (
                "/* webpackChunkName: \"about\", webpackPrefetch: true */",
                F,
            ),
            ("/* webpackIgnore: true */", F),
            ("/* @vite-ignore */", F),
            ("/*#__PURE__*/", F),
            ("/* @__PURE__ */", F),
            ("/*#__NO_SIDE_EFFECTS__*/", F),
        ],
    ),
    (
        "javascript/source-map.js",
        &[
            (
                "// A bundled file with a trailing source-map comment.\n//# sourceMappingURL=stale.js.map",
                P,
            ),
            ("//# sourceMappingURL=data:application/json;base64,e30=", F),
        ],
    ),
    (
        "javascript/jsdoc-policy.js",
        &[
            ("/** Internal helper: prose. */", P),
            ("/** Exported: doc-protected. */", D),
            (
                "/**\n * Load-bearing under checkJs: functional.\n * @param {number} x\n * @returns {number}\n */",
                F,
            ),
            ("/** @typedef {{ id: number }} Item */", F),
            ("/** @deprecated Use exported() instead. */", F),
            ("/* A plain block on an export is not JSDoc: prose. */", P),
        ],
    ),
    (
        "javascript/cjs-exports.cjs",
        &[
            (
                "/** Exported via module.exports.alpha: doc-protected. */",
                D,
            ),
            ("/** Exported via exports.beta: doc-protected. */", D),
            (
                "/** Exported as module.exports itself: doc-protected. */",
                D,
            ),
            (
                "/** Exported via shorthand in an object literal: doc-protected. */",
                D,
            ),
            ("/** Exported via a key-value pair: doc-protected. */", D),
            ("/** Not exported: prose. */", P),
            (
                "/** JSDoc on the export assignment itself: doc-protected. */",
                D,
            ),
            (
                "/** JSDoc on an exports.name assignment: doc-protected. */",
                D,
            ),
        ],
    ),
    (
        "javascript/esm.mjs",
        &[
            ("/** ESM export: doc-protected. */", D),
            ("/** CommonJS shapes do not export in .mjs: prose. */", P),
            ("/** Not exported: prose. */", P),
        ],
    ),
    (
        "python/encoding-cookie-line1.py",
        &[
            ("# -*- coding: utf-8 -*-", F),
            ("# An ordinary comment.", P),
        ],
    ),
    (
        "python/encoding-cookie-line2.py",
        &[
            ("# coding=utf-8", F),
            (
                "# Line 3 says coding: latin-1, but only lines 1-2 count.",
                P,
            ),
        ],
    ),
    (
        "python/encoding-cookie-after-comment.py",
        &[
            ("# A first-line comment that is not a cookie.", P),
            ("# vim: set fileencoding=utf-8 :", F),
        ],
    ),
    (
        "python/encoding-cookie-after-code.py",
        &[(
            "# coding: utf-8\n# A cookie on line 2 after code on line 1 does not count.",
            P,
        )],
    ),
    (
        "python/encoding-cookie-line3.py",
        &[("# coding: utf-8\n# Line 3 is too late for a cookie.", P)],
    ),
    (
        "python/type-comments.py",
        &[
            ("# Type comments are read by type checkers.", P),
            ("# type: List[int]", F),
            ("# type: (int, int) -> int", F),
            ("# type: ignore", F),
        ],
    ),
    (
        "python/tag-flags.py",
        &[
            ("# TODO: x", P),
            ("# FIXME and HACK: retries are not capped yet.", P),
            ("# Notes on NOTEWORTHY things are not flagged.", P),
            ("# todo in lowercase is not a flag either.", P),
            ("# XXX", P),
        ],
    ),
    (
        "python/noqa.py",
        &[
            ("# noqa: F401", F),
            ("# noqa", F),
            (
                "# A long line follows; the trailing noqa silences the linter.",
                P,
            ),
            ("# noqa: E501", F),
            ("# nosec B105", F),
        ],
    ),
    (
        "python/docstrings.py",
        &[("# The only real comment in this file.", P)],
    ),
    (
        "python/indented-comments.py",
        &[
            ("# Indented comment-only line, first in the block.", P),
            (
                "# Deeper indentation.\n        # A two-line group at the same indent.",
                P,
            ),
            ("# Over-indented comment lines are still comments.", P),
            ("# Dedented back to the function body.", P),
        ],
    ),
    (
        "python/copyright-header.py",
        &[
            ("# Copyright 2026 Example Corp.", F),
            ("# All rights reserved.", F),
            ("#", F),
            (
                "# Use of this source code is governed by a BSD-style license.",
                F,
            ),
            (
                "# Not part of the header: a docstring (code) precedes it.",
                P,
            ),
        ],
    ),
    (
        "python/crlf.py",
        &[
            (
                "# A CRLF Python file.\r\n# Its comments end before the CR.",
                P,
            ),
            ("# trailing", P),
            ("# Indented, CRLF.", P),
        ],
    ),
    (
        "python/shebang.py",
        &[("# The shebang is not a record; this comment is.", P)],
    ),
    ("python/lone-cr.py", &[("# a\rx = 1", F)]),
    (
        "go/build-tags.go",
        &[
            ("//go:build linux && amd64", F),
            ("// +build linux,amd64", F),
            ("// Package tags is only built on linux/amd64.", P),
            ("// Answer is the answer.", P),
        ],
    ),
    (
        "go/cgo-preamble.go",
        &[
            ("// #include <stdlib.h>", F),
            ("// #include <stdio.h>", F),
            ("//", F),
            ("// static int add(int a, int b) { return a + b; }", F),
            ("// Add calls into C.", P),
        ],
    ),
    (
        "go/cgo-grouped-import.go",
        &[
            ("/*\n\t#include <math.h>\n\t*/", F),
            ("// A comment above a non-C spec is ordinary prose.", P),
            ("// Sqrt prints a square root computed in C.", P),
        ],
    ),
    (
        "go/generate.go",
        &[
            ("//go:generate stringer -type=Color", F),
            ("// Color is a palette entry.", P),
            ("//go:noinline", F),
        ],
    ),
    (
        "go/nolint.go",
        &[
            (
                "//nolint:errcheck // the explanation after the directive is part of it",
                F,
            ),
            ("//nolint:gosec", F),
            (
                "// This prose sits between the directive and its target.",
                F,
            ),
            ("//nolint:errcheck", F),
        ],
    ),
    (
        "go/example_test.go",
        &[
            ("// ExampleHello shows the greeting.", P),
            ("// Print the greeting.", P),
            ("// Output: hello", F),
            ("// ExampleUnordered shows unordered output.", P),
            ("// Unordered output:", F),
            ("// a", F),
            ("// b", F),
            (
                "// Examplefoo is not an example (lowercase after the prefix).",
                P,
            ),
            ("// Output: not checked", P),
        ],
    ),
    (
        "java/doc-comments.java",
        &[
            (
                "/**\n * A Javadoc comment on a class: doc-protected.\n */",
                D,
            ),
            ("/// A Markdown doc comment (JEP 467): doc-protected.", D),
            ("/// Its lines form one comment each.", D),
            (
                "//// javac reads four slashes as a Markdown doc line too.",
                D,
            ),
            ("/* A plain block comment: prose. */", P),
            ("// A plain line comment: prose.", P),
        ],
    ),
    (
        "java/deprecated.java",
        &[
            (
                "/**\n     * Use {@link #replacement()} instead.\n     *\n     * @deprecated javac reads this tag.\n     */",
                F,
            ),
            ("/** Not deprecated: doc-protected. */", D),
        ],
    ),
    (
        "java/nopmd.java",
        &[
            ("//NOPMD - kept for serialization", F),
            ("// NOPMD", F),
            ("// CHECKSTYLE:OFF", F),
            ("// CHECKSTYLE:ON", F),
            ("//noinspection unused", F),
        ],
    ),
    (
        "java/formatter.java",
        &[
            ("// @formatter:off", F),
            ("// @formatter:on", F),
            ("// A prose comment after the region.", P),
        ],
    ),
    (
        "java/large-class.java",
        &[
            (
                "/**\n * A large class body: many members, each with its own comments.\n */",
                D,
            ),
            ("// The running total, updated by every operation.", P),
            ("/** Adds the two operands. */", D),
            ("// Record the result of add.", P),
            ("/** Subtracts the second operand from the first. */", D),
            ("// Record the result of subtract.", P),
            ("/** Multiplies the operands. */", D),
            ("// Record the result of multiply.", P),
            ("/** Divides, returning zero for a zero divisor. */", D),
            ("// Record the result of divide.", P),
            ("/** Takes the remainder, zero for a zero divisor. */", D),
            ("// Record the result of remainder.", P),
            ("/** Returns the larger operand. */", D),
            ("// Record the result of max.", P),
            ("/** Returns the smaller operand. */", D),
            ("// Record the result of min.", P),
            ("/** Averages the operands, truncating. */", D),
            ("// Record the result of average.", P),
            ("/** Raises the first operand to the second. */", D),
            ("// Record the result of power.", P),
            ("/** Returns the absolute difference. */", D),
            ("// Record the result of distance.", P),
            ("/** Clamps the first operand into [0, b]. */", D),
            ("// Record the result of clamp.", P),
            ("/** Compares the operands by sign. */", D),
            ("// Record the result of sign.", P),
            ("// Reset support for tests.", P),
            ("// back to the initial state", P),
        ],
    ),
    (
        "c/fallthrough.c",
        &[
            ("/* Classifies a character class code. */", P),
            ("/* FALLTHROUGH */", F),
            ("// fall through", F),
            ("/* Deliberately continues into the next case. */", P),
            ("// FALLTHRU", F),
            ("// An ordinary prose comment.", P),
        ],
    ),
    (
        "c/nested-block.c",
        &[
            (
                "/* C block comments do not nest: /* this opener is just text */",
                P,
            ),
            ("/* /* */", P),
            ("/*\n * A multi-line block with a stray /* inside.\n */", P),
        ],
    ),
    (
        "c/define-comments.c",
        &[
            ("/* a trailing block comment: inventoried */", P),
            ("/* Uses both macros. */", P),
        ],
    ),
    (
        "c/line-splice.c",
        &[
            (
                "// A line comment ending in a backslash and a space splices for GCC: \\ ",
                F,
            ),
            ("// so this line is inside it for the compiler.", P),
            ("// A plain comment.", P),
        ],
    ),
    (
        "cpp/nolint.cpp",
        &[
            ("// NOLINTNEXTLINE(cert-err34-c)", F),
            (
                "// Prose between the directive and its target is protected.",
                F,
            ),
            ("// NOLINT(cert-err34-c)", F),
        ],
    ),
    (
        "cpp/doxygen.cpp",
        &[
            (
                "/// Doxygen line documentation: ordinary prose in C++.\n/// A second line of it.",
                P,
            ),
            ("/** Doxygen block documentation: also prose. */", P),
            ("//! Qt-style Doxygen line: prose too.", P),
        ],
    ),
    (
        "cpp/template.cpp",
        &[
            ("// Sums any container of numbers.", P),
            ("// zero-initialized", P),
            ("/* Explicit use, so the template is instantiated. */", P),
        ],
    ),
    (
        "cpp/raw-string-header.h",
        &[
            ("// A real comment before the raw string.", P),
            ("// a real trailing comment", P),
            ("/* A real block comment after it. */", P),
        ],
    ),
    (
        "rust/doc-comments.rs",
        &[
            ("//! Crate-level inner doc comment: doc-protected.", D),
            ("//! A second inner doc line.", D),
            ("/// Outer doc comment on a function: doc-protected.", D),
            ("//// Four slashes: not a doc comment in Rust.", P),
            ("/** Outer block doc comment: doc-protected. */", D),
            ("/*! Inner block doc comment: doc-protected. */", D),
            ("// An ordinary comment: prose.", P),
        ],
    ),
    (
        "rust/safety.rs",
        &[
            ("/// Reads the first byte.", D),
            ("// SAFETY: the slice is non-empty, checked just above.", F),
            ("// A plain comment mentioning safety is prose.", P),
        ],
    ),
    (
        "rust/nested-block.rs",
        &[
            (
                "/* Rust block comments nest: /* inner */ still inside the outer one. */",
                P,
            ),
            ("/* one /* two /* three */ two */ one */", P),
        ],
    ),
    (
        "rust/derive.rs",
        &[
            ("// A comment above the attribute anchors to the struct.", P),
            ("// The horizontal coordinate.", P),
            ("// the vertical coordinate", P),
        ],
    ),
    (
        "rust/shebang-line2.rs",
        &[("// A comment on line 2, right after the shebang.", P)],
    ),
    (
        "rust/shebang-license.rs",
        &[
            ("// Copyright 2026 Example Corp.", F),
            ("// SPDX-License-Identifier: MIT OR Apache-2.0", F),
            ("// Entry point.", F),
        ],
    ),
    (
        "rust/raw-string.rs",
        &[
            ("// A raw string containing comment-like text.", P),
            ("// a real trailing comment", P),
            ("/* a real block comment */", P),
        ],
    ),
    (
        "python/encoding-cookie-after-blank.py",
        &[
            ("# coding: utf-8", F),
            ("# Line 1 is blank, so the line-2 cookie counts.", P),
        ],
    ),
    // Content-empty comments (decision-matrix row 3) in every tier-1 language beside TypeScript, so offline mode's only edit — removing them — is planned, applied and verified in each grammar. Empty and separator-only comments are prose in every language — there is nothing for a classifier to latch onto — yet each language still gets its own fixture so the per-language scanner paths are exercised; the `------` case pins that a body of pure punctuation is not mistaken for a directive.
    (
        "javascript/empty-comments.js",
        &[("//", P), ("/* */", P), ("//", P), ("// ------", P)],
    ),
    (
        "tsx/empty-comments.tsx",
        &[("//", P), ("/* */", P), ("//", P)],
    ),
    (
        "python/empty-comments.py",
        &[("#", P), ("# ------", P), ("#", P), ("#", P)],
    ),
    (
        "go/empty-comments.go",
        &[("//", P), ("/* */", P), ("//", P), ("// ------", P)],
    ),
    (
        "java/empty-comments.java",
        &[("//", P), ("/* */", P), ("//", P), ("// ------", P)],
    ),
    (
        "cpp/empty-comments.cpp",
        &[("//", P), ("/* */", P), ("//", P), ("//------", P)],
    ),
    (
        "rust/empty-comments.rs",
        &[("//", P), ("/* */", P), ("//", P), ("// ------", P)],
    ),
];
