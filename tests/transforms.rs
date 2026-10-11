//! The hiding transforms over the golden corpus.
//!
//! Redact mode: every golden fixture's blind text is snapshotted (JSON, so
//! `\r` stays visible — insta's text snapshots normalize CRLF); the
//! id → range map round-trips exactly, on every fixture and under proptest
//! over random target subsets and chunk ranges; CRLF survives byte-exactly
//! outside replaced ranges; and the collision fixture takes the fallback.
//!
//! Remove mode: on every fixture and under proptest, the offset map
//! partitions the clean text and the original slice, exactly the selected
//! records are removed, the code is unchanged, and every clean offset maps
//! inside its expected original segment; every fixture keeps its token
//! stream and line-break boundaries; the inline-block and ASI-hazard
//! fixtures produce their exact clean texts; and CRLF and the BOM survive.
//!
//! Mode parity (golden corpus plus `tests/fixtures/parity/`): both modes
//! hide the identical editable set; re-scanning the blind text yields
//! exactly the placeholders plus the non-editable records, and re-scanning
//! the clean text exactly the non-editable records (positional tags
//! compared by raw text only); per-range runs over any comment-safe
//! partition cover the same comments as one whole-file run, with each
//! per-range clean text the matching slice of the whole-file clean text;
//! and the edge cases — no-prose files, explicit and empty target subsets,
//! functional-only files — are exact no-ops or hide exactly the subset.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use munge_comments::comments::redact::{
    RedactOutput, RemoveOutput, Segment, TransformOpts, has_placeholder_collision,
    map_clean_offset, redact, remove_comments,
};
use munge_comments::comments::types::{Classification, CommentKind};
use munge_comments::lang::adapter::LanguageAdapter;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::lang::registry::parse;
use munge_comments::lang::types::{PlainNode, leaves};
use munge_comments::{
    FileRef, Inventory, InventoryComment, LanguageId, inventory_file, language_for,
};
use proptest::prelude::*;
use proptest::sample::Index;
use regex::Regex;

/// One inventoried fixture.
struct Fixture {
    /// Path relative to `tests/fixtures/`, `/`-separated.
    key: String,
    language: LanguageId,
    text: String,
    inventory: Inventory,
}

impl Fixture {
    fn load(path: &Path) -> Self {
        let bytes =
            fs::read(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        let text = String::from_utf8(bytes)
            .unwrap_or_else(|err| panic!("fixture {} is not UTF-8: {err}", path.display()));
        let language = language_for(path)
            .unwrap_or_else(|| panic!("{} has no source-file extension", path.display()));
        let file_name = path
            .file_name()
            .expect("a fixture has a file name")
            .to_string_lossy()
            .into_owned();
        let inventory = inventory_file(
            &text,
            FileRef {
                language,
                file_name,
            },
        )
        .unwrap_or_else(|err| panic!("inventorying {}: {err}", path.display()));
        let key = path
            .strip_prefix(common::fixture_path(""))
            .unwrap_or(path)
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        Self {
            key,
            language,
            text,
            inventory,
        }
    }

    fn adapter(&self) -> &'static LanguageAdapter {
        adapter_for(self.language)
    }

    fn redact(&self, opts: TransformOpts<'_>) -> RedactOutput {
        redact(&self.text, &self.inventory, self.adapter(), opts)
    }
}

/// Every golden fixture file, sorted.
fn golden_paths() -> Vec<PathBuf> {
    let root = common::fixture_path("golden");
    let mut paths = Vec::new();
    for directory in fs::read_dir(&root).expect("tests/fixtures/golden is readable") {
        let directory = directory.expect("golden/ is listable").path();
        for file in fs::read_dir(&directory).expect("a golden directory is readable") {
            paths.push(file.expect("a golden directory is listable").path());
        }
    }
    paths.sort();
    paths
}

/// Every golden fixture, inventoried once per test binary.
static GOLDEN: LazyLock<Vec<Fixture>> = LazyLock::new(|| {
    golden_paths()
        .iter()
        .map(|path| Fixture::load(path))
        .collect()
});

/// One placeholder found in a blind text.
struct Placeholder {
    /// Its byte range in the blind text.
    span: Range<usize>,
    id: String,
    /// The opening delimiter (line prefix or block opener).
    open: String,
    /// The line break kept before a block closer (`""` when none).
    line_break: String,
    /// The block closer (`""` for a line placeholder).
    close: String,
}

/// Every placeholder in `blind_text`, in order, for `adapter`'s language:
/// an opening delimiter (line prefix or block opener), the bracketed id,
/// and — for a block placeholder — an optional kept line break and the
/// block closer. Golden fixtures contain no placeholder-shaped text, so
/// every match is a placeholder.
fn placeholders(adapter: &LanguageAdapter, blind_text: &str) -> Vec<Placeholder> {
    let mut openers: Vec<String> = adapter
        .line_prefixes
        .iter()
        .map(|prefix| regex::escape(prefix))
        .collect();
    let mut closer = String::new();
    if let Some(block) = adapter.block_delimiters {
        openers.push(regex::escape(block.open));
        closer = format!(
            "(?:(?P<break>\\r\\n|[\\n\\r\\u{{2028}}\\u{{2029}}])?(?P<close>{}))?",
            regex::escape(block.close)
        );
    }
    let pattern = Regex::new(&format!(
        "(?P<open>{})⟦(?P<id>C[0-9]+)⟧{closer}",
        openers.join("|")
    ))
    .expect("the placeholder pattern compiles");
    let group = |captures: &regex::Captures<'_>, name: &str| {
        captures
            .name(name)
            .map_or_else(String::new, |found| found.as_str().to_string())
    };
    pattern
        .captures_iter(blind_text)
        .map(|captures| Placeholder {
            span: captures.get(0).expect("group 0 always matches").range(),
            id: group(&captures, "id"),
            open: group(&captures, "open"),
            line_break: group(&captures, "break"),
            close: group(&captures, "close"),
        })
        .collect()
}

