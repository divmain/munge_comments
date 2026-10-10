//! Artifact (de)serialization, the incremental writer, and the validation
//! boundary: round trips, refinement violations, and corrupt input.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use munge_comments::artifact::read::read_artifact;
use munge_comments::artifact::types::{
    CommentRecord, DecidedBy, Decision, Difficulty, FailureKind, FilePlan, FileStatus, FileWarning,
    FileWarningKind, PlanArtifact, SkipReason, Summary,
};
use munge_comments::artifact::validate::content_hash;
use munge_comments::artifact::write::{ArtifactHeader, ArtifactWriter, artifact_to_json};
use munge_comments::comments::types::{Anchor, Classification, CommentKind, Position};
use munge_comments::{
    Error, FileRef, LanguageId, Mode, PlanConfig, ValidatedArtifact, inventory_file,
};
use serde_json::{Value, json};

use common::{file_plan_from_inventory, tempdir};

const MATH_TS: &str = "\
// Adds two numbers together.
export function add(a: number, b: number): number {
  return a + b; // the sum
}

/** Multiplies two numbers. */
export function mul(a: number, b: number): number {
  // eslint-disable-next-line no-magic-numbers
  return a * b * 1;
}
";

fn config() -> PlanConfig {
    PlanConfig {
        mode: Mode::Remove,
        model: "gpt-4.1".to_owned(),
        base_url: None,
        allow_add: true,
        similarity_threshold: 0.85,
        max_file_chars: 60_000,
        concurrency: 4,
        offline: true,
        paths: vec![".".to_owned(), "src/**/*.ts".to_owned()],
        include: None,
        exclude: vec!["tests/fixtures/**".to_owned()],
    }
}

fn header() -> ArtifactHeader {
    ArtifactHeader {
        created_at: "2026-10-09T14:03:11Z".to_owned(),
        root: ".".to_owned(),
        glob_base: ".".to_owned(),
        config: config(),
    }
}

/// A stand-in for `compute_summary`: file counts per status and language.
fn stub_summary(files: &[FilePlan], _config: &PlanConfig) -> Result<Summary, Error> {
    let mut summary = Summary::default();
    for file in files {
        summary.files_considered += 1;
        summary.files_failed += u64::from(file.status == FileStatus::Failed);
        summary.files_skipped += u64::from(file.status == FileStatus::Skipped);
        *summary.languages.entry(file.language).or_default() += 1;
        summary.warnings += file.warnings.len() as u64;
    }
    summary.reduction_pct = 0.1 + 0.2; // a float that needs every digit
    Ok(summary)
}

fn failing_summary(_files: &[FilePlan], _config: &PlanConfig) -> Result<Summary, Error> {
    Err(Error::Invariant("summary stub refuses".to_owned()))
}

/// `math.ts` planned from its real inventory: prose rewritten (the first)
/// or kept, everything else protected; plus a NEW insertion and a warning.
fn math_plan() -> FilePlan {
    let inventory = inventory_file(
        MATH_TS,
        FileRef {
            language: LanguageId::Typescript,
            file_name: "math.ts".to_owned(),
        },
    )
    .unwrap();
    let mut plan = file_plan_from_inventory(
        "src/math.ts",
        MATH_TS,
        &inventory,
        LanguageId::Typescript,
        |comment| match (comment.classification, comment.id.as_str()) {
            (Classification::Prose, "C1") => (
                Decision::KeepGenerated,
                Some("// Returns a + b.".to_owned()),
            ),
            (Classification::Prose, _) => (Decision::Remove, None),
            _ => (Decision::KeepProtected, None),
        },
    );
    let first = &mut plan.comments[0];
    first.regenerated = Some("// Returns a + b.".to_owned());
    first.difficulty = Some(Difficulty::Easy);
    first.difficulty_rationale = Some("obvious from the body".to_owned());
    first.confidence = 0.82;
    first.decided_by = DecidedBy::Judge;
    let insertion = MATH_TS.find("export function mul").unwrap();
    plan.comments
        .push(new_record("N1", insertion, Decision::New));
    plan.comments
        .sort_by(|a, b| (a.start, &a.id).cmp(&(b.start, &b.id)));
    plan.warnings.push(FileWarning {
        kind: FileWarningKind::UnresolvableAnchor,
        detail: "citation 3 named no declaration".to_owned(),
    });
    plan
}

