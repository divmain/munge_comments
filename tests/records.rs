//! Draft records built from golden-fixture inventories: the single record
//! builder fills every structural field from the inventory, decides
//! protected records (row 1) and the pre-LLM rows 1b–3, and leaves every
//! other record undecided.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use munge_comments::artifact::types::{DecidedBy, Decision};
use munge_comments::comments::classify::TAG_FLAG_TAGS;
use munge_comments::comments::sanitize::is_content_empty;
use munge_comments::comments::types::Classification;
use munge_comments::engine::{DraftRecord, build_draft_records, finalize_record};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{Error, FileRef, Inventory, inventory_file, language_for};

/// The inventory and draft records of a fixture file.
fn build(path: &Path) -> (Inventory, Vec<DraftRecord>) {
    let bytes = fs::read(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    let text = String::from_utf8(bytes).expect("golden fixtures are UTF-8");
    let language = language_for(path).expect("golden fixtures have a source extension");
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let inventory = inventory_file(
        &text,
        FileRef {
            language,
            file_name,
        },
    )
    .unwrap_or_else(|err| panic!("inventorying {}: {err}", path.display()));
    let drafts = build_draft_records(&inventory, adapter_for(language));
    (inventory, drafts)
}

/// The draft records of `golden/<rel>`.
fn drafts(rel: &str) -> Vec<DraftRecord> {
    build(&common::fixture_path(Path::new("golden").join(rel))).1
}

/// The record whose original text contains `needle`.
fn find<'a>(records: &'a [DraftRecord], needle: &str) -> &'a DraftRecord {
    records
        .iter()
        .find(|record| {
            record
                .original
                .as_deref()
                .is_some_and(|text| text.contains(needle))
        })
        .unwrap_or_else(|| panic!("no record contains {needle:?}"))
}

/// `(decision, decidedBy, confidence)`, all three present or all absent.
fn verdict(record: &DraftRecord) -> Option<(Decision, DecidedBy, f64)> {
    match (record.decision, record.decided_by, record.confidence) {
        (Some(decision), Some(decided_by), Some(confidence)) => {
            Some((decision, decided_by, confidence))
        }
        (None, None, None) => None,
        partial => panic!("{}: partially decided {partial:?}", record.id),
    }
}

/// Every golden fixture file, sorted.
fn golden_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for directory in fs::read_dir(common::fixture_path("golden")).expect("golden/ is listable") {
        let directory = directory.expect("golden/ is listable").path();
        for file in fs::read_dir(&directory).expect("golden/<language>/ is listable") {
            files.push(file.expect("golden/<language>/ is listable").path());
        }
    }
    files.sort();
    files
}

