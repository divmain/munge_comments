//! Mutation-property suites for the semantic verifier
//! (`apply::verify`), over every golden fixture of every tier-1
//! language and each pinned seed of `common::PROPTEST_SEEDS`.
//!
//! - **Legal mutations** come from the real edit engine
//!   (`compute_edits`/`apply_edits`) with random per-record decisions and
//!   random single- and multi-line replacement text drawn from a word
//!   alphabet no functional pattern matches. They must satisfy the
//!   Design's oracle: the verifier accepts, or rejects *only* on the
//!   functional-sequence check, and only because comments carrying a
//!   positional tag (`encoding-cookie`, `modeline`, `license`) were
//!   activated or deactivated by moving lines. Any other rejection is a
//!   false rejection.
//! - **Illegal mutations** change one code token, or remove the line
//!   break between two tokens; the verifier must reject every one.
//! - **Python indentation shifts** (dedenting the last statement of a
//!   block) must fail the shape check.
//!
//! The default run uses proptest's default case count (256 per fixture
//! per seed); the acceptance run raises it explicitly:
//! `PROPTEST_CASES=1000 cargo test --test verifier_props`.

mod common;

use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::Path;

use munge_comments::apply::edits::{EditOpts, apply_edits, compute_edits};
use munge_comments::apply::types::SkippedEditReason;
use munge_comments::apply::verify::{
    FunctionalComment, functional_sequence, verify_semantic_equality,
};
use munge_comments::artifact::types::Decision;
use munge_comments::comments::types::Classification;
use munge_comments::lang;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::lang::types::{PlainNode, leaves};
use munge_comments::{
    Error, FileRef, Inventory, InventoryComment, LanguageId, inventory_file, language_for,
};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// The tags of functional comments that a legal edit may activate or
/// deactivate by moving lines: their rules are positional.
const POSITIONAL_TAGS: [&str; 3] = ["encoding-cookie", "modeline", "license"];

/// Replacement-text words: lowercase letters only, so no functional
/// pattern of any adapter (directives need sigils, colons or specific
/// keywords) and no slot rejection can match. Every generated rewrite is
/// therefore valid; a `FinalCommentInvalid` skip fails the suite.
const WORDS: [&str; 16] = [
    "apple", "river", "stone", "cloud", "quiet", "maple", "lantern", "meadow", "pebble", "harbor",
    "violet", "copper", "ember", "willow", "orchid", "falcon",
];

/// A golden fixture, inventoried once.
struct Fixture {
    /// `language/file`, for failure messages.
    key: String,
    /// The file name (its extension selects the language).
    name: String,
    text: String,
    file: FileRef,
    inventory: Inventory,
    /// The ordered functional comments of `text`.
    functional: Vec<FunctionalComment>,
}

impl Fixture {
    fn load(path: &Path) -> Fixture {
        let directory = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let key = format!("{directory}/{name}");
        let text = fs::read_to_string(path).unwrap_or_else(|err| panic!("{key}: {err}"));
        let file = FileRef {
            language: language_for(Path::new(&name)).expect("known extension"),
            file_name: name.clone(),
        };
        let inventory =
            inventory_file(&text, file.clone()).unwrap_or_else(|err| panic!("{key}: {err}"));
        let functional = functional_sequence(&inventory);
        Fixture {
            key,
            name,
            text,
            file,
            inventory,
            functional,
        }
    }

    /// Verifies `edited` against this fixture's text.
    fn verify(&self, edited: &str) -> Result<(), Error> {
        verify_semantic_equality(&self.text, edited, self.file.clone(), &self.functional)
    }
}

/// Every golden fixture of the language directory `directory`, sorted.
fn fixtures(directory: &str) -> Vec<Fixture> {
    let root = common::fixture_path("golden").join(directory);
    let mut paths: Vec<_> = fs::read_dir(&root)
        .unwrap_or_else(|err| panic!("{}: {err}", root.display()))
        .map(|entry| entry.expect("a golden directory is listable").path())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no golden fixtures in {directory}");
    paths.iter().map(|path| Fixture::load(path)).collect()
}