/// The placeholder delimiters the Design prescribes for `kind`: the line
/// prefix for line records, the block delimiters for block and doc records.
fn expected_delimiters(adapter: &LanguageAdapter, kind: CommentKind) -> (String, String) {
    match (kind, adapter.block_delimiters) {
        (CommentKind::Block | CommentKind::Doc, Some(block)) => {
            (block.open.to_string(), block.close.to_string())
        }
        _ => (adapter.line_prefixes[0].to_string(), String::new()),
    }
}

/// The line break a block placeholder for the record at `range` must keep:
/// the record's first line break as written, when the record spans lines
/// with non-whitespace text both before it on its first line and after it
/// on its last (collapsing it would join two lines of code); else `""`.
/// Line terminators are `\r\n`, `\n`, `\r`, U+2028 and U+2029.
fn expected_line_break(text: &str, range: &Range<usize>) -> String {
    let terminator = |c: char| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    let raw = &text[range.clone()];
    let Some(at) = raw.find(terminator) else {
        return String::new();
    };
    let before = text[..range.start]
        .rsplit(terminator)
        .next()
        .unwrap_or("")
        .trim_start_matches('\u{FEFF}');
    let after = text[range.end..].split(terminator).next().unwrap_or("");
    if before.trim().is_empty() || after.trim().is_empty() {
        return String::new();
    }
    let rest = &raw[at..];
    if rest.starts_with("\r\n") {
        "\r\n".to_string()
    } else {
        rest.chars().next().map(String::from).unwrap_or_default()
    }
}

/// The ids `redact` must hide under `targets` within `range`: editable,
/// selected, and entirely inside the range.
fn expected_ids(
    fixture: &Fixture,
    targets: Option<&BTreeSet<String>>,
    range: &Range<usize>,
) -> BTreeSet<String> {
    fixture
        .inventory
        .comments
        .iter()
        .filter(|comment| comment.editable)
        .filter(|comment| targets.is_none_or(|targets| targets.contains(&comment.id)))
        .filter(|comment| range.start <= comment.start && comment.end <= range.end)
        .map(|comment| comment.id.clone())
        .collect()
}

/// Checks one redaction of `fixture` over `range` and returns a
/// description of the first violation:
///
/// - `id_map` hides exactly `expected` ids, each mapped to its record's
///   original range;
/// - every placeholder in the blind text uses the delimiters of its
///   record's kind and appears once;
/// - replacing every placeholder with the original text of its mapped
///   range reproduces `text[range]` byte for byte.
fn round_trip_violation(
    fixture: &Fixture,
    output: &RedactOutput,
    range: &Range<usize>,
    expected: &BTreeSet<String>,
) -> Option<String> {
    let key = &fixture.key;
    if output.fallback {
        return Some(format!("{key}: unexpected collision fallback"));
    }
    let mapped: BTreeSet<String> = output.id_map.keys().cloned().collect();
    if &mapped != expected {
        return Some(format!("{key}: hid {mapped:?}, expected {expected:?}"));
    }
    let records: BTreeMap<&str, _> = fixture
        .inventory
        .comments
        .iter()
        .map(|comment| (comment.id.as_str(), comment))
        .collect();
    for (id, mapped_range) in &output.id_map {
        let record = records[id.as_str()];
        if *mapped_range != (record.start..record.end) {
            return Some(format!(
                "{key}: {id} maps to {mapped_range:?}, record is {}..{}",
                record.start, record.end
            ));
        }
    }
    let mut restored = String::new();
    let mut cursor = 0;
    let mut seen = BTreeSet::new();
    for placeholder in placeholders(fixture.adapter(), &output.blind_text) {
        let id = placeholder.id.as_str();
        let Some(original) = output.id_map.get(id) else {
            return Some(format!("{key}: placeholder for unmapped id {id}"));
        };
        if !seen.insert(id.to_string()) {
            return Some(format!("{key}: placeholder {id} appears twice"));
        }
        let delimiters = (placeholder.open.clone(), placeholder.close.clone());
        let expected = expected_delimiters(fixture.adapter(), records[id].kind);
        if delimiters != expected {
            return Some(format!(
                "{key}: {id} placeholder delimiters {delimiters:?}, expected {expected:?}"
            ));
        }
        let expected_break = if placeholder.close.is_empty() {
            String::new()
        } else {
            expected_line_break(&fixture.text, original)
        };
        if placeholder.line_break != expected_break {
            return Some(format!(
                "{key}: {id} keeps line break {:?}, expected {expected_break:?}",
                placeholder.line_break
            ));
        }
        restored.push_str(&output.blind_text[cursor..placeholder.span.start]);
        restored.push_str(&fixture.text[original.clone()]);
        cursor = placeholder.span.end;
    }
    restored.push_str(&output.blind_text[cursor..]);
    if seen != mapped {
        return Some(format!("{key}: placeholders {seen:?}, mapped {mapped:?}"));
    }
    if restored != fixture.text[range.clone()] {
        return Some(format!(
            "{key}: restoring the placeholders does not reproduce the original"
        ));
    }
    None
}

#[test]
fn golden_blind_texts_match_their_snapshots() {
    insta::glob!("fixtures/golden/**/*", |path| {
        if path.is_dir() {
            return;
        }
        let fixture = Fixture::load(path);
        let output = fixture.redact(TransformOpts::default());
        assert!(!output.fallback, "{}: unexpected fallback", fixture.key);
        insta::assert_json_snapshot!(output.blind_text);
    });
}

