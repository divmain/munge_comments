//! Replacement rendering over the golden corpus: every scanned record of
//! every golden fixture yields a slot, the source-free slot agrees with
//! the source-derived one, and rendering a record's own body back into its
//! slot preserves the comment's content.
//!
//! The byte-exact layout tests live next to the renderer
//! (`src/apply/render.rs`), on inline inputs.

mod common;

use std::fs;
use std::path::PathBuf;

use munge_comments::apply::render::{render_comment, slot_for, slot_from_record};
use munge_comments::artifact::types::{CommentRecord, Decision};
use munge_comments::comments::sanitize::{comment_body, match_key};
use munge_comments::comments::types::{CommentKind, Position};
use munge_comments::lang::adapter::SlotKind;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{FileRef, LanguageId, inventory_file, language_for};

/// One golden fixture: its key, language, text and scanned records.
struct Golden {
    key: String,
    language: LanguageId,
    text: String,
    records: Vec<CommentRecord>,
}

/// Every golden fixture file, sorted.
fn golden_files() -> Vec<PathBuf> {
    let root = common::fixture_path("golden");
    let mut files = Vec::new();
    for directory in fs::read_dir(&root).expect("tests/fixtures/golden is readable") {
        let directory = directory.expect("golden/ is listable").path();
        for file in fs::read_dir(&directory).expect("a golden directory is readable") {
            files.push(file.expect("a golden directory is listable").path());
        }
    }
    files.sort();
    files
}

/// Every golden fixture with its records, as a hand-authored artifact
/// would hold them.
fn goldens() -> Vec<Golden> {
    let root = common::fixture_path("golden");
    let goldens: Vec<Golden> = golden_files()
        .into_iter()
        .map(|path| {
            let key = path
                .strip_prefix(&root)
                .expect("under golden/")
                .to_string_lossy()
                .replace('\\', "/");
            let language = language_for(&path).expect("a golden fixture has a known extension");
            let text = String::from_utf8(fs::read(&path).expect("fixture is readable"))
                .expect("fixture is UTF-8");
            let file_name = path
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            let inventory = inventory_file(
                &text,
                FileRef {
                    language,
                    file_name,
                },
            )
            .unwrap_or_else(|err| panic!("inventorying {key}: {err}"));
            let plan = common::file_plan_from_inventory(&key, &text, &inventory, language, |_| {
                (Decision::KeepOriginal, None)
            });
            Golden {
                key,
                language,
                text,
                records: plan.comments,
            }
        })
        .collect();
    assert!(
        goldens
            .iter()
            .map(|golden| golden.records.len())
            .sum::<usize>()
            > 100,
        "the golden corpus is unexpectedly small"
    );
    goldens
}

#[test]
fn slot_from_record_matches_slot_for_on_every_golden_record() {
    for golden in goldens() {
        let adapter = adapter_for(golden.language);
        for record in &golden.records {
            let from_text = slot_for(&golden.text, record, adapter)
                .unwrap_or_else(|err| panic!("{} {}: {err}", golden.key, record.id));
            let from_record = slot_from_record(record, adapter);
            let context = format!("{} {} {:?}", golden.key, record.id, record.original);
            assert_eq!(from_text.opener, from_record.opener, "{context}");
            assert_eq!(from_text.kind, from_record.kind, "{context}");
            assert_eq!(from_text.trailing, from_record.trailing, "{context}");
            assert_eq!(from_text.block_close, from_record.block_close, "{context}");

            let original = record.original.as_deref().expect("scanned");
            let block_shaped = adapter
                .block_delimiters
                .is_some_and(|delimiters| original.starts_with(delimiters.open));
            let expected_kind = match (block_shaped, record.kind) {
                (false, _) => SlotKind::Line,
                (true, CommentKind::Doc) => SlotKind::Doc,
                (true, _) => SlotKind::Block,
            };
            assert_eq!(from_text.kind, expected_kind, "{context}");
            assert!(original.starts_with(&from_text.opener), "{context}");
            assert_eq!(
                from_text.trailing,
                record.position == Position::Trailing,
                "{context}"
            );
        }
    }
}

#[test]
fn every_golden_slot_rerenders_its_own_content() {
    for golden in goldens() {
        let adapter = adapter_for(golden.language);
        for record in &golden.records {
            let original = record.original.as_deref().expect("scanned");
            let slot = slot_for(&golden.text, record, adapter).expect("slot");
            let rendered = render_comment(&comment_body(original, adapter), &slot);
            let context = format!("{} {} {original:?} -> {rendered:?}", golden.key, record.id);
            assert!(rendered.starts_with(&slot.opener), "{context}");
            if slot.kind != SlotKind::Line {
                assert!(rendered.trim_end().ends_with(slot.block_close), "{context}");
            }
            if slot.inline || slot.trailing {
                let lines = rendered.trim_end_matches(['\r', '\n']);
                assert!(!lines.contains('\n'), "{context}");
            }
            // Rendering must not mix line endings: LF slots stay LF, CRLF slots use CRLF throughout.
            if slot.eol == "\n" {
                assert!(!rendered.contains("\r\n"), "{context}");
            } else {
                assert_eq!(
                    rendered.matches('\n').count(),
                    rendered.matches("\r\n").count(),
                    "{context}"
                );
            }
            // An inline block slot that held a line break keeps one after
            // the comment; the rest is the comment itself.
            let keeps_break =
                slot.kind != SlotKind::Line && slot.inline && slot.span_had_line_break;
            let comment = if keeps_break {
                rendered
                    .strip_suffix(slot.eol)
                    .unwrap_or_else(|| panic!("{context}: no kept line break"))
            } else {
                rendered.as_str()
            };
            assert_eq!(
                match_key(comment, adapter),
                match_key(original, adapter),
                "{context}"
            );
        }
    }
}
