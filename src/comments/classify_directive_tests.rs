//! Table-driven tests for every directive row of every tier-1 adapter and
//! for the cross-language [`GLOBAL_DIRECTIVES`]: one case per row per
//! language, asserting classification, tag and effective scope, plus the
//! byte-hazard rows, line windows, trailing scopes and the negatives.
//!
//! A coverage check proves the tables and the cases agree: every adapter
//! row must be matched — producing the tag the case asserts — by some case
//! in every language whose adapter carries it, and every global row by
//! cases in at least two languages.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::comments::sanitize::comment_body_with_lines;
use crate::comments::scan::scan_file;
use crate::comments::types::FileRef;
use crate::lang::adapter::Pattern;
use crate::lang::adapters::{ALL_ADAPTERS, GLOBAL_DIRECTIVES};

use Classification::{Functional, Prose};
use LanguageId::{C, Cpp, Go, Java, Javascript, Python, Rust, Tsx, Typescript};
use Scope::{File, Inline, Next};

const JS_FAMILY: &[LanguageId] = &[Javascript, Typescript, Tsx];
const TS_ONLY: &[LanguageId] = &[Typescript, Tsx];
const C_FAMILY: &[LanguageId] = &[C, Cpp];

/// A file name for a language's snippets.
fn file_name(language: LanguageId) -> &'static str {
    match language {
        Javascript => "a.js",
        Typescript => "a.ts",
        Tsx => "a.tsx",
        Python => "a.py",
        Go => "a.go",
        Java => "A.java",
        C => "a.c",
        Cpp => "a.cpp",
        Rust => "a.rs",
    }
}

/// A minimal valid file holding `comment` on its own line, followed by code.
fn own_line(language: LanguageId, comment: &str) -> String {
    match language {
        Javascript | Typescript | Tsx => format!("{comment}\nfoo();\n"),
        Python => format!("{comment}\nx = 1\n"),
        Go => format!("package p\n\n{comment}\nvar x = 1\n"),
        Java => format!("class A {{\n  {comment}\n  int x;\n}}\n"),
        C | Cpp => format!("{comment}\nint x;\n"),
        Rust => format!("{comment}\nfn f() {{}}\n"),
    }
}

/// A minimal valid file holding `comment` after code on the same line.
fn trailing(language: LanguageId, comment: &str) -> String {
    match language {
        Javascript | Typescript | Tsx => format!("foo(); {comment}\nbar();\n"),
        Python => format!("x = 1  {comment}\ny = 2\n"),
        Go => format!("package p\n\nvar x = 1 {comment}\nvar y = 2\n"),
        Java => format!("class A {{\n  int x; {comment}\n  int y;\n}}\n"),
        C | Cpp => format!("int x; {comment}\nint y;\n"),
        Rust => format!("fn f() {{}} {comment}\nfn g() {{}}\n"),
    }
}

fn classify_source(language: LanguageId, text: &str) -> Vec<ClassifiedComment> {
    let file_name = file_name(language);
    let scan = scan_file(
        text,
        FileRef {
            language,
            file_name: file_name.to_string(),
        },
    )
    .unwrap_or_else(|error| panic!("{language:?} snippet scans: {error:?}\n{text}"));
    classify_all(
        scan.comments,
        ClassifyCtx {
            text,
            file_name,
            language,
            has_shebang: scan.has_shebang,
        },
    )
    .unwrap_or_else(|error| panic!("{language:?} snippet classifies: {error:?}\n{text}"))
}

/// The record whose raw text is exactly `comment`.
fn record(language: LanguageId, text: &str, comment: &str) -> ClassifiedComment {
    classify_source(language, text)
        .into_iter()
        .find(|record| record.text == comment)
        .unwrap_or_else(|| panic!("{language:?}: no comment {comment:?} in\n{text}"))
}

/// One positive case: `comment`, on its own line in each of `languages`,
/// is functional, carries `tag`, and has effective scope `scope`.
struct Case {
    languages: &'static [LanguageId],
    comment: &'static str,
    tag: &'static str,
    scope: Scope,
}

const fn case(
    languages: &'static [LanguageId],
    comment: &'static str,
    tag: &'static str,
    scope: Scope,
) -> Case {
    Case {
        languages,
        comment,
        tag,
        scope,
    }
}

// ---- JavaScript / TypeScript / TSX. ---------------------------------------