#[test]
fn golden_id_maps_round_trip_on_every_fixture() {
    let whole_file = |fixture: &Fixture| 0..fixture.text.len();
    let violations: Vec<String> = GOLDEN
        .iter()
        .filter_map(|fixture| {
            let range = whole_file(fixture);
            let output = fixture.redact(TransformOpts::default());
            round_trip_violation(
                fixture,
                &output,
                &range,
                &expected_ids(fixture, None, &range),
            )
        })
        .collect();
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn the_corpus_exercises_every_placeholder_kind() {
    let mut line = false;
    let mut block = false;
    let mut doc = false;
    let mut group = false;
    for comment in GOLDEN
        .iter()
        .flat_map(|fixture| &fixture.inventory.comments)
        .filter(|comment| comment.editable)
    {
        match comment.kind {
            CommentKind::Line if comment.members.len() > 1 => group = true,
            CommentKind::Line => line = true,
            CommentKind::Block => block = true,
            CommentKind::Doc => doc = true,
        }
    }
    assert!(
        line && block && doc && group,
        "line {line}, block {block}, doc {doc}, group {group}"
    );
}

#[test]
fn crlf_survives_outside_replaced_ranges() {
    let crlf_fixtures: Vec<&Fixture> = GOLDEN
        .iter()
        .filter(|fixture| fixture.text.contains("\r\n"))
        .collect();
    assert!(
        crlf_fixtures
            .iter()
            .any(|fixture| fixture.key.ends_with("crlf.py"))
            && crlf_fixtures
                .iter()
                .any(|fixture| fixture.key.ends_with("crlf.ts")),
        "the CRLF fixtures are present"
    );
    for fixture in crlf_fixtures {
        let output = fixture.redact(TransformOpts::default());
        let blind = output.blind_text.as_bytes();
        // Pair each placeholder with the original range it replaced: (original range, placeholder span) pairs, both ascending.
        let found = placeholders(fixture.adapter(), &output.blind_text);
        let replaced: Vec<(Range<usize>, Range<usize>)> = found
            .iter()
            .map(|placeholder| {
                (
                    output.id_map[&placeholder.id].clone(),
                    placeholder.span.clone(),
                )
            })
            .collect();
        let mut survivors = 0;
        for (offset, _) in fixture.text.match_indices("\r\n") {
            if replaced
                .iter()
                .any(|(original, _)| original.start <= offset && offset + 2 <= original.end)
            {
                continue;
            }
            // Shift the original offset into the blind text past every replacement before this CRLF.
            let blind_offset = replaced
                .iter()
                .filter(|(original, _)| original.end <= offset)
                .fold(offset, |at, (original, span)| {
                    at - original.len() + span.len()
                });
            assert_eq!(
                &blind[blind_offset..blind_offset + 2],
                b"\r\n",
                "{}: the CRLF at original offset {offset} did not survive",
                fixture.key
            );
            survivors += 1;
        }
        // CRLFs inside placeholders are kept as block line breaks, not survivors: every other CRLF of the blind text is a kept placeholder line break, and no CR was split from its LF.
        let kept = found
            .iter()
            .filter(|placeholder| placeholder.line_break == "\r\n")
            .count();
        assert_eq!(
            output.blind_text.matches("\r\n").count(),
            survivors + kept,
            "{}: CRLF count",
            fixture.key
        );
        assert_eq!(
            output.blind_text.matches('\r').count(),
            output.blind_text.matches("\r\n").count(),
            "{}: no CRLF was split",
            fixture.key
        );
    }
}

#[test]
fn the_collision_fixture_falls_back() {
    let fixture = Fixture::load(&common::fixture_path("collision/collision.ts"));
    assert!(has_placeholder_collision(&fixture.text));
    assert!(
        fixture
            .inventory
            .comments
            .iter()
            .any(|comment| comment.editable),
        "the fixture has editable prose, so the fallback is what hides it"
    );
    let output = fixture.redact(TransformOpts::default());
    assert!(output.fallback);
    assert!(output.blind_text.is_empty());
    assert!(output.id_map.is_empty());
    // The probe scans the whole file, whatever the range.
    let first_line = fixture.text.find('\n').expect("multi-line") + 1;
    let ranged = fixture.redact(TransformOpts {
        targets: None,
        range: Some(first_line..first_line),
    });
    assert!(ranged.fallback);
}

#[test]
fn golden_fixtures_never_collide() {
    for fixture in GOLDEN.iter() {
        assert!(!has_placeholder_collision(&fixture.text), "{}", fixture.key);
    }
}

/// Byte offsets of every line start in `text`, plus `text.len()` — the
/// chunk boundaries a range may use.
fn line_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect();
    if boundaries.last() != Some(&text.len()) {
        boundaries.push(text.len());
    }
    boundaries
}

proptest! {
    #![proptest_config(common::proptest_config())]

    /// Redact then map ids back: under any target subset (or the default
    /// all-editable targets) and any line-aligned range, the hidden ids are
    /// exactly the selected editable records inside the range, each maps to
    /// its record's original range, and restoring the placeholders from
    /// those ranges reproduces the original slice exactly.
    #[test]
    fn redact_then_map_ids_back_recovers_every_original_range(
        fixture in any::<Index>(),
        select in proptest::option::of(proptest::collection::vec(any::<bool>(), 1..64)),
        from in any::<Index>(),
        to in any::<Index>(),
    ) {
        let fixture = &GOLDEN[fixture.index(GOLDEN.len())];
        let targets: Option<BTreeSet<String>> = select.map(|select| {
            fixture
                .inventory
                .comments
                .iter()
                .enumerate()
                .filter(|(index, _)| select[index % select.len()])
                .map(|(_, comment)| comment.id.clone())
                .collect()
        });
        let boundaries = line_boundaries(&fixture.text);
        let (a, b) = (from.get(&boundaries), to.get(&boundaries));
        let range = *a.min(b)..*a.max(b);
        let output = fixture.redact(TransformOpts {
            targets: targets.as_ref(),
            range: Some(range.clone()),
        });
        let expected = expected_ids(fixture, targets.as_ref(), &range);
        let violation = round_trip_violation(fixture, &output, &range, &expected);
        prop_assert!(violation.is_none(), "{:?}", violation);
    }
}

// ---- remove mode ----

impl Fixture {
    fn remove(&self, opts: TransformOpts<'_>) -> RemoveOutput {
        remove_comments(&self.text, &self.inventory, opts)
    }
}