#[test]
fn every_golden_record_is_built_from_its_inventory_and_decided_per_the_matrix() {
    for path in golden_files() {
        let (inventory, drafts) = build(&path);
        let adapter = adapter_for(language_for(&path).expect("source extension"));
        assert_eq!(drafts.len(), inventory.comments.len(), "{}", path.display());
        for (comment, record) in inventory.comments.iter().zip(&drafts) {
            let at = format!("{} {}", path.display(), record.id);
            // Structural fields come straight from the inventory.
            assert_eq!(record.id, comment.id, "{at}");
            assert_eq!(record.kind, comment.kind, "{at}");
            assert_eq!(
                (record.start, record.end),
                (comment.start, comment.end),
                "{at}"
            );
            assert_eq!(
                (record.line, record.column),
                (comment.line, comment.column),
                "{at}"
            );
            assert_eq!(record.position, comment.position, "{at}");
            assert_eq!(record.classification, comment.classification, "{at}");
            assert_eq!(
                record.original.as_deref(),
                Some(comment.text.as_str()),
                "{at}"
            );
            assert_eq!(record.anchor, comment.anchor, "{at}");
            // Model fields are untouched.
            assert_eq!(record.regenerated, None, "{at}");
            assert_eq!(record.would_you_comment_here, None, "{at}");
            assert_eq!(record.difficulty, None, "{at}");
            assert_eq!(record.final_comment, None, "{at}");
            assert_eq!(record.decision_rationale, None, "{at}");
            // Tags: the inventory's, plus row 2's flag; sorted and unique.
            let mut sorted = record.tags.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(record.tags, sorted, "{at}");
            for tag in &comment.tags {
                assert!(record.has_tag(tag), "{at}: lost {tag}");
            }

            let tag_flagged = TAG_FLAG_TAGS.iter().any(|tag| comment.tags.contains(*tag));
            let expected = if comment.classification != Classification::Prose {
                Some((Decision::KeepProtected, DecidedBy::Protected, 1.0))
            } else if comment.tags.contains("jsx-expression") || tag_flagged {
                Some((Decision::KeepOriginal, DecidedBy::Heuristic, 1.0))
            } else if is_content_empty(&comment.text, adapter) {
                Some((Decision::Remove, DecidedBy::Heuristic, 1.0))
            } else {
                None
            };
            assert_eq!(verdict(record), expected, "{at}");
            let flagged = comment.classification == Classification::Prose
                && !comment.tags.contains("jsx-expression")
                && tag_flagged;
            assert_eq!(
                record.has_tag("flagged-for-review"),
                flagged,
                "{at}: flagged-for-review"
            );
            // Exactly the decided records finalize.
            assert_eq!(
                finalize_record(record.clone()).is_ok(),
                expected.is_some(),
                "{at}"
            );
        }
    }
}

#[test]
fn protected_records_are_decided() {
    let records = drafts("python/noqa.py");
    for needle in ["# noqa: F401", "# noqa: E501", "# nosec B105"] {
        let record = find(&records, needle);
        assert_eq!(record.classification, Classification::Functional);
        assert_eq!(
            verdict(record),
            Some((Decision::KeepProtected, DecidedBy::Protected, 1.0)),
            "{needle}"
        );
    }
    assert_eq!(verdict(find(&records, "A long line follows")), None);
}

#[test]
fn tag_flagged_records_are_pre_decided() {
    let records = drafts("python/tag-flags.py");
    for (needle, tags) in [
        ("# TODO: x", &["flagged-for-review", "todo"][..]),
        (
            "# FIXME and HACK",
            &["fixme", "flagged-for-review", "hack"][..],
        ),
        ("# XXX", &["flagged-for-review", "xxx"][..]),
    ] {
        let record = find(&records, needle);
        assert_eq!(
            verdict(record),
            Some((Decision::KeepOriginal, DecidedBy::Heuristic, 1.0)),
            "{needle}"
        );
        assert_eq!(record.tags, tags, "{needle}");
    }
    for needle in ["# Notes on NOTEWORTHY", "# todo in lowercase"] {
        let record = find(&records, needle);
        assert_eq!(verdict(record), None, "{needle}");
        assert!(record.tags.is_empty(), "{needle}");
    }
}

#[test]
fn content_empty_records_are_pre_decided() {
    let records = drafts("typescript/empty-comments.ts");
    assert_eq!(records.len(), 6);
    for record in &records {
        assert_eq!(
            verdict(record),
            Some((Decision::Remove, DecidedBy::Heuristic, 1.0)),
            "{:?}",
            record.original
        );
    }
}

#[test]
fn jsx_container_comments_keep_and_the_rest_stay_undecided() {
    let records = drafts("tsx/jsx-expression.tsx");
    for needle in ["The sole content of a container.", "A line comment alone"] {
        assert_eq!(
            verdict(find(&records, needle)),
            Some((Decision::KeepOriginal, DecidedBy::Heuristic, 1.0)),
            "{needle}"
        );
    }
    let beside = find(&records, "beside an expression");
    assert_eq!(verdict(beside), None);
    assert!(matches!(
        finalize_record(beside.clone()),
        Err(Error::Invariant(_))
    ));
}