/// Runs `test` over `strategy` once per pinned seed, panicking with the
/// fixture key and seed on the first failure.
fn run_seeds<S: Strategy>(
    key: &str,
    strategy: &S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) {
    for seed in common::PROPTEST_SEEDS {
        let mut runner = TestRunner::new(common::proptest_config_with_seed(seed));
        if let Err(err) = runner.run(strategy, &test) {
            panic!("{key} (seed {seed:#x}): {err}");
        }
    }
}

// ---- Legal mutations ---------------------------------------------------

/// One record's random decision.
#[derive(Debug, Clone)]
enum Choice {
    Keep,
    Remove,
    /// `KEEP_GENERATED` (or `HYBRID`, which renders the same way) with
    /// this `finalComment`.
    Rewrite {
        hybrid: bool,
        text: String,
    },
}

/// Single- and multi-line replacement text over [`WORDS`].
fn replacement_text() -> impl Strategy<Value = String> {
    let line = prop::collection::vec(prop::sample::select(&WORDS[..]), 1..6)
        .prop_map(|words| words.join(" "));
    prop::collection::vec(line, 1..4).prop_map(|lines| lines.join("\n"))
}

fn choice() -> impl Strategy<Value = Choice> {
    prop_oneof![
        Just(Choice::Keep),
        Just(Choice::Remove),
        (any::<bool>(), replacement_text())
            .prop_map(|(hybrid, text)| Choice::Rewrite { hybrid, text }),
    ]
}

/// The decision a legal mutation gives `comment` under `choice`: functional
/// comments are never edited (the firewall); prose and doc-protected
/// comments (human-overridable) take the random choice.
fn decision(comment: &InventoryComment, choice: &Choice) -> (Decision, Option<String>) {
    if comment.classification == Classification::Functional {
        return (Decision::KeepProtected, None);
    }
    match choice {
        Choice::Keep => (Decision::KeepOriginal, None),
        Choice::Remove => (Decision::Remove, None),
        Choice::Rewrite { hybrid, text } => {
            let decision = if *hybrid {
                Decision::Hybrid
            } else {
                Decision::KeepGenerated
            };
            (decision, Some(text.clone()))
        }
    }
}

/// Applies `choices` (one per inventory record, in order) through the
/// edit engine. Only JSX-container skips are expected.
fn mutate(fixture: &Fixture, choices: &[Choice]) -> Result<String, TestCaseError> {
    let index: HashMap<&str, usize> = fixture
        .inventory
        .comments
        .iter()
        .enumerate()
        .map(|(index, comment)| (comment.id.as_str(), index))
        .collect();
    let plan = common::file_plan_from_inventory(
        &fixture.name,
        &fixture.text,
        &fixture.inventory,
        fixture.file.language,
        |comment| decision(comment, &choices[index[comment.id.as_str()]]),
    );
    let computed = compute_edits(
        &fixture.text,
        &plan,
        &fixture.inventory,
        adapter_for(fixture.file.language),
        EditOpts { allow_add: false },
    )
    .map_err(|err| TestCaseError::fail(format!("compute_edits: {err}")))?;
    prop_assert!(
        computed
            .skipped
            .iter()
            .all(|skip| skip.reason == SkippedEditReason::JsxExpression),
        "unexpected skips {:?}",
        computed.skipped
    );
    apply_edits(&fixture.text, &computed.edits)
        .map_err(|err| TestCaseError::fail(format!("apply_edits: {err}")))
}

/// The Design's oracle for a legal mutation `edited` of `fixture`.
fn check_oracle(fixture: &Fixture, edited: &str) -> Result<(), String> {
    let reason = match fixture.verify(edited) {
        Ok(()) => return Ok(()),
        Err(Error::Verify(reason)) => reason,
        Err(other) => return Err(format!("verification errored: {other}")),
    };
    // Check 5 runs last, so its failure means every other check passed.
    if !reason.starts_with("functional-comment mismatch") {
        return Err(format!("false rejection: {reason}"));
    }
    let edited_inventory = inventory_file(edited, fixture.file.clone())
        .map_err(|err| format!("inventorying the edited text: {err}"))?;
    if positional_change_only(&fixture.functional, &functional_sequence(&edited_inventory)) {
        Ok(())
    } else {
        Err(format!(
            "false rejection (not a positional activation): {reason}"
        ))
    }
}