/// Checks one removal of `fixture` over `range` (removing the `expected`
/// ids) and returns a description of the first violation:
///
/// - the segments partition the clean text and `text[range]` in order;
///   verbatim segments copy the original byte for byte, and every other
///   segment's clean side is whitespace and shorter than what it replaces;
/// - every member of an expected record lies in removed (non-verbatim)
///   segments, and every other comment survives in verbatim segments;
/// - the code (all non-whitespace outside removed members) is unchanged;
/// - every clean offset maps inside its segment's original side — onto the
///   identical character for verbatim bytes — and the end maps to the end
///   of the range.
fn removal_violation(
    fixture: &Fixture,
    output: &RemoveOutput,
    range: &Range<usize>,
    expected: &BTreeSet<String>,
) -> Option<String> {
    let key = &fixture.key;
    let text = &fixture.text;
    let clean = &output.clean_text;
    let map = &output.offset_map;
    if output.original_base != range.start {
        return Some(format!("{key}: original_base {}", output.original_base));
    }
    // The segments must be contiguous and cover the range in order.
    let (mut clean_at, mut original_at) = (0, range.start);
    for segment in map {
        if segment.clean_start != clean_at || segment.original_start != original_at {
            return Some(format!("{key}: segment {segment:?} is not contiguous"));
        }
        let clean_side = &clean[segment.clean_start..segment.clean_end];
        let original_side = &text[segment.original_start..segment.original_end];
        let faithful = if segment.verbatim {
            clean_side == original_side
        } else {
            clean_side.chars().all(char::is_whitespace) && clean_side.len() < original_side.len()
        };
        if !faithful {
            return Some(format!(
                "{key}: segment {segment:?}: {clean_side:?} for {original_side:?}"
            ));
        }
        (clean_at, original_at) = (segment.clean_end, segment.original_end);
    }
    if map.is_empty() || clean_at != clean.len() || original_at != range.end {
        return Some(format!("{key}: segments end at {clean_at}/{original_at}"));
    }
    // A span is verbatim when one verbatim segment contains it, removed when no verbatim segment overlaps it.
    let verbatim = |span: &Range<usize>| {
        map.iter().any(|segment| {
            segment.verbatim
                && segment.original_start <= span.start
                && span.end <= segment.original_end
        })
    };
    let removed = |span: &Range<usize>| {
        !map.iter().any(|segment| {
            segment.verbatim
                && segment.original_start < span.end
                && span.start < segment.original_end
        })
    };
    let mut removed_members: Vec<Range<usize>> = Vec::new();
    for comment in &fixture.inventory.comments {
        if expected.contains(&comment.id) {
            for member in &comment.members {
                if !removed(member) {
                    return Some(format!("{key}: {} member {member:?} survived", comment.id));
                }
                removed_members.push(member.clone());
            }
        } else {
            let kept = comment.start.max(range.start)..comment.end.min(range.end);
            if !kept.is_empty() && !verbatim(&kept) {
                return Some(format!("{key}: {} was not kept verbatim", comment.id));
            }
        }
    }
    // The code — all non-whitespace outside removed members — must be unchanged.
    let mut code = String::new();
    let mut at = range.start;
    for member in &removed_members {
        code.extend(
            text[at..member.start]
                .chars()
                .filter(|c| !c.is_whitespace()),
        );
        at = member.end;
    }
    code.extend(text[at..range.end].chars().filter(|c| !c.is_whitespace()));
    let clean_code: String = clean.chars().filter(|c| !c.is_whitespace()).collect();
    if code != clean_code {
        return Some(format!("{key}: the code changed"));
    }
    // Every clean offset must map inside its segment's original side.
    for (offset, clean_char) in clean.char_indices() {
        let segment = map
            .iter()
            .find(|segment| segment.clean_start <= offset && offset < segment.clean_end)
            .expect("the segments partition the clean text");
        let original = map_clean_offset(map, offset);
        let inside = segment.original_start <= original && original < segment.original_end;
        if !inside || segment.verbatim && !text[original..].starts_with(clean_char) {
            return Some(format!(
                "{key}: clean offset {offset} maps to {original}, outside {segment:?}"
            ));
        }
    }
    let end = map_clean_offset(map, clean.len());
    if end != range.end {
        return Some(format!(
            "{key}: the clean end maps to {end}, not {}",
            range.end
        ));
    }
    None
}

#[test]
fn golden_offset_maps_round_trip_on_every_fixture() {
    let mut languages = BTreeSet::new();
    for fixture in GOLDEN.iter() {
        let range = 0..fixture.text.len();
        let output = fixture.remove(TransformOpts::default());
        let expected = expected_ids(fixture, None, &range);
        if let Some(violation) = removal_violation(fixture, &output, &range, &expected) {
            panic!("{violation}");
        }
        languages.insert(fixture.language);
    }
    // Every tier-1 language contributes fixtures.
    assert_eq!(languages.len(), LanguageId::ALL.len(), "{languages:?}");
}

/// The non-comment leaf tokens of `text` under `fixture`'s grammar, each
/// with its text and whether the boundary before the next token holds a
/// line terminator (the ASI / semicolon-insertion signal). Comment
/// interiors count toward the boundary, as do line terminators ending the
/// token itself (a shebang token includes its `\n`). Token texts are
/// compared without edge whitespace: some tokens absorb the whitespace
/// before a trailing comment (tree-sitter-c's `preproc_arg` of
/// `#define X 2 /* c */` is `2 `), which trailing-comment removal deletes.
fn tokens(fixture: &Fixture, text: &str) -> Vec<(&'static str, String, bool)> {
    let tree = parse(text, fixture.language)
        .unwrap_or_else(|err| panic!("{}: parsing: {err}", fixture.key));
    assert!(
        !tree.has_error && !tree.too_deep,
        "{}: clean parse",
        fixture.key
    );
    let comment_kinds = fixture.adapter().comment_node_kinds;
    let leaves: Vec<&PlainNode> =
        leaves(&tree.root, |node| comment_kinds.contains(&node.kind)).collect();
    let terminator = |c: char| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    leaves
        .iter()
        .enumerate()
        .map(|(index, leaf)| {
            let token = &text[leaf.start..leaf.end];
            let gap_end = leaves.get(index + 1).map_or(leaf.end, |next| next.start);
            let line_break =
                token.ends_with(terminator) || text[leaf.end..gap_end].contains(terminator);
            (leaf.kind, token.trim().to_string(), line_break)
        })
        .collect()
}

