//! The README stays true to the tool:
//!
//! - its supported-languages table lists exactly the adapter ids in
//!   `ALL_ADAPTERS`;
//! - the `plan --help` output it embeds equals what `run` prints;
//! - every `munge-comments` command in its `console` blocks parses under
//!   clap, and together they form the install → plan → apply → report
//!   walkthrough.
//!
//! It drives `cli::run`, hence the `cli` feature gate.

#![cfg(feature = "cli")]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use clap::Parser;
use clap::error::ErrorKind;
use munge_comments::cli::{Cli, Io, run};
use munge_comments::lang::adapters::ALL_ADAPTERS;

use common::SharedBuf;

const PLAN_HELP_START: &str = "<!-- plan-help:start -->";
const PLAN_HELP_END: &str = "<!-- plan-help:end -->";
const LANGUAGES_HEADING: &str = "## Supported languages";

fn readme() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

/// The lines of the `## …` section whose heading line is exactly
/// `heading`, up to (excluding) the next `## ` heading.
fn section<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let mut lines = text.lines().skip_while(|line| *line != heading);
    assert_eq!(
        lines.next(),
        Some(heading),
        "README has no {heading:?} section"
    );
    lines.take_while(|line| !line.starts_with("## ")).collect()
}

/// A fenced code block: its info string and its lines.
struct Fence<'a> {
    info: &'a str,
    lines: Vec<&'a str>,
}

/// Every fenced (```` ``` ````) code block in `text`, in order.
fn fences(text: &str) -> Vec<Fence<'_>> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some(info) = line.strip_prefix("```") {
            let body: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
            out.push(Fence {
                info: info.trim(),
                lines: body,
            });
        }
    }
    out
}

/// Splits a POSIX shell command line into words: whitespace separates
/// words, single quotes are literal, double quotes allow `\"`/`\\`
/// escapes, a backslash outside quotes escapes the next character, and an
/// unquoted `#` at the start of a word begins a comment. Panics on an
/// unterminated quote (a README typo worth failing on).
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if let Some(done) = word.take() {
                    words.push(done);
                }
            }
            '#' if word.is_none() => break,
            '\'' => {
                let current = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => panic!("unterminated single quote in {line:?}"),
                    }
                }
            }
            '"' => {
                let current = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => current.push(c),
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => panic!("unterminated double quote in {line:?}"),
                        },
                        Some(c) => current.push(c),
                        None => panic!("unterminated double quote in {line:?}"),
                    }
                }
            }
            '\\' => {
                let escaped = chars
                    .next()
                    .unwrap_or_else(|| panic!("trailing backslash in {line:?}"));
                word.get_or_insert_with(String::new).push(escaped);
            }
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    words
}

/// Whether `word` is a leading `NAME=value` environment assignment.
fn is_env_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    })
}

/// Every command typed in the README's `console` blocks (`$ ` lines), as
/// shell words with leading environment assignments dropped.
fn console_commands(text: &str) -> Vec<Vec<String>> {
    fences(text)
        .into_iter()
        .filter(|fence| fence.info == "console")
        .flat_map(|fence| fence.lines)
        .filter_map(|line| line.strip_prefix("$ "))
        .map(|command| {
            shell_words(command)
                .into_iter()
                .skip_while(|word| is_env_assignment(word))
                .collect::<Vec<_>>()
        })
        .filter(|words| !words.is_empty())
        .collect()
}

#[test]
fn the_supported_languages_table_lists_every_adapter_id() {
    let text = readme();
    let rows: Vec<&str> = section(&text, LANGUAGES_HEADING)
        .into_iter()
        .filter(|line| line.starts_with('|'))
        .collect();
    assert!(rows.len() > 2, "no table under {LANGUAGES_HEADING:?}");
    assert!(
        rows[0].starts_with("| Id |"),
        "the table's first column must be the adapter id: {:?}",
        rows[0]
    );
    let listed: Vec<&str> = rows[2..]
        .iter()
        .map(|row| {
            let cell = row.split('|').nth(1).unwrap_or_default().trim();
            cell.strip_prefix('`')
                .and_then(|cell| cell.strip_suffix('`'))
                .unwrap_or_else(|| panic!("Id cell must be a backticked adapter id: {row:?}"))
        })
        .collect();
    let listed_set: BTreeSet<&str> = listed.iter().copied().collect();
    assert_eq!(listed.len(), listed_set.len(), "duplicate rows: {listed:?}");
    let adapters: BTreeSet<&str> = ALL_ADAPTERS
        .iter()
        .map(|adapter| adapter.id.as_str())
        .collect();
    assert_eq!(
        listed_set, adapters,
        "the README's supported-languages table must list exactly the adapter ids"
    );
}