const JS_CASES: &[Case] = &[
    // TypeScript directives.
    case(JS_FAMILY, "// @ts-ignore", "directive:ts-ignore", Next),
    case(
        JS_FAMILY,
        "// @ts-expect-error",
        "directive:ts-expect-error",
        Next,
    ),
    case(JS_FAMILY, "// @ts-nocheck", "directive:ts-nocheck", File),
    case(JS_FAMILY, "// @ts-check", "directive:ts-check", File),
    case(
        JS_FAMILY,
        "/// <reference path=\"x.d.ts\" />",
        "directive:reference",
        File,
    ),
    case(
        JS_FAMILY,
        "///<amd-module name=\"m\" />",
        "directive:amd-module",
        File,
    ),
    // ESLint.
    case(
        JS_FAMILY,
        "/* eslint-disable */",
        "directive:eslint-disable",
        File,
    ),
    case(
        JS_FAMILY,
        "// eslint-disable-next-line no-console",
        "directive:eslint-disable-next-line",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* eslint-enable */",
        "directive:eslint-enable",
        File,
    ),
    case(
        JS_FAMILY,
        "/* eslint-env node */",
        "directive:eslint-env",
        File,
    ),
    case(JS_FAMILY, "/* global foo, bar */", "directive:global", File),
    case(JS_FAMILY, "/*globals foo*/", "directive:global", File),
    case(JS_FAMILY, "/* exported main */", "directive:exported", File),
    case(
        JS_FAMILY,
        "/* eslint quotes: \"error\" */",
        "directive:eslint-config",
        File,
    ),
    // TSLint, oxlint, Deno, Biome.
    case(
        JS_FAMILY,
        "// tslint:disable",
        "directive:tslint-disable",
        File,
    ),
    case(
        JS_FAMILY,
        "// tslint:disable-next-line",
        "directive:tslint-disable-next-line",
        Next,
    ),
    case(
        JS_FAMILY,
        "// tslint:enable",
        "directive:tslint-enable",
        File,
    ),
    case(
        JS_FAMILY,
        "/* oxlint-disable */",
        "directive:oxlint-disable",
        File,
    ),
    case(
        JS_FAMILY,
        "// oxlint-disable-next-line no-debugger",
        "directive:oxlint-disable-next-line",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* oxlint-enable */",
        "directive:oxlint-enable",
        File,
    ),
    case(
        JS_FAMILY,
        "// deno-lint-ignore no-explicit-any",
        "directive:deno-lint-ignore",
        Next,
    ),
    case(
        JS_FAMILY,
        "// deno-lint-ignore",
        "directive:deno-lint-ignore",
        Next,
    ),
    // A file-level sibling keeps its file scope (the next-scoped row stops
    // at `-`).
    case(
        JS_FAMILY,
        "// deno-lint-ignore-file",
        "directive:deno-lint-ignore-file",
        File,
    ),
    case(
        JS_FAMILY,
        "// biome-ignore lint/style: reason",
        "directive:biome-ignore",
        Next,
    ),
    case(
        JS_FAMILY,
        "// biome-ignore-all lint: reason",
        "directive:biome-ignore-all",
        File,
    ),
    case(
        JS_FAMILY,
        "// biome-ignore-start lint: reason",
        "directive:biome-ignore-start",
        File,
    ),
    case(
        JS_FAMILY,
        "// biome-ignore-end",
        "directive:biome-ignore-end",
        File,
    ),
    // Formatters.
    case(
        JS_FAMILY,
        "// prettier-ignore",
        "directive:prettier-ignore",
        Next,
    ),
    case(JS_FAMILY, "/** @format */", "directive:format", File),
    case(JS_FAMILY, "/** @prettier */", "directive:prettier", File),
    case(JS_FAMILY, "/* @noformat */", "directive:noformat", File),
    case(
        JS_FAMILY,
        "// dprint-ignore",
        "directive:dprint-ignore",
        Next,
    ),
    case(
        JS_FAMILY,
        "// dprint-ignore-file",
        "directive:dprint-ignore-file",
        File,
    ),
    // Coverage.
    case(
        JS_FAMILY,
        "/* istanbul ignore next */",
        "directive:istanbul-ignore-next",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* istanbul ignore if */",
        "directive:istanbul-ignore-if",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* istanbul ignore else */",
        "directive:istanbul-ignore-else",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* istanbul ignore file */",
        "directive:istanbul-ignore-file",
        File,
    ),
    case(
        JS_FAMILY,
        "/* c8 ignore start */",
        "directive:c8-ignore",
        File,
    ),
    case(
        JS_FAMILY,
        "/* c8 ignore next */",
        "directive:c8-ignore-next",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* v8 ignore stop */",
        "directive:v8-ignore",
        File,
    ),
    case(
        JS_FAMILY,
        "/* v8 ignore next 2 */",
        "directive:v8-ignore-next",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* node:coverage ignore next */",
        "directive:node-coverage-ignore-next",
        Next,
    ),
    case(
        JS_FAMILY,
        "/* node:coverage disable */",
        "directive:node-coverage-disable",
        File,
    ),
    case(
        JS_FAMILY,
        "/* node:coverage enable */",
        "directive:node-coverage-enable",
        File,
    ),
    // Bundlers and minifiers.
    case(
        JS_FAMILY,
        "/* webpackChunkName: \"x\" */",
        "directive:webpack-chunk-name",
        Inline,
    ),
    case(
        JS_FAMILY,
        "/* webpackPrefetch: true */",
        "directive:webpack-prefetch",
        Inline,
    ),
    case(
        JS_FAMILY,
        "/* webpackFetchPriority: \"high\" */",
        "directive:webpack-fetch-priority",
        Inline,
    ),
    case(
        JS_FAMILY,
        "/* @vite-ignore */",
        "directive:vite-ignore",
        Inline,
    ),
    case(JS_FAMILY, "/*@__PURE__*/", "pure-annotation", Next),
    case(JS_FAMILY, "/*#__PURE__*/", "pure-annotation", Next),
    case(
        JS_FAMILY,
        "/*@__NO_SIDE_EFFECTS__*/",
        "directive:no-side-effects",
        Next,
    ),
    case(
        JS_FAMILY,
        "/*#__NO_SIDE_EFFECTS__*/",
        "directive:no-side-effects",
        Next,
    ),
    // Legal comments.
    case(JS_FAMILY, "/** @license MIT */", "license", File),
    case(JS_FAMILY, "/** @preserve */", "directive:preserve", File),
    case(
        JS_FAMILY,
        "/*! keep this banner */",
        "directive:legal-comment",
        File,
    ),
    // Editor regions.
    case(JS_FAMILY, "// #region setup", "directive:region", File),
    case(JS_FAMILY, "// #endregion", "directive:endregion", File),
    // JSX and Flow pragmas.
    case(JS_FAMILY, "/** @jsx h */", "jsx-pragma", File),
    case(JS_FAMILY, "/** @jsxFrag Fragment */", "jsx-pragma", File),
    case(
        JS_FAMILY,
        "/** @jsxImportSource preact */",
        "jsx-pragma",
        File,
    ),
    case(JS_FAMILY, "/** @jsxRuntime classic */", "jsx-pragma", File),
    case(JS_FAMILY, "// @flow strict", "directive:flow", File),
    // Flow suppressions and comment types.
    case(
        JS_FAMILY,
        "// $FlowFixMe[incompatible-type]",
        "directive:flow-fix-me",
        Next,
    ),
    case(
        JS_FAMILY,
        "// $FlowExpectedError",
        "directive:flow-expected-error",
        Next,
    ),
    case(JS_FAMILY, "// $FlowIssue", "directive:flow-issue", Next),
    case(JS_FAMILY, "// $FlowIgnore", "directive:flow-ignore", Next),
    case(
        JS_FAMILY,
        "/*:: type T = number; */",
        "directive:flow-comment-type",
        Inline,
    ),
    case(
        JS_FAMILY,
        "/*: number */",
        "directive:flow-comment-type",
        Inline,
    ),
    // Test environments.
    case(
        JS_FAMILY,
        "/** @jest-environment jsdom */",
        "test-environment",
        File,
    ),
    case(
        JS_FAMILY,
        "/** @vitest-environment node */",
        "test-environment",
        File,
    ),
    // TypeScript's `stripInternal`: any comment kind.
    case(TS_ONLY, "// @internal", "directive:internal", Next),
    case(TS_ONLY, "/* @internal */", "directive:internal", Next),
];

