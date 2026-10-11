//! The noisy-repo ground truth stays consistent with its fixture tree, and
//! the `FakeLlm` scenario generated from it covers every entry with the
//! behavior its label calls for.
//!
//! - Every manifest entry matches exactly one inventoried comment (path +
//!   `match_key` + anchor name), every inventoried comment has an entry,
//!   `(path, key)` pairs are unique, and labels agree with the scanner's
//!   classification (prose labels on prose, `functional` on functional,
//!   `protected` on doc-protected).
//! - Pre-LLM-decided prose is labeled by its intended outcome.
//! - Every adapter id has at least one `superfluous` and one `valuable`
//!   entry, and the fixture exercises the traps the plan calls for.
//! - The fixture parses cleanly and holds no placeholder-shaped text.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use common::noisy_scenario::{
    JUDGE_CONFIDENCE, Label, MANIFEST_FILE, Manifest, ManifestEntry, NOISY_REPO,
    SIMILARITY_THRESHOLD, extra_regenerations, load_manifest, near_identical, noisy_scenario,
    rewrite_text, rewritten_key, scenario_key,
};
use munge_comments::apply::render::scanned_slot_kind;
use munge_comments::artifact::types::{Difficulty, JudgeDecision};
use munge_comments::comments::classify::TAG_FLAG_TAGS;
use munge_comments::comments::sanitize::{is_content_empty, match_key, validate_final_comment};
use munge_comments::comments::scan::list_declarations;
use munge_comments::comments::types::Classification;
use munge_comments::engine::judge::TIE_SIMILARITY;
use munge_comments::lang::adapter::SlotKind;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{
    FileRef, InventoryComment, LanguageId, inventory_file, language_for, similarity,
};
use regex::Regex;

/// The tag of a comment that is the sole content of a JSX `{/* … */}` container.
const JSX_EXPRESSION_TAG: &str = "jsx-expression";

/// One inventoried fixture comment.
struct Scanned {
    path: String,
    language: LanguageId,
    key: String,
    comment: InventoryComment,
}

impl Scanned {
    fn describe(&self) -> String {
        format!(
            "{} {} (line {}, anchor {:?}): {:?}",
            self.path, self.comment.id, self.comment.line, self.comment.anchor.name, self.key
        )
    }

    fn matches(&self, entry: &ManifestEntry) -> bool {
        self.path == entry.path
            && self.key == entry.key
            && self.comment.anchor.name == entry.anchor_name
    }
}

fn root() -> PathBuf {
    common::fixture_path(NOISY_REPO)
}

/// Every file under the fixture root, as sorted root-relative POSIX paths.
fn fixture_files() -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in
            fs::read_dir(dir).unwrap_or_else(|err| panic!("listing {}: {err}", dir.display()))
        {
            let path = entry.expect("fixture entries are readable").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("walked paths are under the root");
                let parts: Vec<String> = rel
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    let root = root();
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    files.sort();
    files
}

/// The source files: every fixture file with a source extension.
fn source_files() -> Vec<(String, LanguageId)> {
    fixture_files()
        .into_iter()
        .filter_map(|path| language_for(Path::new(&path)).map(|language| (path, language)))
        .collect()
}

fn read(path: &str) -> String {
    let full = root().join(path);
    fs::read_to_string(&full).unwrap_or_else(|err| panic!("reading {}: {err}", full.display()))
}

/// Every inventoried comment of the fixture tree.
fn scanned() -> Vec<Scanned> {
    let mut all = Vec::new();
    for (path, language) in source_files() {
        let text = read(&path);
        let inventory = inventory_file(
            &text,
            FileRef {
                language,
                file_name: path.clone(),
            },
        )
        .unwrap_or_else(|err| panic!("scanning {path}: {err}"));
        for comment in inventory.comments {
            all.push(Scanned {
                key: match_key(&comment.text, adapter_for(language)),
                path: path.clone(),
                language,
                comment,
            });
        }
    }
    all
}

fn label_fits(label: Label, classification: Classification) -> bool {
    match classification {
        Classification::Prose => label.is_prose(),
        Classification::Functional => label == Label::Functional,
        Classification::DocProtected => label == Label::Protected,
    }
}

/// The entry `comment` must have, panicking unless there is exactly one.
fn entry_of<'m>(manifest: &'m Manifest, comment: &Scanned) -> &'m ManifestEntry {
    let found: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|entry| comment.matches(entry))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{} has {} manifest entries",
        comment.describe(),
        found.len()
    );
    found[0]
}