/// The README embeds `plan --help` verbatim. Trailing whitespace is
/// ignored per line (clap pads some blank lines; editors strip it from
/// markdown), everything else must match exactly.
#[tokio::test]
async fn the_embedded_plan_help_matches_the_binary() {
    let out = SharedBuf::new();
    let err = SharedBuf::new();
    let cwd = common::tempdir();
    let io = Io::new(
        Box::new(out.clone()),
        Box::new(err.clone()),
        cwd.path().to_path_buf(),
        HashMap::new(),
    );
    let code = run(["munge-comments", "plan", "--help"], io).await;
    assert_eq!(code, 0, "stderr: {}", err.contents());
    let help = out.contents();
    assert!(!help.is_empty());

    let text = readme();
    let start = text
        .find(PLAN_HELP_START)
        .expect("README embeds the plan --help output");
    let end = text[start..]
        .find(PLAN_HELP_END)
        .map(|offset| start + offset)
        .expect("the plan --help block is closed");
    let block = &text[start + PLAN_HELP_START.len()..end];
    let embedded = block
        .trim_matches('\n')
        .strip_prefix("```text\n")
        .and_then(|body| body.strip_suffix("\n```"))
        .expect("the plan --help output sits in one ```text fence between the markers");

    let normalize = |s: &str| -> Vec<String> {
        s.trim_end_matches('\n')
            .lines()
            .map(|line| line.trim_end().to_owned())
            .collect()
    };
    assert_eq!(
        normalize(embedded),
        normalize(&help),
        "README's plan --help is stale; paste the current `munge-comments plan --help` output"
    );
}

#[test]
fn every_readme_command_parses_and_the_walkthrough_is_complete() {
    let text = readme();
    let commands = console_commands(&text);
    let mut tool_commands = Vec::new();
    for words in &commands {
        if words[0] != "munge-comments" {
            continue;
        }
        match Cli::try_parse_from(words) {
            Ok(_) => tool_commands.push(words.clone()),
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) => {}
            Err(err) => panic!("README command {words:?} does not parse:\n{}", err.render()),
        }
    }

    let has = |predicate: &dyn Fn(&[String]) -> bool| commands.iter().any(|w| predicate(w));
    assert!(
        has(&|w| w.starts_with(&["cargo".into(), "install".into(), "--locked".into()])),
        "the walkthrough installs with `cargo install --locked`"
    );
    let tool = |predicate: &dyn Fn(&[String]) -> bool| tool_commands.iter().any(|w| predicate(w));
    let flag = |w: &[String], f: &str| w.iter().any(|word| word == f);
    assert!(tool(&|w| w[1] == "plan"), "the walkthrough runs `plan`");
    assert!(
        tool(&|w| w[1] == "apply" && flag(w, "--dry-run")),
        "the walkthrough previews with `apply --dry-run`"
    );
    assert!(
        tool(&|w| w[1] == "apply" && !flag(w, "--dry-run")),
        "the walkthrough runs `apply`"
    );
    assert!(tool(&|w| w[1] == "report"), "the walkthrough runs `report`");
    assert!(
        text.contains("## The plan artifact and how to edit it"),
        "the README documents reviewing and editing the artifact"
    );
}

#[test]
fn shell_word_splitting_handles_quotes_comments_and_assignments() {
    assert_eq!(
        shell_words(r#"munge-comments plan --exclude 'a b/**' "c \"d\"" e\ f  # note"#),
        [
            "munge-comments",
            "plan",
            "--exclude",
            "a b/**",
            r#"c "d""#,
            "e f"
        ]
    );
    assert_eq!(shell_words("a#b"), ["a#b"]);
    assert_eq!(shell_words("''"), [""]);
    assert!(is_env_assignment("MUNGE_COMMENTS_E2E=1"));
    assert!(!is_env_assignment("--out=x"));
    assert!(!is_env_assignment("plan"));
}