fn new_record(id: &str, offset: usize, decision: Decision) -> CommentRecord {
    CommentRecord {
        id: id.to_owned(),
        kind: CommentKind::Line,
        start: offset,
        end: offset,
        line: 6,
        column: 1,
        position: Position::Leading,
        classification: Classification::Prose,
        tags: Vec::new(),
        original: None,
        anchor: Anchor {
            node_kind: "function_declaration".to_owned(),
            name: Some("mul".to_owned()),
            snippet: "export function mul(a: number, b: number): number {".to_owned(),
        },
        regenerated: None,
        would_you_comment_here: None,
        difficulty: Some(Difficulty::Medium),
        difficulty_rationale: None,
        missing_context: Some("why multiply by one".to_owned()),
        decision,
        final_comment: Some("// Multiplies; the factor documents units.".to_owned()),
        confidence: 0.9,
        decision_rationale: Some("aligned citation".to_owned()),
        decided_by: DecidedBy::Engine,
    }
}

fn unread_file(path: &str, language: LanguageId, status: FileStatus) -> FilePlan {
    FilePlan {
        path: path.to_owned(),
        content_hash: None,
        byte_length: 0,
        language,
        effective_mode: Mode::Remove,
        status,
        failure_kind: None,
        skip_reason: None,
        error: None,
        warnings: Vec::new(),
        comments: Vec::new(),
    }
}

fn failed_io() -> FilePlan {
    FilePlan {
        failure_kind: Some(FailureKind::Io),
        error: Some("permission denied".to_owned()),
        ..unread_file("src/locked.py", LanguageId::Python, FileStatus::Failed)
    }
}

fn failed_llm() -> FilePlan {
    FilePlan {
        content_hash: Some(content_hash(b"fn main() {}\n")),
        byte_length: 13,
        failure_kind: Some(FailureKind::Llm),
        error: Some("llm error (timeout)".to_owned()),
        ..unread_file("src/main.rs", LanguageId::Rust, FileStatus::Failed)
    }
}

fn skipped() -> FilePlan {
    FilePlan {
        content_hash: Some(content_hash(b"\xff\xfe")),
        byte_length: 2,
        skip_reason: Some(SkipReason::NonUtf8),
        error: Some("invalid UTF-8 at byte 0".to_owned()),
        ..unread_file("pkg/latin1.go", LanguageId::Go, FileStatus::Skipped)
    }
}

/// Writes `files` through a fresh writer at `<dir>/plan.json`.
fn write_plan(dir: &Path, files: Vec<FilePlan>) -> (PathBuf, PlanArtifact) {
    let path = dir.join("plan.json");
    let writer = ArtifactWriter::new(&path, header(), files, stub_summary).unwrap();
    (path, writer.finish().unwrap())
}

/// A valid artifact, as JSON, for hand-edit tests.
fn valid_json() -> Value {
    let dir = tempdir();
    let (path, _) = write_plan(dir.path(), vec![math_plan(), failed_io(), skipped()]);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// Writes `value` to a temp artifact and reads it back.
fn read_json(value: &Value) -> Result<ValidatedArtifact, Error> {
    let dir = tempdir();
    let path = dir.path().join("edited.json");
    fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    read_artifact(&path)
}

/// Asserts the edited artifact is rejected with exit 2 and a message
/// containing every `needle`.
fn assert_rejected(value: &Value, needles: &[&str]) {
    let err = read_json(value).expect_err("the edited artifact must be rejected");
    assert_eq!(err.exit_code(), 2, "{err}");
    let message = err.to_string();
    for needle in needles {
        assert!(message.contains(needle), "{message:?} lacks {needle:?}");
    }
}

/// The JSON of the file entry at `path`.
fn file_mut<'a>(value: &'a mut Value, path: &str) -> &'a mut Value {
    value["files"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|file| file["path"] == path)
        .unwrap()
}

/// The JSON of `math.ts`'s record `id`.
fn record_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    math_mut(value)["comments"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["id"] == id)
        .unwrap()
}