// ---- Python. ----------------------------------------------------------------

const PY: &[LanguageId] = &[Python];

const PYTHON_CASES: &[Case] = &[
    case(
        PY,
        "# type: ignore[attr-defined]",
        "directive:type-ignore",
        Inline,
    ),
    case(PY, "# type: List[int]", "directive:type", Inline),
    case(
        PY,
        "# mypy: disallow-any-generics",
        "directive:mypy-disallow-any-generics",
        File,
    ),
    case(PY, "# pyright: strict", "directive:pyright-strict", File),
    case(PY, "# pyre-ignore[7]", "directive:pyre-ignore", Next),
    case(PY, "# pyre-fixme[6]: reason", "directive:pyre-fixme", Next),
    case(PY, "# pyre-strict", "directive:pyre-strict", File),
    // The file-level sibling keeps its file scope.
    case(
        PY,
        "# pyre-ignore-all-errors",
        "directive:pyre-ignore-all-errors",
        File,
    ),
    case(
        PY,
        "# pytype: disable=attribute-error",
        "directive:pytype-disable",
        File,
    ),
    case(PY, "# noqa: E501", "directive:noqa", Inline),
    case(PY, "# flake8: noqa", "directive:flake8-noqa", File),
    case(PY, "# ruff: noqa: F401", "directive:ruff-noqa", File),
    case(
        PY,
        "# pylint: disable=invalid-name",
        "directive:pylint-disable",
        File,
    ),
    case(
        PY,
        "# pylint: disable-next=line-too-long",
        "directive:pylint-disable-next",
        Next,
    ),
    case(PY, "# fmt: off", "directive:fmt-off", File),
    case(PY, "# fmt: skip", "directive:fmt-skip", Inline),
    case(PY, "# yapf: disable", "directive:yapf-disable", File),
    case(PY, "# isort: skip_file", "directive:isort-skip-file", File),
    case(
        PY,
        "# pragma: no cover",
        "directive:pragma-no-cover",
        Inline,
    ),
    case(
        PY,
        "# pragma: no branch",
        "directive:pragma-no-branch",
        Inline,
    ),
    case(
        PY,
        "# cython: language_level=3",
        "directive:cython-language-level",
        File,
    ),
    // tree-sitter-python swallows the lone CR into the comment; CPython
    // executes `x = 1`.
    case(PY, "# a\rx = 1", "lone-cr", Inline),
];