#[test]
fn the_manifest_is_never_discovered() {
    assert!(language_for(Path::new(MANIFEST_FILE)).is_none());
    let others: Vec<String> = fixture_files()
        .into_iter()
        .filter(|path| path != MANIFEST_FILE && language_for(Path::new(path)).is_none())
        .collect();
    assert!(
        others.is_empty(),
        "non-source files in the noisy repo: {others:?}"
    );
}

#[test]
fn every_entry_matches_exactly_one_inventoried_comment() {
    let manifest = load_manifest();
    let scanned = scanned();
    let mut failures = Vec::new();
    for entry in &manifest.entries {
        let matches = scanned
            .iter()
            .filter(|comment| comment.matches(entry))
            .count();
        if matches != 1 {
            failures.push(format!("{entry:?} matches {matches} comments"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_inventoried_comment_has_one_entry_whose_label_fits_its_classification() {
    let manifest = load_manifest();
    let mut failures = Vec::new();
    for comment in scanned() {
        let entries: Vec<&ManifestEntry> = manifest
            .entries
            .iter()
            .filter(|entry| comment.matches(entry))
            .collect();
        match entries.as_slice() {
            [entry] if !label_fits(entry.label, comment.comment.classification) => {
                failures.push(format!(
                    "{} is {:?} but labeled {}",
                    comment.describe(),
                    comment.comment.classification,
                    entry.label.as_str()
                ))
            }
            [_] => {}
            _ => failures.push(format!(
                "{} has {} entries",
                comment.describe(),
                entries.len()
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn path_key_pairs_are_unique_and_rewrite_texts_belong_to_rewrites() {
    let manifest = load_manifest();
    let mut seen = BTreeSet::new();
    for entry in &manifest.entries {
        assert!(
            seen.insert((entry.path.as_str(), entry.key.as_str())),
            "duplicate (path, key): {} / {:?}",
            entry.path,
            entry.key
        );
        assert_eq!(
            entry.rewrite_to.is_some(),
            entry.label == Label::Rewrite,
            "rewriteTo is present exactly on rewrite entries: {entry:?}"
        );
    }
}

#[test]
fn pre_llm_decided_prose_is_labeled_by_its_intended_outcome() {
    let manifest = load_manifest();
    let mut failures = Vec::new();
    for comment in scanned() {
        let entry = entry_of(&manifest, &comment);
        let tags = &comment.comment.tags;
        let expected = if tags.contains("directive-adjacent") {
            Some(Label::Functional)
        } else if comment.comment.classification != Classification::Prose {
            None
        } else if tags.contains(JSX_EXPRESSION_TAG)
            || TAG_FLAG_TAGS.iter().any(|tag| tags.contains(*tag))
        {
            Some(Label::Valuable)
        } else if is_content_empty(&comment.comment.text, adapter_for(comment.language)) {
            Some(Label::Superfluous)
        } else {
            None
        };
        if let Some(expected) = expected
            && entry.label != expected
        {
            failures.push(format!(
                "{} is labeled {}, expected {}",
                comment.describe(),
                entry.label.as_str(),
                expected.as_str()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_adapter_id_has_superfluous_and_valuable_entries() {
    let manifest = load_manifest();
    let mut counts: BTreeMap<(LanguageId, Label), usize> = BTreeMap::new();
    for entry in &manifest.entries {
        *counts.entry((entry.language(), entry.label)).or_default() += 1;
    }
    for language in LanguageId::ALL {
        for label in [Label::Superfluous, Label::Valuable] {
            assert!(
                counts.get(&(language, label)).copied().unwrap_or(0) >= 1,
                "{} has no {} entry",
                language.as_str(),
                label.as_str()
            );
        }
    }
    for label in Label::ALL {
        assert!(
            manifest.with_label(label).next().is_some(),
            "no {} entries",
            label.as_str()
        );
    }
}

#[test]
fn the_fixture_exercises_the_planned_traps() {
    let manifest = load_manifest();
    let scanned = scanned();
    let has = |what: &str, pred: &dyn Fn(&Scanned, &ManifestEntry) -> bool| {
        assert!(
            scanned
                .iter()
                .any(|comment| pred(comment, entry_of(&manifest, comment))),
            "the noisy repo lacks {what}"
        );
    };
    let tagged = |comment: &Scanned, tag: &str| comment.comment.tags.contains(tag);
    has("a superfluous dangling end-of-file comment", &|c, e| {
        c.comment.anchor.node_kind == "end_of_file"
            && e.label == Label::Superfluous
            && !c.key.is_empty()
    });
    has("a Go example `// Output:` block", &|c, e| {
        c.language == LanguageId::Go && tagged(c, "example-output") && e.label == Label::Functional
    });
    has("a Go build tag", &|c, _| tagged(c, "directive:go-build"));
    has("a Python type comment", &|c, _| {
        c.language == LanguageId::Python && tagged(c, "directive:type")
    });
    has("an exported JS JSDoc (doc-protected)", &|c, e| {
        c.language == LanguageId::Javascript
            && c.comment.anchor_exported
            && e.label == Label::Protected
    });
    has("a Java Javadoc (doc-protected)", &|c, e| {
        c.language == LanguageId::Java && e.label == Label::Protected
    });
    has("a Rust doc comment (doc-protected)", &|c, e| {
        c.language == LanguageId::Rust && e.label == Label::Protected
    });
    has("a TODO", &|c, _| tagged(c, "todo"));
    has("directive-adjacent prose", &|c, _| {
        tagged(c, "directive-adjacent")
    });
    has("a JSX expression comment", &|c, _| {
        tagged(c, JSX_EXPRESSION_TAG)
    });
}

#[test]
fn the_fixture_parses_cleanly_and_holds_no_placeholder_shaped_text() {
    let placeholder = Regex::new("⟦C[0-9]+⟧").expect("the placeholder pattern compiles");
    let languages: BTreeSet<LanguageId> = source_files()
        .iter()
        .map(|(_, language)| *language)
        .collect();
    assert_eq!(
        languages.len(),
        LanguageId::ALL.len(),
        "every adapter id has a fixture file"
    );
    for path in fixture_files() {
        let text = read(&path);
        assert!(
            !placeholder.is_match(&text),
            "{path} contains placeholder-shaped text"
        );
        if let Some(language) = language_for(Path::new(&path)) {
            let tree = munge_comments::lang::parse(&text, language)
                .unwrap_or_else(|err| panic!("parsing {path}: {err}"));
            assert!(
                !tree.has_error && !tree.too_deep,
                "{path} must parse cleanly"
            );
        }
    }
}

/// The slot kind of the comment `entry` labels.
fn slot_kind_of(scanned: &[Scanned], entry: &ManifestEntry) -> SlotKind {
    let comment = scanned
        .iter()
        .find(|comment| comment.matches(entry))
        .unwrap_or_else(|| panic!("{entry:?} matches no comment"));
    scanned_slot_kind(
        &comment.comment.text,
        comment.comment.kind,
        adapter_for(comment.language),
    )
    .unwrap_or_else(|| panic!("{} has no slot kind", comment.describe()))
}

#[test]
fn the_scenario_scripts_every_llm_bound_entry_per_its_label() {
    let manifest = load_manifest();
    let scanned = scanned();
    let scenario = noisy_scenario(&manifest);
    for entry in &manifest.entries {
        let adapter = adapter_for(entry.language());
        let script = scenario.by_key.get(&entry.scenario_key());
        match entry.label {
            Label::Functional | Label::Protected => {
                assert!(
                    script.is_none(),
                    "{entry:?} never reaches the model, so it has no script"
                );
            }
            Label::Superfluous => {
                let regen = script
                    .and_then(|s| s.regen.as_ref())
                    .unwrap_or_else(|| panic!("{entry:?} lacks a regeneration"));
                assert_eq!(regen.difficulty, Difficulty::Easy, "{entry:?}");
                assert!(regen.would_you_comment_here, "{entry:?}");
                if !entry.key.is_empty() {
                    let score = similarity(&entry.key, &regen.comment, adapter);
                    assert!(
                        score >= SIMILARITY_THRESHOLD,
                        "{entry:?}: similarity {score}"
                    );
                }
            }
            Label::Valuable => {
                let script = script.unwrap_or_else(|| panic!("{entry:?} lacks a script"));
                let regen = script.regen.as_ref().expect("a valuable regeneration");
                assert_eq!(regen.difficulty, Difficulty::Difficult, "{entry:?}");
                assert!(regen.missing_context.is_some(), "{entry:?}");
                let judge = script.judge.as_ref().expect("a valuable judge script");
                assert_eq!(judge.decision, JudgeDecision::KeepOriginal, "{entry:?}");
                assert_eq!(judge.confidence, JUDGE_CONFIDENCE, "{entry:?}");
            }
            Label::Rewrite => {
                let rewritten = rewrite_text(entry);
                let slot = slot_kind_of(&scanned, entry);
                let script = script.unwrap_or_else(|| panic!("{entry:?} lacks a script"));
                assert_eq!(
                    script.regen.as_ref().map(|r| r.difficulty),
                    Some(Difficulty::Medium)
                );
                let judge = script.judge.as_ref().expect("a pass-1 judge script");
                assert_eq!(judge.decision, JudgeDecision::KeepGenerated, "{entry:?}");
                assert_eq!(judge.confidence, JUDGE_CONFIDENCE, "{entry:?}");
                assert_eq!(judge.final_comment.as_deref(), Some(rewritten), "{entry:?}");
                let score = similarity(&entry.key, rewritten, adapter);
                assert!(
                    score < TIE_SIMILARITY,
                    "{entry:?}: the pass-1 rewrite is a tie ({score})"
                );
                assert_ne!(match_key(rewritten, adapter), entry.key, "{entry:?}");
                validate_final_comment(rewritten, slot, adapter).unwrap_or_else(|err| {
                    panic!("{entry:?}: the rewrite is not writable: {err:?}")
                });

                let pass2_key = scenario_key(&entry.path, &rewritten_key(entry));
                let pass2 = scenario
                    .by_key
                    .get(&pass2_key)
                    .unwrap_or_else(|| panic!("no pass-2 script at {pass2_key:?}"));
                assert_eq!(
                    pass2.regen.as_ref().map(|r| r.difficulty),
                    Some(Difficulty::Medium)
                );
                let judge = pass2.judge.as_ref().expect("a pass-2 judge script");
                assert_eq!(judge.decision, JudgeDecision::Hybrid, "{entry:?}");
                assert_eq!(judge.confidence, JUDGE_CONFIDENCE, "{entry:?}");
                let merged = judge
                    .final_comment
                    .as_deref()
                    .expect("a HYBRID finalComment");
                assert_eq!(merged, near_identical(rewritten));
                assert_ne!(
                    match_key(merged, adapter),
                    rewritten_key(entry),
                    "{entry:?}"
                );
                let score = similarity(rewritten, merged, adapter);
                assert!(
                    score >= TIE_SIMILARITY,
                    "{entry:?}: the pass-2 merge is no tie ({score})"
                );
                validate_final_comment(merged, slot, adapter)
                    .unwrap_or_else(|err| panic!("{entry:?}: the merge is not writable: {err:?}"));
                assert!(
                    manifest.entry(&entry.path, &rewritten_key(entry)).is_none(),
                    "{entry:?}: the rewritten text collides with another fixture comment"
                );
            }
        }
    }
    let expected = manifest
        .entries
        .iter()
        .filter(|e| e.label.is_prose())
        .count()
        + manifest.with_label(Label::Rewrite).count();
    assert_eq!(
        scenario.by_key.len(),
        expected,
        "one script per prose entry plus one per rewrite"
    );
    assert!(scenario.failures.is_empty());
}

#[test]
fn extra_regenerations_cite_uncommented_declarations() {
    let scanned = scanned();
    let extras = extra_regenerations();
    assert!(
        extras.iter().any(|e| e.difficulty == Difficulty::Difficult),
        "one extra is DIFFICULT"
    );
    assert!(
        extras.iter().any(|e| e.difficulty != Difficulty::Difficult),
        "some extra is insertable"
    );
    for extra in &extras {
        let language = language_for(Path::new(&extra.path)).expect("a source path");
        let adapter = adapter_for(language);
        let text = read(&extra.path);
        let tree = munge_comments::lang::parse(&text, language).expect("the fixture parses");
        let declarations = list_declarations(&tree.root, &text, adapter);
        let found: Vec<_> = declarations
            .iter()
            .filter(|decl| {
                decl.node_kind == extra.anchor.node_kind && decl.name == extra.anchor.name
            })
            .collect();
        assert_eq!(
            found.len(),
            1,
            "{extra:?} must cite exactly one declaration"
        );
        let line_start = text[..found[0].start].rfind('\n').map_or(0, |at| at + 1);
        let line = text[line_start..].lines().next().unwrap_or_default();
        assert_eq!(
            line.trim(),
            extra.anchor.snippet,
            "{extra:?}: the snippet must be the declaration's first line"
        );
        assert!(
            !scanned.iter().any(|c| c.path == extra.path
                && c.comment.anchor.node_kind == extra.anchor.node_kind
                && c.comment.anchor.name == extra.anchor.name),
            "{extra:?} cites a declaration that already has a comment"
        );
        validate_final_comment(&extra.comment, SlotKind::Line, adapter)
            .unwrap_or_else(|err| panic!("{extra:?} is not writable: {err:?}"));
    }
}