/// The JSON of the `src/math.ts` file entry.
fn math_mut(value: &mut Value) -> &mut Value {
    file_mut(value, "src/math.ts")
}

// ---- round trips -----------------------------------------------------------

#[test]
fn write_then_read_is_lossless() {
    let dir = tempdir();
    let files = vec![math_plan(), failed_io(), failed_llm(), skipped()];
    let (path, written) = write_plan(dir.path(), files);
    let read = read_artifact(&path).unwrap();
    assert_eq!(*read, written);
    assert_eq!(read.into_inner(), written);
    // Re-serializing the read artifact reproduces the file byte-for-byte.
    let on_disk = fs::read_to_string(&path).unwrap();
    assert_eq!(artifact_to_json(&written).unwrap(), on_disk);
    assert!(on_disk.ends_with("}\n"));
    assert!(on_disk.starts_with("{\n  \"version\": 1,\n  \"createdAt\""));
}

#[test]
fn failed_and_skipped_entries_round_trip() {
    let dir = tempdir();
    let (path, written) = write_plan(dir.path(), vec![failed_io(), failed_llm(), skipped()]);
    let read = read_artifact(&path).unwrap().into_inner();
    assert_eq!(read, written);
    let by_path = |p: &str| read.files.iter().find(|f| f.path == p).unwrap().clone();
    assert_eq!(by_path("src/locked.py"), failed_io());
    assert_eq!(by_path("src/main.rs"), failed_llm());
    assert_eq!(by_path("pkg/latin1.go"), skipped());
}