/// Whether the functional sequences `original` and `edited` differ solely
/// by positional-tag comments that were activated or deactivated:
///
/// - set aside every comment carrying a positional tag; the remaining
///   ordered sequences (texts and tags) are equal;
/// - the set-aside comments differ: their symmetric difference is
///   non-empty;
/// - each set-aside comment that differs only gained or lost positional
///   roles: either its non-positional projection (text, other tags)
///   appears on both sides, or its text is absent from the other side's
///   set-aside comments (it entered or left the functional list).
fn positional_change_only(original: &[FunctionalComment], edited: &[FunctionalComment]) -> bool {
    let (original_other, original_positional) = split_positional(original);
    let (edited_other, edited_positional) = split_positional(edited);
    if original_other != edited_other || original_positional == edited_positional {
        return false;
    }
    let projections = |comments: &[&FunctionalComment]| -> Vec<(String, Vec<String>)> {
        let mut projections: Vec<_> = comments
            .iter()
            .map(|comment| {
                let tags = comment
                    .tags
                    .iter()
                    .filter(|tag| !POSITIONAL_TAGS.contains(&tag.as_str()))
                    .cloned()
                    .collect();
                (comment.text.clone(), tags)
            })
            .collect();
        projections.sort();
        projections
    };
    let (original_projections, edited_projections) = (
        projections(&original_positional),
        projections(&edited_positional),
    );
    let only_role_changes = |from: &[(String, Vec<String>)], to: &[(String, Vec<String>)]| {
        let mut unmatched = to.to_vec();
        from.iter().all(|projection| {
            if let Some(at) = unmatched.iter().position(|other| other == projection) {
                unmatched.remove(at);
                true
            } else {
                !to.iter().any(|(text, _)| *text == projection.0)
            }
        })
    };
    only_role_changes(&original_projections, &edited_projections)
        && only_role_changes(&edited_projections, &original_projections)
}

/// `comments` split into those without a positional tag (in order) and
/// those with one (sorted by text, then tags: a multiset).
fn split_positional(
    comments: &[FunctionalComment],
) -> (Vec<&FunctionalComment>, Vec<&FunctionalComment>) {
    let (mut positional, other): (Vec<_>, Vec<_>) = comments.iter().partition(|comment| {
        POSITIONAL_TAGS
            .iter()
            .any(|tag| comment.tags.contains(*tag))
    });
    positional.sort_by(|a, b| (&a.text, &a.tags).cmp(&(&b.text, &b.tags)));
    (other, positional)
}

/// The rejection reason prefix pinned for editing `comment` of `fixture`
/// (`common::EXPECTED_VERIFIER_REJECTIONS`), if any.
fn pinned_rejection(fixture: &Fixture, comment: &InventoryComment) -> Option<&'static str> {
    common::EXPECTED_VERIFIER_REJECTIONS
        .iter()
        .find(|(key, text, _)| *key == fixture.key && *text == comment.text)
        .map(|(_, _, reason)| *reason)
}

/// One legal-mutation case. A mutation that edits a record with a pinned
/// (known, grammar-induced) false rejection must be rejected for exactly
/// that reason — never accepted — and the same mutation with those
/// records kept must still satisfy the oracle, so a pinned record never
/// masks another false rejection.
fn legal_case(fixture: &Fixture, choices: &[Choice]) -> Result<(), TestCaseError> {
    let fail = |reason: String, edited: &str| {
        TestCaseError::fail(format!("{reason}\n--- edited ---\n{edited}"))
    };
    let mut kept = choices.to_vec();
    let mut pinned = None;
    for (comment, choice) in fixture.inventory.comments.iter().zip(&mut kept) {
        if let Some(reason) = pinned_rejection(fixture, comment)
            && !matches!(choice, Choice::Keep)
        {
            pinned = Some(reason);
            *choice = Choice::Keep;
        }
    }
    if let Some(prefix) = pinned {
        let edited = mutate(fixture, choices)?;
        match fixture.verify(&edited) {
            Err(Error::Verify(reason)) if reason.starts_with(prefix) => {}
            other => {
                return Err(fail(
                    format!("expected the pinned rejection {prefix:?}, got {other:?}"),
                    &edited,
                ));
            }
        }
    }
    let edited = mutate(fixture, &kept)?;
    check_oracle(fixture, &edited).map_err(|reason| fail(reason, &edited))
}