// ---- Go. --------------------------------------------------------------------

const GO: &[LanguageId] = &[Go];

const GO_CASES: &[Case] = &[
    case(GO, "//go:build linux", "directive:go-build", File),
    case(
        GO,
        "//go:generate stringer -type=T",
        "directive:go-generate",
        File,
    ),
    case(GO, "//go:noinline", "directive:go-noinline", File),
    case(GO, "// +build linux", "directive:build", File),
    case(GO, "//line foo.go:10", "directive:line", File),
    case(GO, "/*line foo.go:10:1*/", "directive:line", File),
    case(GO, "//export Foo", "directive:export", Next),
    case(GO, "//nolint:errcheck", "directive:nolint", Next),
    case(
        GO,
        "//noinspection GoUnusedExportedFunction",
        "directive:noinspection",
        Next,
    ),
    case(
        GO,
        "//lint:ignore SA1019 reason",
        "directive:lint-ignore",
        Next,
    ),
    case(
        GO,
        "//lint:file-ignore U1000 reason",
        "directive:lint-file-ignore",
        File,
    ),
    case(
        GO,
        "// Deprecated: use Bar instead.",
        "directive:deprecated",
        Next,
    ),
    case(
        GO,
        "// Code generated by stringer; DO NOT EDIT.",
        "directive:go-generated",
        File,
    ),
];

// ---- Java. ------------------------------------------------------------------

const JAVA: &[LanguageId] = &[Java];

const JAVA_CASES: &[Case] = &[
    case(JAVA, "//NOPMD - intentional", "directive:nopmd", Inline),
    case(JAVA, "// CHECKSTYLE:OFF", "directive:checkstyle-off", File),
    case(JAVA, "// CHECKSTYLE:ON", "directive:checkstyle-on", File),
    case(JAVA, "// @formatter:off", "directive:formatter-off", File),
    case(JAVA, "// @formatter:on", "directive:formatter-on", File),
    case(JAVA, "// spotless:off", "directive:spotless-off", File),
    case(JAVA, "// spotless:on", "directive:spotless-on", File),
    case(
        JAVA,
        "//noinspection unchecked",
        "directive:noinspection",
        Next,
    ),
    case(JAVA, "// $NON-NLS-1$", "directive:non-nls", Inline),
    case(JAVA, "// $FALL-THROUGH$", "directive:fall-through", Next),
    // javac decodes `\u000a` to a line break before lexing.
    case(JAVA, "// a \\u000a b", "unicode-escape", Inline),
    case(JAVA, "/* a \\uuu0041 */", "unicode-escape", Inline),
    case(JAVA, "// a\rfoo();", "lone-cr", Inline),
];