#[test]
fn removal_keeps_every_token_and_line_break_boundary_on_every_fixture() {
    for fixture in GOLDEN.iter() {
        let output = fixture.remove(TransformOpts::default());
        assert_eq!(
            tokens(fixture, &output.clean_text),
            tokens(fixture, &fixture.text),
            "{}",
            fixture.key
        );
    }
}

/// The golden fixture at `key`.
fn golden(key: &str) -> &'static Fixture {
    GOLDEN
        .iter()
        .find(|fixture| fixture.key == key)
        .unwrap_or_else(|| panic!("no golden fixture {key}"))
}

#[test]
fn inline_block_comments_never_merge_tokens() {
    let output = golden("golden/typescript/inline-block.ts").remove(TransformOpts::default());
    assert_eq!(
        output.clean_text,
        "declare const a: number;\ndeclare const b: number;\nexport const sum = a + b;\n\n\
         export function f(x: number): number {\n  return x;\n}\n\nlet y = 1;\nexport { y };\n"
    );
}

#[test]
fn line_break_hazards_keep_their_newline_structure() {
    let output = golden("golden/typescript/asi-hazard.ts").remove(TransformOpts::default());
    assert_eq!(
        output.clean_text,
        "export function lineComment(x: number): number | undefined {\n  return\n    x;\n}\n\n\
         export function blockComment(x: number): number | undefined {\n  return\n  x;\n}\n"
    );
}

#[test]
fn removal_normalizes_no_line_endings_and_keeps_the_bom() {
    let crlf: Vec<&Fixture> = GOLDEN
        .iter()
        .filter(|fixture| fixture.text.contains("\r\n"))
        .collect();
    assert!(crlf.len() >= 2, "the CRLF fixtures are present");
    for fixture in crlf {
        let clean = fixture.remove(TransformOpts::default()).clean_text;
        let bare_lf = |text: &str| text.matches('\n').count() - text.matches("\r\n").count();
        let bare_cr = |text: &str| text.matches('\r').count() - text.matches("\r\n").count();
        assert!(clean.contains("\r\n"), "{}", fixture.key);
        assert_eq!(
            bare_lf(&clean),
            bare_lf(&fixture.text),
            "{}: no LF was added or lost",
            fixture.key
        );
        assert!(
            bare_cr(&clean) <= bare_cr(&fixture.text),
            "{}: no CRLF was split",
            fixture.key
        );
    }
    let bom = golden("golden/typescript/bom.ts");
    assert!(bom.text.starts_with('\u{FEFF}'));
    assert!(
        bom.inventory
            .comments
            .iter()
            .any(|comment| comment.editable)
    );
    let clean = bom.remove(TransformOpts::default()).clean_text;
    assert!(clean.starts_with('\u{FEFF}'), "{clean:?}");
    assert_ne!(clean, bom.text, "the BOM fixture's prose was removed");
}

proptest! {
    #![proptest_config(common::proptest_config())]

    /// Remove, then map every clean offset back: under any target subset
    /// (or the default) and any line-aligned range, the selected editable
    /// records inside the range are removed, everything else survives, and
    /// every clean offset maps inside the expected original segment.
    #[test]
    fn remove_then_map_offsets_back_lands_in_the_expected_segment(
        fixture in any::<Index>(),
        select in proptest::option::of(proptest::collection::vec(any::<bool>(), 1..64)),
        from in any::<Index>(),
        to in any::<Index>(),
    ) {
        let fixture = &GOLDEN[fixture.index(GOLDEN.len())];
        let targets: Option<BTreeSet<String>> = select.map(|select| {
            fixture
                .inventory
                .comments
                .iter()
                .enumerate()
                .filter(|(index, _)| select[index % select.len()])
                .map(|(_, comment)| comment.id.clone())
                .collect()
        });
        let boundaries = line_boundaries(&fixture.text);
        let (a, b) = (from.get(&boundaries), to.get(&boundaries));
        let range = *a.min(b)..*a.max(b);
        let output = fixture.remove(TransformOpts {
            targets: targets.as_ref(),
            range: Some(range.clone()),
        });
        let expected = expected_ids(fixture, targets.as_ref(), &range);
        let violation = removal_violation(fixture, &output, &range, &expected);
        prop_assert!(violation.is_none(), "{:?}", violation);
    }
}

// ---- mode parity ----

/// Tags whose presence depends on line position: collapsing a group into
/// one placeholder, or deleting comment lines, can legitimately switch them
/// on or off in a re-scanned text. Comments carrying one (before or after)
/// are compared by raw text only.
const POSITIONAL_TAGS: [&str; 3] = ["encoding-cookie", "modeline", "license"];

fn positional(comment: &InventoryComment) -> bool {
    POSITIONAL_TAGS
        .iter()
        .any(|tag| comment.tags.contains(*tag))
}

/// Inventories a transformed text of `fixture` under the same file
/// identity.
fn rescan(fixture: &Fixture, text: &str) -> Inventory {
    let file_name = fixture
        .key
        .rsplit('/')
        .next()
        .expect("a key has a file name")
        .to_string();
    inventory_file(
        text,
        FileRef {
            language: fixture.language,
            file_name,
        },
    )
    .unwrap_or_else(|err| panic!("{}: re-scanning: {err}", fixture.key))
}