/// The legal-mutation suite over one language directory.
fn legal_mutations(directory: &str) {
    for fixture in fixtures(directory) {
        let editable = fixture
            .inventory
            .comments
            .iter()
            .any(|comment| comment.classification != Classification::Functional);
        if !editable {
            continue;
        }
        let strategy = prop::collection::vec(choice(), fixture.inventory.comments.len());
        run_seeds(&fixture.key, &strategy, |choices| {
            legal_case(&fixture, &choices)
        });
    }
}

/// One `#[test]` per tier-1 language directory and suite, so the suites
/// run in parallel.
macro_rules! per_language {
    ($suite:ident: $($test:ident => $directory:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test() {
                $suite($directory);
            }
        )+
    };
}

per_language!(legal_mutations:
    legal_mutations_c => "c",
    legal_mutations_cpp => "cpp",
    legal_mutations_go => "go",
    legal_mutations_java => "java",
    legal_mutations_javascript => "javascript",
    legal_mutations_python => "python",
    legal_mutations_rust => "rust",
    legal_mutations_tsx => "tsx",
    legal_mutations_typescript => "typescript",
);

// ---- Named oracle cases ------------------------------------------------

/// A fixture built from inline `text`.
fn inline_fixture(name: &str, text: &str) -> Fixture {
    let file = FileRef {
        language: language_for(Path::new(name)).expect("known extension"),
        file_name: name.to_owned(),
    };
    let inventory = inventory_file(text, file.clone()).expect("inventory");
    let functional = functional_sequence(&inventory);
    Fixture {
        key: name.to_owned(),
        name: name.to_owned(),
        text: text.to_owned(),
        file,
        inventory,
        functional,
    }
}

/// Asserts that `edited` is rejected on the functional-sequence check and
/// that the oracle accepts the rejection, with `cookie` functional in
/// exactly one of the two texts.
fn assert_positional_rejection(fixture: &Fixture, edited: &str, cookie: &str, activated: bool) {
    let reason = match fixture.verify(edited) {
        Err(Error::Verify(reason)) => reason,
        other => panic!("expected a rejection, got {other:?}\n{edited}"),
    };
    assert!(
        reason.starts_with("functional-comment mismatch"),
        "{reason}"
    );
    check_oracle(fixture, edited).expect("the oracle accepts a positional (de)activation");
    let edited_functional =
        functional_sequence(&inventory_file(edited, fixture.file.clone()).unwrap());
    let is_cookie = |comment: &FunctionalComment| {
        comment.text == cookie && comment.tags.contains("encoding-cookie")
    };
    assert_eq!(fixture.functional.iter().any(is_cookie), !activated);
    assert_eq!(edited_functional.iter().any(is_cookie), activated);
}

#[test]
fn a_multi_line_rewrite_pushing_a_line_2_cookie_to_line_3_is_a_deactivation() {
    let cookie = "# -*- coding: utf-8 -*-";
    let fixture = inline_fixture("a.py", &format!("# A note.\n{cookie}\nVALUE = 1\n"));
    let mut choices = vec![Choice::Keep; fixture.inventory.comments.len()];
    choices[0] = Choice::Rewrite {
        hybrid: false,
        text: "apple river\nstone cloud".to_owned(),
    };
    let edited = mutate(&fixture, &choices).unwrap();
    assert_eq!(
        edited,
        format!("# apple river\n# stone cloud\n{cookie}\nVALUE = 1\n")
    );
    assert_positional_rejection(&fixture, &edited, cookie, false);
}