// ---- C / C++. ---------------------------------------------------------------

const C_CASES: &[Case] = &[
    // clang-tidy, clang-format.
    case(C_FAMILY, "// NOLINT", "directive:nolint", Inline),
    case(
        C_FAMILY,
        "// NOLINTNEXTLINE(bugprone-branch-clone)",
        "directive:nolintnextline",
        Next,
    ),
    case(C_FAMILY, "// NOLINTBEGIN", "directive:nolintbegin", File),
    case(C_FAMILY, "// NOLINTEND", "directive:nolintend", File),
    case(
        C_FAMILY,
        "// clang-format off",
        "directive:clang-format-off",
        File,
    ),
    case(
        C_FAMILY,
        "// clang-format on",
        "directive:clang-format-on",
        File,
    ),
    // GCC's `-Wimplicit-fallthrough=3` regexes, one case per regex.
    case(C_FAMILY, "// -fallthrough", "directive:fallthrough", Next),
    case(
        C_FAMILY,
        "/* @fallthrough@ */",
        "directive:fallthrough",
        Next,
    ),
    case(
        C_FAMILY,
        "/* lint -fallthrough */",
        "directive:fallthrough",
        Next,
    ),
    case(C_FAMILY, "/* FALLTHRU */", "directive:fallthrough", Next),
    case(C_FAMILY, "// Fall through.", "directive:fallthrough", Next),
    case(
        C_FAMILY,
        "// else fall through - see above",
        "directive:fallthrough",
        Next,
    ),
    // include-what-you-use, cppcheck, PC-lint, Coverity.
    case(
        C_FAMILY,
        "// IWYU pragma: keep",
        "directive:iwyu-pragma-keep",
        Inline,
    ),
    case(
        C_FAMILY,
        "// cppcheck-suppress nullPointer",
        "directive:cppcheck-suppress",
        Next,
    ),
    case(C_FAMILY, "//lint -e123", "directive:lint", File),
    case(C_FAMILY, "/*lint -save -e123 */", "directive:lint", File),
    case(
        C_FAMILY,
        "// coverity[dead_error_line]",
        "directive:coverity",
        Next,
    ),
    // lcov / gcovr exclusions.
    case(
        C_FAMILY,
        "// LCOV_EXCL_LINE",
        "directive:lcov-excl-line",
        Inline,
    ),
    case(
        C_FAMILY,
        "// GCOVR_EXCL_BR_LINE",
        "directive:gcovr-excl-br-line",
        Inline,
    ),
    case(
        C_FAMILY,
        "// LCOV_EXCL_START",
        "directive:lcov-excl-start",
        File,
    ),
    case(
        C_FAMILY,
        "// GCOVR_EXCL_STOP",
        "directive:gcovr-excl-stop",
        File,
    ),
    // Byte hazards: a backslash plus trailing blank, or a `??/` trigraph,
    // splices the next line into the comment for GCC/Clang only; a lone CR
    // ends the line for the compiler only.
    case(C_FAMILY, "// note \\ ", "line-splice", Inline),
    case(C_FAMILY, "// note ??/", "line-splice", Inline),
    case(C_FAMILY, "// a\rint y;", "lone-cr", Inline),
];

// ---- Rust. ------------------------------------------------------------------

const RS: &[LanguageId] = &[Rust];

const RUST_CASES: &[Case] = &[
    case(
        RS,
        "// SAFETY: the pointer is valid",
        "directive:safety",
        Next,
    ),
    case(
        RS,
        "//@ check-pass",
        "directive:compiletest-check-pass",
        File,
    ),
    case(
        RS,
        "//@ compile-flags: -O",
        "directive:compiletest-compile-flags",
        File,
    ),
    case(
        RS,
        "//~ ERROR mismatched types",
        "directive:compiletest-annotation",
        Inline,
    ),
];

// ---- Global rows (each exercised in at least two languages). ----------------

/// Languages whose line comments open with `//`.
const SLASH: &[LanguageId] = &[Javascript, Typescript, Tsx, Go, Java, C, Cpp, Rust];