/// What one original record should re-scan as.
enum Expected<'a> {
    /// A placeholder for this hidden id: an editable prose record whose
    /// whole text is the placeholder.
    Placeholder(&'a str),
    /// The original record, byte-identical, with its classification, tags
    /// and editability (unless a positional tag is involved).
    Kept(&'a InventoryComment),
}

/// Compares a re-scan against the expected records, in order, and returns
/// a description of the first violation.
fn rescan_violation(
    fixture: &Fixture,
    mode: &str,
    rescanned: &Inventory,
    expected: &[Expected<'_>],
) -> Option<String> {
    let key = &fixture.key;
    let found = &rescanned.comments;
    if found.len() != expected.len() {
        let texts: Vec<&str> = found.iter().map(|comment| comment.text.as_str()).collect();
        return Some(format!(
            "{key} ({mode}): re-scan found {} records, expected {}: {texts:?}",
            found.len(),
            expected.len()
        ));
    }
    for (comment, expected) in found.iter().zip(expected) {
        match expected {
            Expected::Placeholder(id) => {
                let whole = placeholders(fixture.adapter(), &comment.text);
                let is_placeholder = whole.len() == 1
                    && whole[0].id == *id
                    && whole[0].span == (0..comment.text.len());
                if !is_placeholder {
                    return Some(format!(
                        "{key} ({mode}): expected the placeholder for {id}, found {:?}",
                        comment.text
                    ));
                }
                if !positional(comment)
                    && !(comment.editable && comment.classification == Classification::Prose)
                {
                    return Some(format!(
                        "{key} ({mode}): placeholder {id} re-scans as {:?} {:?}",
                        comment.classification, comment.tags
                    ));
                }
            }
            Expected::Kept(original) => {
                if comment.text != original.text {
                    return Some(format!(
                        "{key} ({mode}): {} re-scans as {:?}, expected {:?}",
                        original.id, comment.text, original.text
                    ));
                }
                let same = (comment.classification, &comment.tags, comment.editable)
                    == (original.classification, &original.tags, original.editable);
                if !positional(comment) && !positional(original) && !same {
                    return Some(format!(
                        "{key} ({mode}): {} re-scans as {:?} {:?}, was {:?} {:?}",
                        original.id,
                        comment.classification,
                        comment.tags,
                        original.classification,
                        original.tags
                    ));
                }
            }
        }
    }
    None
}

/// The ids, among the records inside the slice a removal covers, whose
/// every member it deleted (lies only in non-verbatim segments) — read off
/// the offset map alone.
fn removed_ids(fixture: &Fixture, output: &RemoveOutput) -> BTreeSet<String> {
    let slice_end = output
        .offset_map
        .last()
        .map_or(output.original_base, |segment| segment.original_end);
    let removed = |member: &Range<usize>| {
        !output.offset_map.iter().any(|segment| {
            segment.verbatim
                && segment.original_start < member.end
                && member.start < segment.original_end
        })
    };
    fixture
        .inventory
        .comments
        .iter()
        .filter(|comment| output.original_base <= comment.start && comment.end <= slice_end)
        .filter(|comment| comment.members.iter().all(removed))
        .map(|comment| comment.id.clone())
        .collect()
}

/// Whole-file mode parity for `fixture` with the default targets: both
/// transforms hide exactly the editable set; re-scanning the blind text
/// yields exactly the placeholders plus the non-editable records, and
/// re-scanning the clean text exactly the non-editable records.
fn parity_violation(fixture: &Fixture) -> Option<String> {
    let key = &fixture.key;
    let whole = 0..fixture.text.len();
    let editable = expected_ids(fixture, None, &whole);
    let blind = fixture.redact(TransformOpts::default());
    let clean = fixture.remove(TransformOpts::default());
    let hidden: BTreeSet<String> = blind.id_map.keys().cloned().collect();
    let removed = removed_ids(fixture, &clean);
    if hidden != editable || removed != editable {
        return Some(format!(
            "{key}: redact hid {hidden:?}, remove removed {removed:?}, editable {editable:?}"
        ));
    }
    let comments = &fixture.inventory.comments;
    let with_placeholders: Vec<Expected<'_>> = comments
        .iter()
        .map(|comment| {
            if hidden.contains(&comment.id) {
                Expected::Placeholder(&comment.id)
            } else {
                Expected::Kept(comment)
            }
        })
        .collect();
    let kept: Vec<Expected<'_>> = comments
        .iter()
        .filter(|comment| !removed.contains(&comment.id))
        .map(Expected::Kept)
        .collect();
    rescan_violation(
        fixture,
        "redact",
        &rescan(fixture, &blind.blind_text),
        &with_placeholders,
    )
    .or_else(|| {
        rescan_violation(
            fixture,
            "remove",
            &rescan(fixture, &clean.clean_text),
            &kept,
        )
    })
}

/// Fixtures written for the parity suite (`tests/fixtures/parity/`): a
/// C++ file mixing prose with kept comments (no golden C++ fixture has
/// both), and a Python file whose kept comment only becomes a PEP 263
/// cookie once the prose group above it collapses or disappears.
static PARITY: LazyLock<Vec<Fixture>> = LazyLock::new(|| {
    let root = common::fixture_path("parity");
    let mut paths: Vec<PathBuf> = fs::read_dir(&root)
        .expect("tests/fixtures/parity is readable")
        .map(|file| file.expect("parity/ is listable").path())
        .collect();
    paths.sort();
    paths.iter().map(|path| Fixture::load(path)).collect()
});

/// The golden corpus followed by the parity fixtures.
fn corpus() -> impl Iterator<Item = &'static Fixture> {
    GOLDEN.iter().chain(PARITY.iter())
}

#[test]
fn both_modes_hide_the_same_set_and_re_scan_to_the_non_editable_set() {
    let violations: Vec<String> = corpus().filter_map(parity_violation).collect();
    assert!(violations.is_empty(), "{violations:#?}");
    // Every tier-1 language has a fixture where parity is non-trivial: some
    // comments hidden and some kept.
    let exercised: BTreeSet<LanguageId> = corpus()
        .filter(|fixture| {
            let comments = &fixture.inventory.comments;
            comments.iter().any(|comment| comment.editable)
                && comments.iter().any(|comment| !comment.editable)
        })
        .map(|fixture| fixture.language)
        .collect();
    assert_eq!(exercised.len(), LanguageId::ALL.len(), "{exercised:?}");
}

#[test]
fn a_positional_tag_switched_on_by_either_transform_is_exempt_but_byte_identical() {
    let fixture = PARITY
        .iter()
        .find(|fixture| fixture.key == "parity/positional-shift.py")
        .expect("the positional-shift fixture");
    let kept = |inventory: &Inventory| -> InventoryComment {
        inventory
            .comments
            .iter()
            .find(|comment| comment.text.starts_with("# coding"))
            .expect("the cookie-shaped comment")
            .clone()
    };
    let original = kept(&fixture.inventory);
    // On line 3 it is no cookie: functional only through its `noqa`.
    assert!(!original.editable && !original.tags.contains("encoding-cookie"));
    let blind = fixture.redact(TransformOpts::default()).blind_text;
    let clean = fixture.remove(TransformOpts::default()).clean_text;
    for transformed in [blind, clean] {
        // Moved to line 2 (redact) or line 1 (remove), it now counts as a PEP 263 cookie.
        let shifted = kept(&rescan(fixture, &transformed));
        assert_eq!(shifted.text, original.text);
        assert!(shifted.tags.contains("encoding-cookie"), "{transformed:?}");
    }
    assert_eq!(parity_violation(fixture), None);
}

/// The line starts of `fixture` that cut no comment record (group spans
/// included) — where chunk boundaries may fall — plus `0` and the end.
fn chunk_boundaries(fixture: &Fixture) -> Vec<usize> {
    line_boundaries(&fixture.text)
        .into_iter()
        .filter(|&at| {
            !fixture
                .inventory
                .comments
                .iter()
                .any(|comment| comment.start < at && at < comment.end)
        })
        .collect()
}

/// Where the original offset `at` falls in a whole-file clean text, read
/// off its offset map: a verbatim byte keeps its relative position; inside
/// a removed span, a replacement belongs to the chunk its start lies in.
fn clean_offset_of(map: &[Segment], at: usize) -> usize {
    let Some(segment) = map
        .iter()
        .find(|segment| segment.original_start <= at && at < segment.original_end)
    else {
        return map.last().map_or(0, |segment| segment.clean_end);
    };
    if segment.verbatim {
        segment.clean_start + (at - segment.original_start)
    } else if at == segment.original_start {
        segment.clean_start
    } else {
        segment.clean_end
    }
}

/// Range parity for `fixture` over the partition at `cuts` (ascending,
/// starting at 0 and ending at the text's end): per-range runs of either
/// transform cover exactly the comments of one whole-file run — the
/// per-range id maps union to the whole-file id map, the per-range removed
/// sets to the whole-file removed set — the blind texts concatenate to the
/// whole-file blind text, and each per-range clean text is the slice of the
/// whole-file clean text its range maps to.
fn range_parity_violation(
    fixture: &Fixture,
    targets: Option<&BTreeSet<String>>,
    cuts: &[usize],
) -> Option<String> {
    let key = &fixture.key;
    let whole_blind = fixture.redact(TransformOpts {
        targets,
        range: None,
    });
    let whole_clean = fixture.remove(TransformOpts {
        targets,
        range: None,
    });
    let whole_removed = removed_ids(fixture, &whole_clean);
    let mut id_map = BTreeMap::new();
    let mut blind_text = String::new();
    let mut removed = BTreeSet::new();
    for window in cuts.windows(2) {
        let range = window[0]..window[1];
        let blind = fixture.redact(TransformOpts {
            targets,
            range: Some(range.clone()),
        });
        id_map.extend(blind.id_map);
        blind_text.push_str(&blind.blind_text);
        let clean = fixture.remove(TransformOpts {
            targets,
            range: Some(range.clone()),
        });
        removed.extend(removed_ids(fixture, &clean));
        let slice = clean_offset_of(&whole_clean.offset_map, range.start)
            ..clean_offset_of(&whole_clean.offset_map, range.end);
        if clean.clean_text != whole_clean.clean_text[slice.clone()] {
            return Some(format!(
                "{key}: the clean text of {range:?} is {:?}, the whole-file slice {slice:?} is {:?}",
                clean.clean_text,
                &whole_clean.clean_text[slice.clone()]
            ));
        }
    }
    if id_map != whole_blind.id_map {
        return Some(format!(
            "{key}: per-range id maps {id_map:?}, whole file {:?}",
            whole_blind.id_map
        ));
    }
    if blind_text != whole_blind.blind_text {
        return Some(format!("{key}: the per-range blind texts differ"));
    }
    if removed != whole_removed {
        return Some(format!(
            "{key}: per-range removals {removed:?}, whole file {whole_removed:?}"
        ));
    }
    None
}

#[test]
fn per_line_ranges_match_one_whole_file_run_on_every_fixture() {
    // The finest partition: a range per comment-safe line boundary.
    let violations: Vec<String> = corpus()
        .filter_map(|fixture| range_parity_violation(fixture, None, &chunk_boundaries(fixture)))
        .collect();
    assert!(violations.is_empty(), "{violations:#?}");
}

proptest! {
    #![proptest_config(common::proptest_config())]

    /// Range parity over random partitions and target subsets (or the
    /// default targets): per-range runs of both transforms cover exactly
    /// the comments of one whole-file run, and each per-range clean text is
    /// the corresponding slice of the whole-file clean text.
    #[test]
    fn any_partition_matches_one_whole_file_run(
        fixture in any::<Index>(),
        select in proptest::option::of(proptest::collection::vec(any::<bool>(), 1..64)),
        cut in proptest::collection::vec(any::<bool>(), 1..64),
    ) {
        let fixtures: Vec<&Fixture> = corpus().collect();
        let fixture = fixtures[fixture.index(fixtures.len())];
        let targets: Option<BTreeSet<String>> = select.map(|select| {
            fixture
                .inventory
                .comments
                .iter()
                .enumerate()
                .filter(|(index, _)| select[index % select.len()])
                .map(|(_, comment)| comment.id.clone())
                .collect()
        });
        let boundaries = chunk_boundaries(fixture);
        let last = boundaries.len() - 1;
        let cuts: Vec<usize> = boundaries
            .iter()
            .enumerate()
            .filter(|&(index, _)| index == 0 || index == last || cut[index % cut.len()])
            .map(|(_, &at)| at)
            .collect();
        let violation = range_parity_violation(fixture, targets.as_ref(), &cuts);
        prop_assert!(violation.is_none(), "{:?}", violation);
    }
}

// ---- edge cases ----

impl Fixture {
    /// A fixture from inline text, inventoried as `file_name`.
    fn inline(language: LanguageId, file_name: &str, text: &str) -> Self {
        let file = FileRef {
            language,
            file_name: file_name.to_string(),
        };
        let inventory = inventory_file(text, file)
            .unwrap_or_else(|err| panic!("inventorying {file_name}: {err}"));
        Self {
            key: format!("inline/{file_name}"),
            language,
            text: text.to_string(),
            inventory,
        }
    }
}

/// Asserts both transforms leave `fixture` untouched under `targets`, on
/// the whole file and on every comment-safe line range: the blind text is
/// the original with an empty id map, and the clean text is the original
/// with an identity offset map (one verbatim segment).
fn assert_no_op(fixture: &Fixture, targets: Option<&BTreeSet<String>>) {
    let key = &fixture.key;
    let boundaries = chunk_boundaries(fixture);
    let ranges = std::iter::once(0..fixture.text.len())
        .chain(boundaries.windows(2).map(|window| window[0]..window[1]));
    for range in ranges {
        let opts = || TransformOpts {
            targets,
            range: Some(range.clone()),
        };
        let slice = &fixture.text[range.clone()];
        let blind = fixture.redact(opts());
        assert!(!blind.fallback, "{key} {range:?}");
        assert_eq!(blind.blind_text, slice, "{key} {range:?}");
        assert!(blind.id_map.is_empty(), "{key} {range:?}");
        let clean = fixture.remove(opts());
        assert_eq!(clean.clean_text, slice, "{key} {range:?}");
        assert_eq!(clean.original_base, range.start, "{key} {range:?}");
        assert_eq!(
            clean.offset_map,
            [Segment {
                clean_start: 0,
                clean_end: slice.len(),
                original_start: range.start,
                original_end: range.end,
                verbatim: true,
            }],
            "{key} {range:?}"
        );
        for offset in 0..=slice.len() {
            assert_eq!(
                map_clean_offset(&clean.offset_map, offset),
                range.start + offset,
                "{key} {range:?}"
            );
        }
    }
}

#[test]
fn files_without_editable_prose_pass_through_both_modes_unchanged() {
    let inline = [
        Fixture::inline(LanguageId::Typescript, "empty.ts", ""),
        Fixture::inline(LanguageId::Rust, "no-comments.rs", "fn main() {}\n"),
        Fixture::inline(
            LanguageId::Javascript,
            "directives-only.js",
            "// @ts-check\n/* eslint-disable no-console */\nconsole.log(1); // eslint-disable-line\n",
        ),
    ];
    let golden_no_prose: Vec<&Fixture> = corpus()
        .filter(|fixture| {
            !fixture
                .inventory
                .comments
                .iter()
                .any(|comment| comment.editable)
        })
        .collect();
    assert!(
        !golden_no_prose.is_empty(),
        "the corpus has no-prose fixtures"
    );
    for fixture in inline.iter().chain(golden_no_prose) {
        assert!(
            !fixture
                .inventory
                .comments
                .iter()
                .any(|comment| comment.editable),
            "{}",
            fixture.key
        );
        assert_no_op(fixture, None);
    }
}

#[test]
fn an_empty_target_set_hides_nothing_in_either_mode() {
    for fixture in corpus() {
        assert_no_op(fixture, Some(&BTreeSet::new()));
    }
}

#[test]
fn an_explicit_target_subset_hides_exactly_those_ids_in_both_modes() {
    let mut exercised = 0;
    for fixture in corpus() {
        let comments = &fixture.inventory.comments;
        let editable: Vec<&str> = comments
            .iter()
            .filter(|comment| comment.editable)
            .map(|comment| comment.id.as_str())
            .collect();
        // Every other editable id, plus ids that name nothing hideable: a
        // non-editable record's id and an unknown one.
        let mut targets: BTreeSet<String> = editable
            .iter()
            .step_by(2)
            .map(|id| (*id).to_string())
            .collect();
        let expected = targets.clone();
        targets.extend(
            comments
                .iter()
                .find(|comment| !comment.editable)
                .map(|comment| comment.id.clone()),
        );
        targets.insert("C99999".to_string());
        let opts = || TransformOpts {
            targets: Some(&targets),
            range: None,
        };
        let blind = fixture.redact(opts());
        let hidden: BTreeSet<String> = blind.id_map.keys().cloned().collect();
        assert_eq!(hidden, expected, "{} (redact)", fixture.key);
        let clean = fixture.remove(opts());
        assert_eq!(
            removed_ids(fixture, &clean),
            expected,
            "{} (remove)",
            fixture.key
        );
        // Everything else — including the editable non-targets — survives
        // verbatim (the shared violation checks assert exactly that).
        let whole = 0..fixture.text.len();
        let violation = round_trip_violation(fixture, &blind, &whole, &expected)
            .or_else(|| removal_violation(fixture, &clean, &whole, &expected));
        assert_eq!(violation, None);
        if !expected.is_empty() && expected.len() < editable.len() {
            exercised += 1;
        }
    }
    assert!(exercised > 0, "some fixture has a proper, non-empty subset");
}

/// Functional-only files: every comment is a load-bearing directive, so
/// both modes are no-ops rather than errors.
#[test]
fn functional_only_files_pass_through_both_modes_unchanged() {
    let python = Fixture::inline(
        LanguageId::Python,
        "functional.py",
        "import os  # noqa: F401\n\
         import sys  # noqa\n\
         \n\
         LIMIT = 10  # type: int\n\
         \n\
         \n\
         def scale(value):\n\
         \x20   # type: (int) -> int\n\
         \x20   return value * LIMIT  # type: ignore\n",
    );
    let go = Fixture::inline(
        LanguageId::Go,
        "functional.go",
        "//go:build linux\n\
         \n\
         package main\n\
         \n\
         import _ \"embed\"\n\
         \n\
         //go:generate stringer -type=Color\n\
         \n\
         //go:embed banner.txt\n\
         var banner string\n\
         \n\
         //go:noinline\n\
         func main() {}\n",
    );
    for fixture in [&python, &go] {
        let comments = &fixture.inventory.comments;
        assert!(comments.len() >= 4, "{}: {comments:#?}", fixture.key);
        for comment in comments {
            assert_eq!(
                comment.classification,
                Classification::Functional,
                "{}: {:?}",
                fixture.key,
                comment.text
            );
        }
        assert_no_op(fixture, None);
    }
}