#[test]
fn a_removal_pulling_a_line_3_cookie_onto_line_2_is_an_activation() {
    let cookie = "# -*- coding: utf-8 -*-";
    let fixture = inline_fixture("a.py", &format!("# A note.\n\n{cookie}\nVALUE = 1\n"));
    let mut choices = vec![Choice::Keep; fixture.inventory.comments.len()];
    choices[0] = Choice::Remove;
    let edited = mutate(&fixture, &choices).unwrap();
    assert_eq!(edited, format!("\n{cookie}\nVALUE = 1\n"));
    assert_positional_rejection(&fixture, &edited, cookie, true);
}

#[test]
fn the_oracle_refuses_a_non_positional_functional_change() {
    // Guards the oracle itself: losing a directive is never explained by
    // a positional (de)activation.
    let fixture = inline_fixture("a.py", "x = f()  # type: ignore\n");
    let edited = "x = f()\n";
    let reason = check_oracle(&fixture, edited).expect_err("a lost directive is a real change");
    assert!(reason.contains("not a positional activation"), "{reason}");
}

// ---- Illegal mutations -------------------------------------------------

/// The code tokens of `text` as the verifier walks them: the leaves of
/// its tree, comment subtrees and hidden (`_`-prefixed named) tokens
/// skipped, as `(kind, byte range)` in document order.
fn tokens_of(text: &str, language: LanguageId) -> Vec<(&'static str, Range<usize>)> {
    let tree = lang::parse(text, language).expect("parse");
    assert!(!tree.too_deep, "too deep");
    let adapter = adapter_for(language);
    let skip = |node: &PlainNode| {
        adapter.comment_node_kinds.contains(&node.kind)
            || (node.named && node.kind.starts_with('_'))
    };
    if tree.root.children.is_empty() {
        return Vec::new();
    }
    leaves(&tree.root, skip)
        .map(|leaf| (leaf.kind, leaf.start..leaf.end))
        .collect()
}

/// The code token ranges of `fixture` (see [`tokens_of`]).
fn code_tokens(fixture: &Fixture) -> Vec<Range<usize>> {
    tokens_of(&fixture.text, fixture.file.language)
        .into_iter()
        .map(|(_, range)| range)
        .collect()
}

/// Whether `character` is a line terminator, as line hygiene and the
/// verifier define it.
fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Whether `range` overlaps a comment record of `fixture`.
fn overlaps_comment(fixture: &Fixture, range: &Range<usize>) -> bool {
    fixture
        .inventory
        .comments
        .iter()
        .any(|comment| comment.start < range.end && range.start < comment.end)
}

/// A token-change site: a code token's range minus its edge whitespace,
/// non-empty and clear of every comment.
fn token_change_sites(fixture: &Fixture) -> Vec<Range<usize>> {
    code_tokens(fixture)
        .into_iter()
        .filter_map(|leaf| {
            let text = &fixture.text[leaf.clone()];
            let start = leaf.start + (text.len() - text.trim_start().len());
            let end = leaf.start + text.trim_end().len();
            (start < end).then_some(start..end)
        })
        .filter(|core| !overlaps_comment(fixture, core))
        .collect()
}

/// Characters a token change writes: never whitespace, never part of a
/// comment opener, so the change always lands in code.
const CODE_CHARS: [char; 6] = ['a', 'q', 'z', '0', '7', 'Q'];

/// One token change: insert a code character at a character boundary of
/// a site, or replace one of its characters with a different one.
#[derive(Debug, Clone)]
struct TokenChange {
    site: prop::sample::Index,
    position: prop::sample::Index,
    character: char,
    insert: bool,
}

fn token_change() -> impl Strategy<Value = TokenChange> {
    (
        any::<prop::sample::Index>(),
        any::<prop::sample::Index>(),
        prop::sample::select(&CODE_CHARS[..]),
        any::<bool>(),
    )
        .prop_map(|(site, position, character, insert)| TokenChange {
            site,
            position,
            character,
            insert,
        })
}