const GLOBAL_CASES: &[Case] = &[
    case(SLASH, "// vim: set ts=4 sw=4 :", "modeline", File),
    case(PY, "# vim: set ts=4 sw=4 :", "modeline", File),
    case(
        &[Javascript, C, Rust],
        "// -*- mode: c; indent-tabs-mode: nil -*-",
        "modeline",
        File,
    ),
    case(PY, "# -*- mode: python -*-", "modeline", File),
    case(SLASH, "// SPDX-License-Identifier: MIT", "license", File),
    case(PY, "# SPDX-License-Identifier: MIT", "license", File),
    case(SLASH, "// NOSONAR", "directive:nosonar", Inline),
    case(PY, "# NOSONAR", "directive:nosonar", Inline),
    case(SLASH, "// nosemgrep: rule-id", "directive:nosemgrep", Next),
    case(PY, "# nosemgrep", "directive:nosemgrep", Next),
    case(
        SLASH,
        "// lgtm[js/unused-local-variable]",
        "directive:lgtm",
        Next,
    ),
    case(PY, "# lgtm[py/unused-import]", "directive:lgtm", Next),
    case(
        SLASH,
        "// codeql[js/unused-local-variable]",
        "directive:codeql",
        Next,
    ),
    case(PY, "# codeql[py/unused-import]", "directive:codeql", Next),
    case(
        SLASH,
        "// #nosec G101 -- test fixture",
        "directive:nosec",
        Next,
    ),
    case(PY, "# nosec B101", "directive:nosec", Next),
    case(PY, "# noqa  # nosec", "directive:nosec", Next),
    case(
        SLASH,
        "// gitleaks:allow",
        "directive:gitleaks-allow",
        Inline,
    ),
    case(PY, "# gitleaks:allow", "directive:gitleaks-allow", Inline),
    case(
        SLASH,
        "// pragma: allowlist secret",
        "directive:pragma-allowlist-secret",
        Inline,
    ),
    case(
        PY,
        "# pragma: allowlist secret",
        "directive:pragma-allowlist-secret",
        Inline,
    ),
    case(
        SLASH,
        "// pragma: allowlist nextline secret",
        "directive:pragma-allowlist-nextline-secret",
        Next,
    ),
    case(
        PY,
        "# pragma: allowlist nextline secret",
        "directive:pragma-allowlist-nextline-secret",
        Next,
    ),
];

const ALL_CASES: &[&[Case]] = &[
    JS_CASES,
    PYTHON_CASES,
    GO_CASES,
    JAVA_CASES,
    C_CASES,
    RUST_CASES,
    GLOBAL_CASES,
];

fn cases() -> impl Iterator<Item = &'static Case> {
    ALL_CASES.iter().flat_map(|cases| cases.iter())
}

#[test]
fn every_case_is_functional_with_its_tag_and_scope() {
    for case in cases() {
        for &language in case.languages {
            let text = own_line(language, case.comment);
            let record = record(language, &text, case.comment);
            assert_eq!(
                record.classification, Functional,
                "{language:?} {:?}",
                case.comment
            );
            assert!(
                record.tags.contains(case.tag),
                "{language:?} {:?}: tags {:?} lack {}",
                case.comment,
                record.tags,
                case.tag,
            );
            assert_eq!(
                record.directive_scope,
                Some(case.scope),
                "{language:?} {:?}",
                case.comment
            );
        }
    }
}

/// Every table row is matched — producing the tag its case asserts — by a
/// case in every language whose adapter carries it; every global row by
/// cases in at least two languages.
#[test]
fn every_row_is_covered_by_a_case_in_every_language_that_carries_it() {
    let mut covered: BTreeMap<LanguageId, BTreeSet<usize>> = BTreeMap::new();
    for case in cases() {
        for &language in case.languages {
            let adapter = adapter_for(language);
            let patterns = compiled(adapter).expect("pinned tables compile");
            let body = comment_body_with_lines(case.comment, adapter);
            for (index, row) in patterns.rows.iter().enumerate() {
                if row_matches(row, case.comment, &body, None)
                    && row_tag(row, case.comment, &body) == case.tag
                {
                    covered.entry(language).or_default().insert(index);
                }
            }
        }
    }
    let mut global_languages = vec![BTreeSet::new(); GLOBAL_DIRECTIVES.len()];
    for adapter in ALL_ADAPTERS {
        let own = adapter.directives.len();
        let hit = covered.get(&adapter.id).cloned().unwrap_or_default();
        for (index, row) in adapter.directives.iter().enumerate() {
            assert!(
                hit.contains(&index),
                "{:?}: no case covers row {:?} ({:?})",
                adapter.id,
                row.pattern,
                row.tag,
            );
        }
        for (index, languages) in global_languages.iter_mut().enumerate() {
            if hit.contains(&(own + index)) {
                languages.insert(adapter.id);
            }
        }
    }
    for (row, languages) in GLOBAL_DIRECTIVES.iter().zip(&global_languages) {
        assert!(
            languages.len() >= 2,
            "global row {:?} is covered in {languages:?}",
            row.pattern,
        );
    }
}