#[test]
fn options_serialize_as_null() {
    let value = valid_json();
    let locked = value["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "src/locked.py")
        .unwrap();
    assert_eq!(locked["contentHash"], Value::Null);
    assert_eq!(locked["skipReason"], Value::Null);
    assert_eq!(value["config"]["baseUrl"], Value::Null);
    assert_eq!(value["config"]["include"], Value::Null);
}

#[test]
fn a_hand_flipped_rewrite_keeping_its_final_comment_validates() {
    let mut value = valid_json();
    let record = record_mut(&mut value, "C1");
    assert_eq!(record["decision"], "KEEP_GENERATED");
    record["decision"] = json!("KEEP_ORIGINAL");
    record["decidedBy"] = json!("human");
    let read = read_json(&value).unwrap();
    let math = read.files.iter().find(|f| f.path == "src/math.ts").unwrap();
    assert_eq!(math.comments[0].decision, Decision::KeepOriginal);
    assert_eq!(
        math.comments[0].final_comment.as_deref(),
        Some("// Returns a + b.")
    );
}

#[test]
fn a_held_new_record_may_be_flipped_back_to_new() {
    let mut value = valid_json();
    record_mut(&mut value, "N1")["decision"] = json!("NEEDS_REVIEW");
    read_json(&value).unwrap();
    record_mut(&mut value, "N1")["decision"] = json!("NEW");
    read_json(&value).unwrap();
}

// ---- the writer ------------------------------------------------------------

#[test]
fn a_writer_with_no_files_leaves_a_valid_empty_artifact() {
    let dir = tempdir();
    let path = dir.path().join("plan.json");
    let writer = ArtifactWriter::new(&path, header(), Vec::new(), stub_summary).unwrap();
    // Written immediately, before any record or finish.
    let early = read_artifact(&path).unwrap();
    assert!(early.files.is_empty());
    assert_eq!(early.version, 1);
    assert_eq!(early.config, config());
    let finished = writer.finish().unwrap();
    assert_eq!(read_artifact(&path).unwrap().into_inner(), finished);
    assert_eq!(finished.summary.files_considered, 0);
}

#[test]
fn the_writer_keeps_files_sorted_and_replaces_by_path() {
    let dir = tempdir();
    let path = dir.path().join("plan.json");
    let mut writer =
        ArtifactWriter::new(&path, header(), vec![failed_llm(), skipped()], stub_summary).unwrap();
    let paths = |artifact: &PlanArtifact| -> Vec<String> {
        artifact.files.iter().map(|f| f.path.clone()).collect()
    };
    assert_eq!(
        paths(&read_artifact(&path).unwrap()),
        ["pkg/latin1.go", "src/main.rs"]
    );
    writer.record(math_plan()).unwrap();
    writer.record(failed_io()).unwrap();
    let on_disk = read_artifact(&path).unwrap();
    assert_eq!(
        paths(&on_disk),
        [
            "pkg/latin1.go",
            "src/locked.py",
            "src/main.rs",
            "src/math.ts"
        ]
    );
    assert_eq!(on_disk.summary.files_considered, 4);
    assert_eq!(on_disk.summary.files_failed, 2);
    // Re-recording a path replaces its entry and recomputes the summary.
    let mut retried = failed_llm();
    retried.status = FileStatus::Skipped;
    retried.failure_kind = None;
    retried.skip_reason = Some(SkipReason::SyntaxErrors);
    writer.record(retried.clone()).unwrap();
    let finished = writer.finish().unwrap();
    assert_eq!(finished.files.len(), 4);
    assert_eq!(finished.files[2], retried);
    assert_eq!(finished.summary.files_failed, 1);
    assert_eq!(finished.summary.files_skipped, 2);
    assert_eq!(read_artifact(&path).unwrap().into_inner(), finished);
}

#[test]
fn writer_errors_keep_their_own_variant() {
    let dir = tempdir();
    let err = ArtifactWriter::new(
        &dir.path().join("plan.json"),
        header(),
        Vec::new(),
        failing_summary,
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 7);
    let err = ArtifactWriter::new(
        &dir.path().join("missing-dir").join("plan.json"),
        header(),
        Vec::new(),
        stub_summary,
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 6);
}

// ---- reading ---------------------------------------------------------------

#[test]
fn a_relative_artifact_path_is_a_usage_error() {
    let err = read_artifact(Path::new("plan.json")).unwrap_err();
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn a_missing_artifact_is_exit_2() {
    let dir = tempdir();
    let path = dir.path().join("nope.json");
    let err = read_artifact(&path).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("nope.json"), "{err}");
}

#[test]
fn truncated_json_is_exit_2_with_a_pointed_message() {
    let dir = tempdir();
    let (path, _) = write_plan(dir.path(), vec![math_plan()]);
    let full = fs::read_to_string(&path).unwrap();
    fs::write(&path, &full[..full.len() / 2]).unwrap();
    let err = read_artifact(&path).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let message = err.to_string();
    assert!(message.contains("truncated"), "{message}");
    assert!(message.contains("plan.json"), "{message}");
    assert!(message.contains("line "), "{message}");
}

#[test]
fn corrupt_json_is_exit_2_with_a_pointed_message() {
    let dir = tempdir();
    let path = dir.path().join("plan.json");
    fs::write(&path, "{\n  \"version\": 1,\n  oops\n}\n").unwrap();
    let err = read_artifact(&path).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let message = err.to_string();
    assert!(message.contains("malformed"), "{message}");
    assert!(message.contains("line 3 column 3"), "{message}");
    fs::write(&path, b"\xff\xfe not json").unwrap();
    assert_eq!(read_artifact(&path).unwrap_err().exit_code(), 2);
}

// ---- refinement violations (each is exit 2, naming file and record) --------

#[test]
fn a_bad_version_is_rejected() {
    let mut value = valid_json();
    value["version"] = json!(2);
    assert_rejected(&value, &["version 2"]);
}

#[test]
fn an_unknown_key_is_rejected() {
    let mut value = valid_json();
    record_mut(&mut value, "C1")["finalComent"] = json!("typo");
    assert_rejected(&value, &["finalComent", "line "]);
    let mut value = valid_json();
    value["summary"]["extra"] = json!(0);
    assert_rejected(&value, &["extra"]);
}

#[test]
fn an_absolute_glob_base_is_rejected() {
    for bad in ["/home/user/project", "C:/project", ""] {
        let mut value = valid_json();
        value["globBase"] = json!(bad);
        assert_rejected(&value, &["globBase"]);
    }
    let mut value = valid_json();
    value["globBase"] = json!("../pkg");
    read_json(&value).unwrap();
}

#[test]
fn an_unknown_language_is_rejected() {
    let mut value = valid_json();
    math_mut(&mut value)["language"] = json!("kotlin");
    assert_rejected(&value, &["kotlin"]);
}

#[test]
fn a_mismatched_language_is_rejected() {
    let mut value = valid_json();
    math_mut(&mut value)["language"] = json!("javascript");
    assert_rejected(&value, &["src/math.ts", "javascript", "typescript"]);
    let mut value = valid_json();
    math_mut(&mut value)["path"] = json!("src/math.txt");
    assert_rejected(&value, &["src/math.txt", "no supported language"]);
}

#[test]
fn duplicate_paths_are_rejected_after_normalization() {
    for alias in ["src/math.ts", "src/x/../math.ts", "./src//math.ts"] {
        let mut value = valid_json();
        let mut copy = math_mut(&mut value).clone();
        copy["path"] = json!(alias);
        value["files"].as_array_mut().unwrap().push(copy);
        assert_rejected(&value, &["duplicate file entry"]);
    }
}

#[test]
fn duplicate_ids_are_rejected() {
    let mut value = valid_json();
    record_mut(&mut value, "C2")["id"] = json!("C1");
    assert_rejected(&value, &["src/math.ts", "duplicate comment id C1"]);
}

#[test]
fn malformed_ids_are_rejected() {
    for bad in ["C0", "C01", "c1", "X1", "C", "C1a", "N"] {
        let mut value = valid_json();
        record_mut(&mut value, "C2")["id"] = json!(bad);
        assert_rejected(&value, &["src/math.ts", "^[CN][1-9][0-9]*$"]);
    }
}

#[test]
fn id_prefix_must_agree_with_original() {
    let mut value = valid_json();
    record_mut(&mut value, "C2")["original"] = Value::Null;
    assert_rejected(&value, &["src/math.ts: C2", "original is null"]);
    let mut value = valid_json();
    record_mut(&mut value, "N1")["original"] = json!("// smuggled");
    assert_rejected(&value, &["src/math.ts: N1", "original is null"]);
}

#[test]
fn new_on_a_scanned_record_is_rejected() {
    let mut value = valid_json();
    let record = record_mut(&mut value, "C2");
    record["decision"] = json!("NEW");
    record["finalComment"] = json!("// sum");
    assert_rejected(&value, &["src/math.ts: C2", "may not be NEW"]);
}

#[test]
fn new_origin_records_may_only_be_new_or_needs_review() {
    for decision in [
        "REMOVE",
        "KEEP_ORIGINAL",
        "KEEP_GENERATED",
        "HYBRID",
        "KEEP_PROTECTED",
    ] {
        let mut value = valid_json();
        record_mut(&mut value, "N1")["decision"] = json!(decision);
        assert_rejected(
            &value,
            &["src/math.ts: N1", "NEW or NEEDS_REVIEW", decision],
        );
    }
}

#[test]
fn new_origin_records_in_a_redact_file_are_rejected() {
    let mut value = valid_json();
    math_mut(&mut value)["effectiveMode"] = json!("redact");
    assert_rejected(&value, &["src/math.ts: N1", "\"remove\""]);
}

#[test]
fn a_new_start_beyond_the_byte_length_is_rejected() {
    let mut value = valid_json();
    let length = MATH_TS.len();
    let record = record_mut(&mut value, "N1");
    record["start"] = json!(length);
    record["end"] = json!(length);
    read_json(&value).expect("insertion at end of file is in bounds");
    let record = record_mut(&mut value, "N1");
    record["start"] = json!(length + 1);
    record["end"] = json!(length + 1);
    assert_rejected(&value, &["src/math.ts: N1", "beyond the file length"]);
}

#[test]
fn a_new_record_must_be_zero_length() {
    let mut value = valid_json();
    let record = record_mut(&mut value, "N1");
    record["end"] = json!(record["start"].as_u64().unwrap() + 1);
    assert_rejected(&value, &["src/math.ts: N1", "start == end"]);
}

#[test]
fn start_after_end_is_rejected() {
    let mut value = valid_json();
    let record = record_mut(&mut value, "C2");
    record["start"] = json!(record["end"].as_u64().unwrap() + 1);
    assert_rejected(&value, &["src/math.ts: C2", "exceeds end"]);
}

#[test]
fn overlapping_scanned_ranges_are_rejected() {
    let mut value = valid_json();
    let c1_end = record_mut(&mut value, "C1")["end"].as_u64().unwrap();
    record_mut(&mut value, "C2")["start"] = json!(c1_end - 1);
    assert_rejected(&value, &["src/math.ts", "overlap", "C1", "C2"]);
    // A duplicated record under a fresh id overlaps its twin.
    let mut value = valid_json();
    let mut twin = record_mut(&mut value, "C3").clone();
    twin["id"] = json!("C9");
    math_mut(&mut value)["comments"]
        .as_array_mut()
        .unwrap()
        .push(twin);
    assert_rejected(&value, &["overlap", "C3", "C9"]);
}

#[test]
fn a_range_nested_inside_another_is_rejected() {
    let mut value = valid_json();
    let c1_end = record_mut(&mut value, "C1")["end"].as_u64().unwrap();
    // Widen C1 to swallow C2 and C3 entirely.
    let c3_end = record_mut(&mut value, "C3")["end"].as_u64().unwrap();
    record_mut(&mut value, "C1")["end"] = json!(c3_end.max(c1_end));
    assert_rejected(&value, &["overlap", "C1"]);
}

#[test]
fn a_writing_decision_without_final_comment_is_rejected() {
    for decision in ["KEEP_GENERATED", "HYBRID"] {
        let mut value = valid_json();
        let record = record_mut(&mut value, "C2");
        record["decision"] = json!(decision);
        record["finalComment"] = Value::Null;
        assert_rejected(&value, &["src/math.ts: C2", decision, "finalComment"]);
    }
    let mut value = valid_json();
    record_mut(&mut value, "N1")["finalComment"] = Value::Null;
    assert_rejected(&value, &["src/math.ts: N1", "NEW", "finalComment"]);
}

#[test]
fn confidence_outside_the_unit_interval_is_rejected() {
    for bad in [-0.01, 1.5] {
        let mut value = valid_json();
        record_mut(&mut value, "C1")["confidence"] = json!(bad);
        assert_rejected(&value, &["src/math.ts: C1", "confidence"]);
    }
}

#[test]
fn a_null_content_hash_on_a_complete_file_is_rejected() {
    let mut value = valid_json();
    math_mut(&mut value)["contentHash"] = Value::Null;
    assert_rejected(&value, &["src/math.ts", "contentHash"]);
}

#[test]
fn a_malformed_content_hash_is_rejected() {
    let mut value = valid_json();
    let hash = math_mut(&mut value)["contentHash"]
        .as_str()
        .unwrap()
        .to_uppercase();
    math_mut(&mut value)["contentHash"] = json!(hash);
    assert_rejected(&value, &["src/math.ts", "64 lowercase hex"]);
}

#[test]
fn status_fields_must_agree() {
    let mut value = valid_json();
    file_mut(&mut value, "src/locked.py")["failureKind"] = Value::Null;
    assert_rejected(&value, &["src/locked.py", "failureKind"]);

    let mut value = valid_json();
    file_mut(&mut value, "src/math.ts")["failureKind"] = json!("llm");
    assert_rejected(&value, &["src/math.ts", "failureKind"]);

    let mut value = valid_json();
    file_mut(&mut value, "pkg/latin1.go")["skipReason"] = Value::Null;
    assert_rejected(&value, &["pkg/latin1.go", "skipReason"]);

    let mut value = valid_json();
    file_mut(&mut value, "src/math.ts")["skipReason"] = json!("too-deep");
    assert_rejected(&value, &["src/math.ts", "skipReason"]);

    // An io failure was never read: its hash must be null.
    let mut value = valid_json();
    file_mut(&mut value, "src/locked.py")["contentHash"] = json!(content_hash(b""));
    assert_rejected(&value, &["src/locked.py", "contentHash"]);

    // Any other failure was read: its hash must be present.
    let mut value = valid_json();
    file_mut(&mut value, "src/locked.py")["failureKind"] = json!("llm");
    assert_rejected(&value, &["src/locked.py", "contentHash"]);
}