/// `fixture`'s text with `change` applied to one of `sites`.
fn apply_token_change(fixture: &Fixture, sites: &[Range<usize>], change: &TokenChange) -> String {
    let site = &sites[change.site.index(sites.len())];
    let text = &fixture.text;
    if change.insert {
        // Boundaries from the site's start to its end, both included.
        let boundaries: Vec<usize> = (site.start..=site.end)
            .filter(|offset| text.is_char_boundary(*offset))
            .collect();
        let at = boundaries[change.position.index(boundaries.len())];
        return format!("{}{}{}", &text[..at], change.character, &text[at..]);
    }
    let characters: Vec<(usize, char)> = text[site.clone()]
        .char_indices()
        .map(|(offset, character)| (site.start + offset, character))
        .filter(|(_, character)| !character.is_whitespace())
        .collect();
    let (at, old) = characters[change.position.index(characters.len())];
    // A different character than the one replaced.
    let new = if old == change.character {
        CODE_CHARS[(CODE_CHARS.iter().position(|c| *c == old).unwrap() + 1) % CODE_CHARS.len()]
    } else {
        change.character
    };
    format!("{}{new}{}", &text[..at], &text[at + old.len_utf8()..])
}

/// A newline-removal site: an interior boundary between consecutive code
/// tokens whose own text (comment interiors included) holds a line
/// terminator, while neither neighbor carries one at its facing edge.
fn newline_sites(fixture: &Fixture) -> Vec<Range<usize>> {
    let text = &fixture.text;
    code_tokens(fixture)
        .windows(2)
        .filter_map(|pair| {
            let (before, after) = (&text[pair[0].clone()], &text[pair[1].clone()]);
            let trailing = &before[before.trim_end().len()..];
            let leading = &after[..after.len() - after.trim_start().len()];
            let gap = pair[0].end..pair[1].start;
            (text[gap.clone()].contains(is_line_terminator)
                && !trailing.contains(is_line_terminator)
                && !leading.contains(is_line_terminator))
            .then_some(gap)
        })
        .collect()
}

/// `fixture`'s text with every line terminator of `gap` (`\r\n` as one)
/// replaced by a space.
fn remove_newlines(fixture: &Fixture, gap: &Range<usize>) -> String {
    let text = &fixture.text;
    let joined = text[gap.clone()]
        .replace("\r\n", " ")
        .replace(is_line_terminator, " ");
    format!("{}{joined}{}", &text[..gap.start], &text[gap.end..])
}

/// Requires the verifier to reject `edited`.
fn require_rejection(fixture: &Fixture, edited: &str) -> Result<(), TestCaseError> {
    match fixture.verify(edited) {
        Err(Error::Verify(_)) => Ok(()),
        other => Err(TestCaseError::fail(format!(
            "an illegal mutation was not rejected: {other:?}\n--- edited ---\n{edited}"
        ))),
    }
}

/// The illegal-mutation suites over one language directory.
fn illegal_mutations(directory: &str) {
    let (mut token_sites, mut gap_sites) = (0, 0);
    for fixture in fixtures(directory) {
        let sites = token_change_sites(&fixture);
        token_sites += sites.len();
        if !sites.is_empty() {
            run_seeds(&fixture.key, &token_change(), |change| {
                require_rejection(&fixture, &apply_token_change(&fixture, &sites, &change))
            });
        }
        let gaps = newline_sites(&fixture);
        gap_sites += gaps.len();
        if !gaps.is_empty() {
            run_seeds(&fixture.key, &any::<prop::sample::Index>(), |index| {
                let gap = &gaps[index.index(gaps.len())];
                require_rejection(&fixture, &remove_newlines(&fixture, gap))
            });
        }
    }
    assert!(
        token_sites > 0 && gap_sites > 0,
        "{directory}: no mutation sites"
    );
}

per_language!(illegal_mutations:
    illegal_mutations_c => "c",
    illegal_mutations_cpp => "cpp",
    illegal_mutations_go => "go",
    illegal_mutations_java => "java",
    illegal_mutations_javascript => "javascript",
    illegal_mutations_python => "python",
    illegal_mutations_rust => "rust",
    illegal_mutations_tsx => "tsx",
    illegal_mutations_typescript => "typescript",
);