/// Rows with `scope_when_trailing`: next-scoped on their own line, inline
/// after code.
#[test]
fn trailing_directives_scope_inline() {
    let cases: &[(&[LanguageId], &str, &str)] = &[
        (GO, "//nolint:errcheck", "directive:nolint"),
        (
            GO,
            "//noinspection GoUnusedParameter",
            "directive:noinspection",
        ),
        (GO, "//lint:ignore SA4006 reason", "directive:lint-ignore"),
        (JAVA, "//noinspection unchecked", "directive:noinspection"),
        (
            C_FAMILY,
            "// cppcheck-suppress nullPointer",
            "directive:cppcheck-suppress",
        ),
        (PY, "# pyre-ignore[7]", "directive:pyre-ignore"),
        (
            PY,
            "# pyright: ignore[reportGeneralTypeIssues]",
            "directive:pyright-ignore",
        ),
        (
            PY,
            "# pytype: disable=attribute-error",
            "directive:pytype-disable",
        ),
        (
            PY,
            "# pylint: disable=invalid-name",
            "directive:pylint-disable",
        ),
        (PY, "# isort: skip", "directive:isort-skip"),
        (JS_FAMILY, "/*#__PURE__*/", "pure-annotation"),
        (
            JS_FAMILY,
            "/*@__NO_SIDE_EFFECTS__*/",
            "directive:no-side-effects",
        ),
        (SLASH, "// nosemgrep", "directive:nosemgrep"),
        (SLASH, "// lgtm[js/unused]", "directive:lgtm"),
        (SLASH, "// codeql[js/unused]", "directive:codeql"),
        (SLASH, "// #nosec G101", "directive:nosec"),
        (PY, "# nosec", "directive:nosec"),
    ];
    for &(languages, comment, tag) in cases {
        for &language in languages {
            let text = trailing(language, comment);
            let record = record(language, &text, comment);
            assert_eq!(
                record.position,
                Position::Trailing,
                "{language:?} {comment:?}"
            );
            assert!(
                record.tags.contains(tag),
                "{language:?} {comment:?}: {:?}",
                record.tags
            );
            assert_eq!(
                record.directive_scope,
                Some(Inline),
                "{language:?} {comment:?}"
            );
        }
    }
}

#[test]
fn coverage_line_windows_default_to_one_and_read_n() {
    let window = |language: LanguageId, comment: &str| {
        record(language, &own_line(language, comment), comment).directive_line_window
    };
    for &language in JS_FAMILY {
        assert_eq!(window(language, "/* c8 ignore next 3 */"), Some(3));
        assert_eq!(window(language, "/* c8 ignore next */"), Some(1));
        assert_eq!(window(language, "/* v8 ignore next 2 */"), Some(2));
        assert_eq!(
            window(language, "/* node:coverage ignore next 4 */"),
            Some(4)
        );
        assert_eq!(window(language, "/* c8 ignore start */"), None);
        assert_eq!(window(language, "/* istanbul ignore next */"), None);
    }
    assert_eq!(js_family_window("c8 ignore next 0"), 1);
    assert_eq!(js_family_window("c8 ignore next   12 -- reason"), 12);
    assert_eq!(js_family_window("c8 ignore next lines"), 1);
}

fn js_family_window(body: &str) -> usize {
    crate::lang::adapters::js_family::ignore_next_window(body)
}

#[test]
fn noqa_is_case_insensitive_as_flake8_and_ruff_read_it() {
    for comment in ["# NOQA: E501", "# NoQA", "# noqa", "#noqa:E501,W291"] {
        let record = record(Python, &own_line(Python, comment), comment);
        assert_eq!(record.classification, Functional, "{comment:?}");
        assert!(record.tags.contains("directive:noqa"), "{comment:?}");
    }
    for (comment, tag) in [
        ("# FLAKE8: NOQA", "directive:flake8-noqa"),
        ("# Ruff: NoQA", "directive:ruff-noqa"),
    ] {
        let record = record(Python, &own_line(Python, comment), comment);
        assert!(record.tags.contains(tag), "{comment:?}: {:?}", record.tags);
    }
}