// ---- Python indentation shifts -----------------------------------------

/// The leading horizontal whitespace of the line holding `offset`, as a
/// range, when only such whitespace precedes `offset` on that line.
fn indentation(text: &str, offset: usize) -> Option<Range<usize>> {
    let line_start = text[..offset]
        .rfind(is_line_terminator)
        .map_or(0, |at| at + text[at..].chars().next().unwrap().len_utf8());
    let indent_end = line_start
        + text[line_start..]
            .find(|character| !matches!(character, ' ' | '\t'))
            .unwrap_or(text.len() - line_start);
    (indent_end == offset).then_some(line_start..indent_end)
}

/// Every dedent of `fixture`: a statement that starts its line inside a
/// `block` moved to the indentation of the block's parent clause.
fn dedents(fixture: &Fixture) -> Vec<String> {
    let text = &fixture.text;
    let tree = lang::parse(text, fixture.file.language).expect("parse");
    let adapter = adapter_for(fixture.file.language);
    let mut edits = Vec::new();
    let mut stack = vec![&tree.root];
    while let Some(node) = stack.pop() {
        stack.extend(node.children.iter());
        for block in node.children.iter().filter(|child| child.kind == "block") {
            let Some(target) = indentation(text, node.start) else {
                continue;
            };
            let statements = block
                .children
                .iter()
                .filter(|child| child.named && !adapter.comment_node_kinds.contains(&child.kind));
            for statement in statements {
                let Some(current) = indentation(text, statement.start) else {
                    continue;
                };
                if current.len() > target.len() {
                    edits.push(format!(
                        "{}{}{}",
                        &text[..current.start],
                        &text[target.clone()],
                        &text[current.end..]
                    ));
                }
            }
        }
    }
    edits
}

#[test]
fn python_indentation_shifts_fail_the_shape_check() {
    let language = LanguageId::Python;
    let token_texts = |text: &str| -> Vec<(&'static str, String)> {
        tokens_of(text, language)
            .into_iter()
            .map(|(kind, range)| (kind, text[range].to_owned()))
            .collect()
    };
    let mut shape_failures = 0;
    for fixture in fixtures("python") {
        let original_tokens = token_texts(&fixture.text);
        for edited in dedents(&fixture) {
            let reason = match fixture.verify(&edited) {
                Err(Error::Verify(reason)) => reason,
                other => panic!(
                    "{}: a dedent was not rejected: {other:?}\n--- edited ---\n{edited}",
                    fixture.key
                ),
            };
            // A dedent changes whitespace inside one line-broken gap. When
            // the result still parses cleanly to the same tokens, the token
            // and gap checks cannot see it: the shape check must.
            let tree = lang::parse(&edited, language).unwrap();
            if !tree.has_error && token_texts(&edited) == original_tokens {
                assert!(
                    reason.starts_with("tree-shape mismatch"),
                    "{}: {reason}\n--- edited ---\n{edited}",
                    fixture.key
                );
                shape_failures += 1;
            }
        }
    }
    assert!(
        shape_failures > 0,
        "no token-preserving dedent in the corpus"
    );
}

#[test]
fn the_oracle_accepts_a_positional_role_gained_by_a_functional_comment() {
    // The vim modeline is functional either way; moved onto line 1 it also
    // becomes the PEP 263 declaration. The verifier rejects (see
    // tests/verifier.rs); the oracle explains it as an activation.
    let fixture = inline_fixture(
        "a.py",
        "# A note.\n# Another note.\n# vim: set fileencoding=utf-8 :\nVALUE = 1\n",
    );
    let mut choices = vec![Choice::Keep; fixture.inventory.comments.len()];
    choices[0] = Choice::Remove;
    let edited = mutate(&fixture, &choices).unwrap();
    assert_positional_rejection(&fixture, &edited, "# vim: set fileencoding=utf-8 :", true);
}