#[test]
fn a_bare_trailing_backslash_does_not_trip_line_splice() {
    // tree-sitter-c already continues `// note \` onto the next line, so
    // the comment node holds both lines; nothing is hidden from it.
    for &language in C_FAMILY {
        let text = "// note \\\nint x;\nint y;\n";
        let comments = classify_source(language, text);
        let note = comments
            .iter()
            .find(|record| record.text.starts_with("// note"))
            .expect("the comment is scanned");
        assert!(
            note.text.contains("int x;"),
            "{language:?}: {:?}",
            note.text
        );
        assert!(!note.tags.contains("line-splice"), "{language:?}");
    }
    // A block comment is unaffected by splices inside it.
    for &language in C_FAMILY {
        let comment = "/* a \\ \n b */";
        let record = record(language, &own_line(language, comment), comment);
        assert!(!record.tags.contains("line-splice"), "{language:?}");
    }
}

#[test]
fn internal_is_a_directive_in_typescript_only() {
    for &language in TS_ONLY {
        let record = record(
            language,
            &own_line(language, "// @internal"),
            "// @internal",
        );
        assert_eq!(record.classification, Functional, "{language:?}");
    }
    let record = record(
        Javascript,
        &own_line(Javascript, "// @internal"),
        "// @internal",
    );
    assert_eq!(record.classification, Prose);
    assert!(record.tags.is_empty(), "{:?}", record.tags);
}

/// Literal rows follow the derivation rule: a `Fixed` `directive:` tag is
/// the derived tag of the literal, and a literal `Family` prefix is the
/// derived tag plus `-` (so `//go:build` → `directive:go-build`). Rows in a
/// named category carry the bare category tag instead.
#[test]
fn literal_rows_follow_the_tag_derivation_rule() {
    let rows = ALL_ADAPTERS
        .iter()
        .flat_map(|adapter| adapter.directives)
        .chain(GLOBAL_DIRECTIVES);
    for row in rows {
        let Pattern::Lit(literal) = row.pattern else {
            continue;
        };
        let derived = derive_directive_tag(literal).expect("literal rows have names");
        match row.tag {
            TagSpec::Fixed(tag) if tag.starts_with("directive:") => {
                assert_eq!(tag, derived, "{literal:?}");
            }
            // A named category's bare tag (`license`, `pure-annotation`,
            // `test-environment`).
            TagSpec::Fixed(_) => {}
            TagSpec::Family { prefix } => assert_eq!(prefix, format!("{derived}-"), "{literal:?}"),
        }
    }
}

/// Prose that mentions a tool, or a directive word outside the directive
/// form, stays prose with no tags.
#[test]
fn prose_mentions_are_not_directives() {
    let negatives: &[(&[LanguageId], &str)] = &[
        // Mentions of eslint / global / tool names.
        (JS_FAMILY, "// we lint with eslint before every release"),
        (JS_FAMILY, "// global state lives in the store"),
        (JS_FAMILY, "/* we keep exported names stable */"),
        (JS_FAMILY, "// the c8 tool reports coverage"),
        (JS_FAMILY, "// see the webpack config for chunk names"),
        (JS_FAMILY, "// the formatter runs on save"),
        (JS_FAMILY, "/* the flowchart below */"),
        // noqa / nosec / type mentions.
        (PY, "# we never use noqa in this module"),
        (PY, "# bandit nosec comments are audited weekly"),
        (PY, "# the type of x is an int"),
        (PY, "# pragmatic choice: no coverage here"),
        (GO, "// gosec flags this; we do not add nosec here"),
        (C_FAMILY, "// this nosec mention is prose"),
        // GCC applies its fall-through regexes to the whole comment.
        (C_FAMILY, "/* we fall through to the default */"),
        (C_FAMILY, "// linting is enforced in CI"),
        // Go's Deprecated paragraph is case-sensitive and paragraph-initial.
        (GO, "// deprecated: old path"),
        (GO, "// This was Deprecated: long ago"),
        (GO, "// Code generated by hand, please edit."),
        // Rust: lowercase safety notes and spaced annotations.
        (RS, "// safety first"),
        (RS, "// a ~ b"),
        // Java: mentions without the marker form.
        (JAVA, "// the checkstyle config lives elsewhere"),
    ];
    for &(languages, comment) in negatives {
        for &language in languages {
            let record = record(language, &own_line(language, comment), comment);
            assert_eq!(record.classification, Prose, "{language:?} {comment:?}");
            assert!(
                record.tags.is_empty(),
                "{language:?} {comment:?}: {:?}",
                record.tags
            );
            assert_eq!(record.directive_scope, None, "{language:?} {comment:?}");
        }
    }
}
