//! The real-write apply path (`apply::run::run_apply`): drift and
//! selection, firewall checks (a) and (b), the NEW-offset checks,
//! `allowAdd`, per-file outcomes, permissions, reports and exit codes.
//!
//! Every test plans temp copies of small sources with
//! `common::file_plan_from_inventory`, hand-edits the plan where the test
//! needs it, writes the artifact beside the sources, and runs `run_apply`
//! against that directory.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use munge_comments::apply::edits::{EditOpts, apply_edits, compute_edits};
use munge_comments::apply::run::exit_code_for;
use munge_comments::apply::types::{ApplyFileResult, ApplyStatus, SkippedEdit, SkippedEditReason};
use munge_comments::artifact::types::{
    ARTIFACT_VERSION, CommentRecord, DecidedBy, Decision, FailureKind, FilePlan, FileStatus,
};
use munge_comments::comments::types::{Anchor, Classification, CommentKind, Position};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{
    ApplyDeps, ApplyHooks, ApplyOutcome, ApplyRequest, ApplyResult, CapturingLog, Error, FileRef,
    Inventory, InventoryComment, Mode, PlanArtifact, PlanConfig, compute_summary, inventory_file,
    language_for, run_apply,
};
use tempfile::TempDir;

use Decision::{KeepGenerated, KeepOriginal, KeepProtected, NeedsReview, New, Remove};

/// The default report file name the CLI would pass.
const REPORT: &str = "apply-report.md";

/// A temp source tree with its (hand-editable) plan.
struct Repo {
    dir: TempDir,
    files: Vec<FilePlan>,
    allow_add: bool,
}

/// One run's outcome and log.
struct Run {
    outcome: Result<ApplyOutcome, Error>,
    log: Vec<String>,
}

impl Run {
    /// The outcome, asserting the run did not abort.
    fn ok(&self) -> &ApplyOutcome {
        match &self.outcome {
            Ok(outcome) => outcome,
            Err(err) => panic!("run aborted: {err}"),
        }
    }

    /// The abort's exit code, asserting the run aborted.
    fn err_code(&self) -> u8 {
        match &self.outcome {
            Ok(outcome) => panic!("run did not abort: {:?}", outcome.result),
            Err(err) => err.exit_code(),
        }
    }

    fn err_message(&self) -> String {
        match &self.outcome {
            Ok(outcome) => panic!("run did not abort: {:?}", outcome.result),
            Err(err) => err.to_string(),
        }
    }

    fn exit_code(&self) -> u8 {
        self.ok().exit_code
    }

    /// The result for artifact path `path`.
    fn file(&self, path: &str) -> &ApplyFileResult {
        self.ok()
            .result
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("no result for {path}"))
    }

    fn status(&self, path: &str) -> ApplyStatus {
        self.file(path).status
    }
}

/// Decides each scanned record by its raw text: an exact `decisions` match
/// wins; other prose records are kept, every non-prose record protected.
fn by_text(
    decisions: &[(&str, Decision, Option<&str>)],
) -> impl Fn(&InventoryComment) -> (Decision, Option<String>) {
    let decisions: Vec<(String, Decision, Option<String>)> = decisions
        .iter()
        .map(|(text, decision, final_comment)| {
            (
                (*text).to_owned(),
                *decision,
                final_comment.map(str::to_owned),
            )
        })
        .collect();
    move |comment| {
        decisions
            .iter()
            .find(|(text, _, _)| *text == comment.text)
            .map_or_else(
                || {
                    let fallback = if comment.classification == Classification::Prose {
                        KeepOriginal
                    } else {
                        KeepProtected
                    };
                    (fallback, None)
                },
                |(_, decision, final_comment)| (*decision, final_comment.clone()),
            )
    }
}

fn inventory_of(path: &str, text: &str) -> Inventory {
    let language = language_for(Path::new(path)).expect("known extension");
    inventory_file(
        text,
        FileRef {
            language,
            file_name: path.to_owned(),
        },
    )
    .expect("inventory")
}

impl Repo {
    fn new() -> Repo {
        Repo {
            dir: common::tempdir(),
            files: Vec::new(),
            allow_add: true,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn abs(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    /// Writes `text` at `path` and plans it, deciding records with
    /// `decisions` (see [`by_text`]).
    fn add(&mut self, path: &str, text: &str, decisions: &[(&str, Decision, Option<&str>)]) {
        let abs = self.abs(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, text).unwrap();
        let inventory = inventory_of(path, text);
        let language = language_for(Path::new(path)).unwrap();
        let plan =
            common::file_plan_from_inventory(path, text, &inventory, language, by_text(decisions));
        self.files.push(plan);
    }

    /// Adds an artifact entry recorded as `failed` (LLM), never written to
    /// disk here.
    fn add_failed(&mut self, path: &str) {
        let language = language_for(Path::new(path)).unwrap();
        self.files.push(FilePlan {
            path: path.to_owned(),
            content_hash: Some(format!("sha256:{}", "0".repeat(64))),
            byte_length: 0,
            language,
            effective_mode: Mode::Remove,
            status: FileStatus::Failed,
            failure_kind: Some(FailureKind::Llm),
            skip_reason: None,
            error: Some("endpoint unavailable".to_owned()),
            warnings: Vec::new(),
            comments: Vec::new(),
        });
    }

    fn plan_mut(&mut self, path: &str) -> &mut FilePlan {
        self.files
            .iter_mut()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("no plan for {path}"))
    }

    /// The scanned record of `path` whose raw text starts with `needle`.
    fn record_mut(&mut self, path: &str, needle: &str) -> &mut CommentRecord {
        self.plan_mut(path)
            .comments
            .iter_mut()
            .find(|record| {
                record
                    .original
                    .as_deref()
                    .is_some_and(|original| original.starts_with(needle))
            })
            .unwrap_or_else(|| panic!("no record starting with {needle:?} in {path}"))
    }

    /// Appends a NEW-origin record to `path`'s plan.
    fn push_new(&mut self, path: &str, id: &str, offset: usize, decision: Decision, text: &str) {
        self.push_new_at(path, id, offset, Position::Leading, decision, text);
    }

    fn push_new_at(
        &mut self,
        path: &str,
        id: &str,
        offset: usize,
        position: Position,
        decision: Decision,
        text: &str,
    ) {
        let mut record = new_record(id, offset, decision, text);
        record.position = position;
        self.plan_mut(path).comments.push(record);
    }

    fn config(&self) -> PlanConfig {
        PlanConfig {
            mode: Mode::Remove,
            model: "test-model".to_owned(),
            base_url: None,
            allow_add: self.allow_add,
            similarity_threshold: 0.6,
            max_file_chars: 120_000,
            concurrency: 1,
            offline: true,
            paths: vec![".".to_owned()],
            include: None,
            exclude: Vec::new(),
        }
    }

    /// Validates and writes the artifact to `plan.json` in the root.
    fn write_artifact(&self) -> PathBuf {
        let mut files = self.files.clone();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let config = self.config();
        let summary = compute_summary(&files, &config).unwrap();
        let artifact = PlanArtifact {
            version: ARTIFACT_VERSION,
            created_at: "2026-10-10T00:00:00Z".to_owned(),
            root: ".".to_owned(),
            glob_base: ".".to_owned(),
            config,
            files,
            summary,
        };
        let artifact = artifact.validate().expect("valid artifact").into_inner();
        let path = self.abs("plan.json");
        fs::write(&path, serde_json::to_string_pretty(&artifact).unwrap()).unwrap();
        path
    }

    /// Runs apply with `only`, a report at [`REPORT`], and no hooks.
    fn run(&self, only: &[&str]) -> Run {
        self.run_with(only, Some(REPORT), ApplyHooks::default())
    }

    fn run_with(&self, only: &[&str], report: Option<&str>, hooks: ApplyHooks) -> Run {
        let (run, stdout) = self.run_request(only, false, report, hooks);
        assert!(stdout.is_empty(), "a real run prints nothing to stdout");
        run
    }

    /// A dry run (asking for a report at [`REPORT`], which it must not
    /// write); returns the run and its stdout.
    fn run_dry(&self, hooks: ApplyHooks) -> (Run, String) {
        self.run_request(&[], true, Some(REPORT), hooks)
    }

    fn run_request(
        &self,
        only: &[&str],
        dry_run: bool,
        report: Option<&str>,
        hooks: ApplyHooks,
    ) -> (Run, String) {
        self.write_artifact();
        let log = CapturingLog::new();
        let mut stdout: Vec<u8> = Vec::new();
        let outcome = run_apply(
            ApplyRequest {
                artifact: PathBuf::from("plan.json"),
                only: only.iter().map(|path| (*path).to_owned()).collect(),
                dry_run,
                report_path: report.map(PathBuf::from),
            },
            ApplyDeps {
                log: &log,
                stdout: &mut stdout,
                cwd: self.root(),
                hooks,
            },
        );
        let run = Run {
            outcome,
            log: log.lines(),
        };
        (run, String::from_utf8(stdout).expect("UTF-8 diffs"))
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.abs(path)).unwrap()
    }

    fn report_exists(&self) -> bool {
        self.abs(REPORT).exists()
    }
}

fn new_record(id: &str, offset: usize, decision: Decision, text: &str) -> CommentRecord {
    CommentRecord {
        id: id.to_owned(),
        kind: CommentKind::Line,
        start: offset,
        end: offset,
        line: 1,
        column: 1,
        position: Position::Leading,
        classification: Classification::Prose,
        tags: Vec::new(),
        original: None,
        anchor: Anchor {
            node_kind: "expression_statement".to_owned(),
            name: None,
            snippet: String::new(),
        },
        regenerated: Some(text.to_owned()),
        would_you_comment_here: None,
        difficulty: None,
        difficulty_rationale: None,
        missing_context: None,
        decision,
        final_comment: Some(text.to_owned()),
        confidence: 1.0,
        decision_rationale: None,
        decided_by: DecidedBy::Human,
    }
}

/// Hand-edits the record starting with `needle` the way a human routing an
/// edit into a protected comment would: relabeled `prose`, decided
/// `REMOVE`.
fn relabel_remove(repo: &mut Repo, path: &str, needle: &str) {
    let record = repo.record_mut(path, needle);
    record.classification = Classification::Prose;
    record.tags.clear();
    record.decision = Remove;
    record.final_comment = None;
}

// ---------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------

const PY: &str = "\
def add(a, b):
    # add the numbers
    return a + b  # sum


def mul(a, b):
    # multiply them
    return a * b
";

const TS: &str = "\
// increments the counter
export function inc(n: number): number {
  return n + 1;
}
";

/// A repo with `a.py` (one REMOVE, one KEEP_GENERATED, one trailing
/// REMOVE) and `b.ts` (one REMOVE).
fn two_file_repo() -> Repo {
    let mut repo = Repo::new();
    repo.add(
        "a.py",
        PY,
        &[
            ("# add the numbers", Remove, None),
            ("# sum", Remove, None),
            ("# multiply them", KeepGenerated, Some("Multiply a by b.")),
        ],
    );
    repo.add(
        "src/b.ts",
        TS,
        &[("// increments the counter", Remove, None)],
    );
    repo
}

const PY_APPLIED: &str = "\
def add(a, b):
    return a + b


def mul(a, b):
    # Multiply a by b.
    return a * b
";

const TS_APPLIED: &str = "\
export function inc(n: number): number {
  return n + 1;
}
";

// ---------------------------------------------------------------------
// The happy path and reports
// ---------------------------------------------------------------------

#[test]
fn a_clean_run_applies_every_file_and_writes_the_report() {
    let repo = two_file_repo();
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0, "{:?}", run.ok().result);
    assert_eq!(run.status("a.py"), ApplyStatus::Applied);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::Applied);
    assert_eq!(run.file("a.py").reason, None);
    assert_eq!(repo.read("a.py"), PY_APPLIED);
    assert_eq!(repo.read("src/b.ts"), TS_APPLIED);
    let report = repo.read(REPORT);
    assert!(report.contains("a.py"), "{report}");
    assert!(
        run.log.iter().any(|line| line == "apply: 2 applied"),
        "{:?}",
        run.log
    );
    assert_eq!(run.ok().result.partial_apply_hint, None);
}

#[test]
fn no_report_path_writes_no_report() {
    let repo = two_file_repo();
    let run = repo.run_with(&[], None, ApplyHooks::default());
    assert_eq!(run.exit_code(), 0);
    assert_eq!(repo.read("a.py"), PY_APPLIED);
    let markdown: Vec<_> = fs::read_dir(repo.root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".md"))
        .collect();
    assert!(markdown.is_empty(), "{markdown:?}");
}

#[test]
fn a_report_in_a_missing_directory_is_created() {
    let repo = two_file_repo();
    let run = repo.run_with(&[], Some("out/nested/report.md"), ApplyHooks::default());
    assert_eq!(run.exit_code(), 0);
    assert!(repo.abs("out/nested/report.md").is_file());
}

#[test]
fn an_unwritable_report_path_is_exit_6_before_any_write() {
    let repo = two_file_repo();
    fs::write(repo.abs("blocker"), "a file, not a directory").unwrap();
    let run = repo.run_with(&[], Some("blocker/report.md"), ApplyHooks::default());
    assert_eq!(run.err_code(), 6);
    assert_eq!(repo.read("a.py"), PY, "sources are untouched");
    assert_eq!(repo.read("src/b.ts"), TS);
}

#[test]
fn files_without_actionable_edits_and_unprocessed_files_are_never_read() {
    let mut repo = two_file_repo();
    // Only KEEP_ORIGINAL / held decisions: nothing to write.
    repo.add("kept.py", "x = 1  # keep me\n", &[]);
    repo.push_new("kept.py", "N1", 0, NeedsReview, "held");
    repo.add_failed("failed.py");
    // Neither is hashed: editing the one and never creating the other is
    // not drift.
    fs::write(repo.abs("kept.py"), "x = 2  # edited since the plan\n").unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0, "{:?}", run.ok().result);
    assert_eq!(run.status("kept.py"), ApplyStatus::Unchanged);
    let failed = run.file("failed.py");
    assert_eq!(failed.status, ApplyStatus::SkippedPlanStatus);
    let reason = failed.reason.as_deref().unwrap();
    assert!(reason.contains("failed (llm)"), "{reason}");
    assert!(reason.contains("endpoint unavailable"), "{reason}");
    assert_eq!(run.status("a.py"), ApplyStatus::Applied);
}

#[test]
fn a_file_whose_edits_are_all_skipped_is_unchanged_with_its_skips() {
    let mut repo = Repo::new();
    // `*/` cannot be written into a block slot.
    let text = "/* old note */\nconst x = 1;\n";
    repo.add(
        "a.js",
        text,
        &[("/* old note */", KeepGenerated, Some("a */ b"))],
    );
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0);
    let file = run.file("a.js");
    assert_eq!(file.status, ApplyStatus::Unchanged);
    assert_eq!(
        file.skipped_edits,
        [SkippedEdit {
            id: "C1".to_owned(),
            reason: SkippedEditReason::FinalCommentInvalid,
        }]
    );
    assert_eq!(repo.read("a.js"), text);
}

// ---------------------------------------------------------------------
// Drift and selection
// ---------------------------------------------------------------------

#[test]
fn drift_in_any_selected_file_aborts_before_any_write() {
    let repo = two_file_repo();
    fs::write(repo.abs("src/b.ts"), format!("{TS}// appended\n")).unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 3);
    let message = run.err_message();
    assert!(message.contains("src/b.ts"), "{message}");
    assert_eq!(
        repo.read("a.py"),
        PY,
        "the clean file is not written either"
    );
    assert!(!repo.report_exists(), "an aborted run writes no report");
}

#[test]
fn every_drifted_file_is_listed() {
    let repo = two_file_repo();
    fs::write(repo.abs("a.py"), "changed = True\n").unwrap();
    fs::write(repo.abs("src/b.ts"), "let changed = true;\n").unwrap();
    let message = repo.run(&[]).err_message();
    assert!(message.contains("2 selected file(s)"), "{message}");
    assert!(
        message.contains("a.py") && message.contains("src/b.ts"),
        "{message}"
    );
}

#[test]
fn a_deleted_file_is_drift() {
    let repo = two_file_repo();
    fs::remove_file(repo.abs("a.py")).unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 3);
    assert!(
        run.err_message().contains("deleted"),
        "{}",
        run.err_message()
    );
    assert_eq!(repo.read("src/b.ts"), TS);
}

#[cfg(unix)]
#[test]
fn a_symlink_at_a_planned_path_is_drift() {
    let repo = two_file_repo();
    let elsewhere = repo.abs("elsewhere.py");
    fs::rename(repo.abs("a.py"), &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, repo.abs("a.py")).unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 3);
    assert!(
        run.err_message().contains("symlink"),
        "{}",
        run.err_message()
    );
    assert_eq!(fs::read_to_string(&elsewhere).unwrap(), PY);
    assert_eq!(repo.read("src/b.ts"), TS);
}

#[cfg(unix)]
#[test]
fn a_hard_linked_file_is_written_with_a_warning() {
    let repo = two_file_repo();
    fs::hard_link(repo.abs("a.py"), repo.abs("a-link.py")).unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0);
    assert!(
        run.log
            .iter()
            .any(|line| line.contains("a.py") && line.contains("hard links")),
        "{:?}",
        run.log
    );
    assert_eq!(repo.read("a.py"), PY_APPLIED);
    assert_eq!(repo.read("a-link.py"), PY, "the rename splits the link");
}

#[test]
fn only_selects_a_clean_subset_past_a_drifted_file() {
    let repo = two_file_repo();
    fs::write(repo.abs("src/b.ts"), "let changed = true;\n").unwrap();
    let run = repo.run(&["a.py"]);
    assert_eq!(run.exit_code(), 0);
    assert_eq!(run.status("a.py"), ApplyStatus::Applied);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::NotSelected);
    assert_eq!(repo.read("a.py"), PY_APPLIED);
    assert_eq!(repo.read("src/b.ts"), "let changed = true;\n");
}

#[test]
fn only_paths_resolve_against_the_cwd() {
    let repo = two_file_repo();
    // Relative to the cwd (the root here), in any spelling.
    let run = repo.run(&["./src/../src/b.ts"]);
    assert_eq!(run.exit_code(), 0);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::Applied);
    assert_eq!(run.status("a.py"), ApplyStatus::NotSelected);
    assert_eq!(repo.read("a.py"), PY);
}

#[test]
fn only_naming_a_file_outside_the_artifact_is_a_usage_error() {
    let repo = two_file_repo();
    fs::write(repo.abs("other.py"), "x = 1\n").unwrap();
    for only in ["other.py", "missing.py", "src"] {
        let run = repo.run(&[only]);
        assert_eq!(run.err_code(), 1, "{only}");
    }
    assert_eq!(repo.read("a.py"), PY);
}

#[test]
fn an_invalid_path_on_an_unselected_failed_entry_aborts_with_exit_2() {
    let mut repo = two_file_repo();
    repo.add_failed("../escape.py");
    let run = repo.run(&["a.py"]);
    assert_eq!(run.err_code(), 2);
    assert!(
        run.err_message().contains("escape"),
        "{}",
        run.err_message()
    );
    assert_eq!(repo.read("a.py"), PY);
}

// ---------------------------------------------------------------------
// Firewall check (a): artifact/codebase range mismatch
// ---------------------------------------------------------------------

#[test]
fn a_record_whose_original_text_differs_aborts_with_exit_2() {
    let mut repo = two_file_repo();
    repo.record_mut("a.py", "# multiply").original = Some("# multiplx them".to_owned());
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 2);
    assert!(run.err_message().contains("a.py"), "{}", run.err_message());
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(
        repo.read("src/b.ts"),
        TS,
        "nothing is written, not even partially"
    );
}

#[test]
fn a_record_whose_range_matches_no_comment_aborts_with_exit_2() {
    let mut repo = two_file_repo();
    // Even a record that edits nothing is range-checked.
    let record = repo.record_mut("a.py", "# sum");
    record.decision = KeepOriginal;
    record.start += 1;
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 2);
    assert!(
        run.err_message().contains("matches no comment"),
        "{}",
        run.err_message()
    );
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(repo.read("src/b.ts"), TS);
}

#[test]
fn a_range_mismatch_outranks_drift() {
    let mut repo = two_file_repo();
    repo.record_mut("a.py", "# multiply").original = Some("# other".to_owned());
    fs::write(repo.abs("src/b.ts"), "let changed = true;\n").unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 2);
    let message = run.err_message();
    assert!(
        message.contains("a.py") && message.contains("src/b.ts"),
        "{message}"
    );
}

// ---------------------------------------------------------------------
// NEW-offset checks
// ---------------------------------------------------------------------

const SHEBANG_PY: &str = "#!/usr/bin/env python3\n# a note\nx = 1  # trailing note\n";

/// `SHEBANG_PY` planned with a REMOVE of the trailing note and one NEW
/// insertion at `offset`.
fn shebang_repo(offset: usize, position: Position) -> Repo {
    let mut repo = two_file_repo();
    repo.add("tool.py", SHEBANG_PY, &[("# trailing note", Remove, None)]);
    repo.push_new_at("tool.py", "N1", offset, position, New, "inserted");
    repo
}

/// Asserts the run aborts with exit 2, mentioning `fragment`, and writes
/// nothing.
fn assert_bad_new_offset(repo: &Repo, path: &str, original: &str, fragment: &str) {
    let run = repo.run(&[]);
    assert_eq!(run.err_code(), 2, "{fragment}");
    let message = run.err_message();
    assert!(message.contains(fragment), "{message}");
    assert_eq!(repo.read(path), original);
    assert_eq!(repo.read("a.py"), PY, "no other file is written");
}

#[test]
fn a_new_after_the_shebang_line_is_inserted() {
    let line_two = SHEBANG_PY.find('\n').unwrap() + 1;
    let repo = shebang_repo(line_two, Position::Leading);
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0, "{:?}", run.ok().result);
    assert_eq!(
        repo.read("tool.py"),
        "#!/usr/bin/env python3\n# inserted\n# a note\nx = 1\n"
    );
}

#[test]
fn a_new_at_offset_0_of_a_shebang_file_is_exit_2() {
    let repo = shebang_repo(0, Position::Leading);
    assert_bad_new_offset(&repo, "tool.py", SHEBANG_PY, "shebang");
}

#[test]
fn a_new_on_the_shebang_line_is_exit_2() {
    let newline = SHEBANG_PY.find('\n').unwrap();
    for offset in [5, newline] {
        let repo = shebang_repo(offset, Position::Trailing);
        assert_bad_new_offset(&repo, "tool.py", SHEBANG_PY, "shebang");
    }
}

#[test]
fn a_new_out_of_bounds_is_exit_2() {
    let mut repo = shebang_repo(SHEBANG_PY.len() + 5, Position::Leading);
    // Validation bounds NEW offsets by the recorded length; a hand edit
    // can raise both.
    repo.plan_mut("tool.py").byte_length += 10;
    assert_bad_new_offset(&repo, "tool.py", SHEBANG_PY, "out of bounds");
}

#[test]
fn a_new_off_a_char_boundary_is_exit_2() {
    let text = "s = \"é\"  # note\n";
    let mut repo = two_file_repo();
    repo.add("u.py", text, &[("# note", Remove, None)]);
    let inside = text.find('é').unwrap() + 1;
    repo.push_new_at("u.py", "N1", inside, Position::Trailing, New, "x");
    assert_bad_new_offset(&repo, "u.py", text, "character boundary");
}

#[test]
fn a_new_inside_a_scanned_comment_is_exit_2() {
    let inside = SHEBANG_PY.find("# a note").unwrap() + 3;
    let repo = shebang_repo(inside, Position::Trailing);
    assert_bad_new_offset(&repo, "tool.py", SHEBANG_PY, "inside the comment");
}

#[test]
fn a_new_before_or_inside_a_leading_bom_is_exit_2() {
    let text = "\u{feff}x = 1  # note\n";
    for offset in [0, 1] {
        let mut repo = two_file_repo();
        repo.add("bom.py", text, &[("# note", Remove, None)]);
        repo.push_new("bom.py", "N1", offset, New, "x");
        let fragment = if offset == 0 {
            "byte-order mark"
        } else {
            "character boundary"
        };
        assert_bad_new_offset(&repo, "bom.py", text, fragment);
    }
}

#[test]
fn a_new_right_after_a_bom_is_inserted_after_it() {
    let text = "\u{feff}x = 1  # note\n";
    let mut repo = Repo::new();
    repo.add("bom.py", text, &[("# note", Remove, None)]);
    repo.push_new("bom.py", "N1", '\u{feff}'.len_utf8(), New, "set x");
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0, "{:?}", run.ok().result);
    assert_eq!(repo.read("bom.py"), "\u{feff}# set x\nx = 1\n");
}

#[test]
fn a_new_inside_another_edits_deleted_range_is_exit_2() {
    // Removing the trailing note also deletes the spaces before it.
    let in_gap = SHEBANG_PY.find("x = 1").unwrap() + "x = 1 ".len();
    let repo = shebang_repo(in_gap, Position::Trailing);
    assert_bad_new_offset(&repo, "tool.py", SHEBANG_PY, "another edit deletes");
}

// ---------------------------------------------------------------------
// allowAdd
// ---------------------------------------------------------------------

#[test]
fn allow_add_is_read_from_the_config_snapshot() {
    let line_two = SHEBANG_PY.find('\n').unwrap() + 1;
    let mut repo = shebang_repo(line_two, Position::Leading);
    repo.allow_add = false;
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0);
    let file = run.file("tool.py");
    assert_eq!(file.status, ApplyStatus::Applied);
    assert_eq!(
        file.skipped_edits,
        [SkippedEdit {
            id: "N1".to_owned(),
            reason: SkippedEditReason::AllowAddOff,
        }]
    );
    assert_eq!(
        repo.read("tool.py"),
        "#!/usr/bin/env python3\n# a note\nx = 1\n"
    );
}

#[test]
fn a_new_is_offset_checked_only_when_it_inserts() {
    // With allowAdd off, a NEW at offset 0 of a shebang file makes no edit.
    let mut off = shebang_repo(0, Position::Leading);
    off.allow_add = false;
    assert_eq!(off.run(&[]).exit_code(), 0);
    // A downgraded (held) NEW passes untouched, wherever it points.
    for offset in [0, SHEBANG_PY.find("# a note").unwrap() + 3] {
        let mut held = two_file_repo();
        held.add("tool.py", SHEBANG_PY, &[("# trailing note", Remove, None)]);
        held.push_new("tool.py", "N1", offset, NeedsReview, "held");
        let run = held.run(&[]);
        assert_eq!(run.exit_code(), 0, "{offset}");
        assert_eq!(run.file("tool.py").skipped_edits, []);
        assert_eq!(
            held.read("tool.py"),
            "#!/usr/bin/env python3\n# a note\nx = 1\n"
        );
    }
}

// ---------------------------------------------------------------------
// Firewall check (b): hand edits routed into functional comments
// ---------------------------------------------------------------------

/// `(path, text, needle)`: a file whose comment starting with `needle`
/// the current inventory classifies `functional`.
const FUNCTIONAL_CASES: &[(&str, &str, &str)] = &[
    (
        "jsdoc.js",
        "/** @param {string} x */\nfunction f(x) {\n  return x;\n}\n",
        "/** @param",
    ),
    (
        "internal-doc.ts",
        "/** @internal */\nexport function f(): void {}\n",
        "/** @internal",
    ),
    (
        "internal-line.ts",
        "// @internal\nexport function g(): void {}\n",
        "// @internal",
    ),
    (
        "Deprecated.java",
        "class Deprecated {\n  /** @deprecated */\n  void f() {}\n}\n",
        "/** @deprecated",
    ),
    ("typed.py", "import os  # type: ignore\n", "# type: ignore"),
    (
        "tags.go",
        "//go:build linux\n\npackage main\n",
        "//go:build",
    ),
    (
        "hello_test.go",
        "package main\n\nimport \"fmt\"\n\nfunc ExampleHello() {\n\tfmt.Println(\"hello\")\n\t// Output: hello\n}\n",
        "// Output:",
    ),
    (
        "licensed.py",
        "# Copyright 2026 Example Corp.\n# Licensed under the MIT License.\n\nx = 1\n",
        "# Copyright",
    ),
    (
        "adjacent.ts",
        "// @ts-expect-error\n/** prose about the value */\nconst x = missing;\n",
        "/** prose about",
    ),
];

#[test]
fn hand_edits_into_functional_comments_are_rejected_by_the_firewall() {
    for &(path, text, needle) in FUNCTIONAL_CASES {
        let mut repo = two_file_repo();
        repo.add(path, text, &[]);
        relabel_remove(&mut repo, path, needle);
        let run = repo.run(&[]);
        assert_eq!(run.exit_code(), 4, "{path}: {:?}", run.ok().result);
        let file = run.file(path);
        assert_eq!(
            file.status,
            ApplyStatus::SkippedFirewall,
            "{path}: {file:?}"
        );
        let reason = file.reason.as_deref().unwrap();
        assert!(reason.contains("functional comment"), "{path}: {reason}");
        assert_eq!(repo.read(path), text, "{path} is untouched");
        // Per-file: the other files are still written.
        assert_eq!(run.status("a.py"), ApplyStatus::Applied, "{path}");
        assert_eq!(repo.read("a.py"), PY_APPLIED, "{path}");
        assert!(
            run.log
                .iter()
                .any(|line| line.contains("skipped-firewall") && line.contains(path)),
            "{path}: {:?}",
            run.log
        );
    }
}

#[test]
fn an_edit_beside_a_functional_comment_is_not_a_firewall_hit() {
    // The removal's line deletion ends exactly where the directive starts.
    let text = "# explain the import\n# type: ignore\nimport os\n";
    let mut repo = Repo::new();
    repo.add("beside.py", text, &[("# explain the import", Remove, None)]);
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0, "{:?}", run.ok().result);
    assert_eq!(repo.read("beside.py"), "# type: ignore\nimport os\n");
}

// ---------------------------------------------------------------------
// Per-file outcomes
// ---------------------------------------------------------------------

/// The production edit computation, for hooks that wrap it.
fn real_edit(text: &str, plan: &FilePlan, inventory: &Inventory) -> Result<String, Error> {
    let computed = compute_edits(
        text,
        plan,
        inventory,
        adapter_for(plan.language),
        EditOpts { allow_add: true },
    )?;
    apply_edits(text, &computed.edits)
}

/// A hook running the real computation everywhere except `a.py`, where
/// `on_a` decides.
fn hook_on_a_py(
    on_a: impl Fn(&str, &FilePlan, &Inventory) -> Result<String, Error> + 'static,
) -> ApplyHooks {
    ApplyHooks {
        apply_file: Some(Box::new(move |text, plan, inventory| {
            if plan.path == "a.py" {
                on_a(text, plan, inventory)
            } else {
                real_edit(text, plan, inventory)
            }
        })),
    }
}

#[test]
fn a_tampered_edit_is_rejected_by_the_real_verifier() {
    let repo = two_file_repo();
    let hooks = hook_on_a_py(|text, plan, inventory| {
        Ok(real_edit(text, plan, inventory)?.replace("a + b", "a - b"))
    });
    let run = repo.run_with(&[], Some(REPORT), hooks);
    assert_eq!(run.exit_code(), 4);
    let file = run.file("a.py");
    assert_eq!(file.status, ApplyStatus::SkippedVerify);
    let reason = file.reason.as_deref().unwrap();
    assert!(reason.contains("semantic verification failed"), "{reason}");
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::Applied);
    assert_eq!(repo.read("src/b.ts"), TS_APPLIED);
    assert!(repo.report_exists(), "per-file failures still get a report");
}

#[test]
fn an_invariant_failure_skips_only_that_file() {
    let repo = two_file_repo();
    let hooks = hook_on_a_py(|_, _, _| Err(Error::Invariant("canary".to_owned())));
    let run = repo.run_with(&[], Some(REPORT), hooks);
    assert_eq!(run.exit_code(), 7);
    let file = run.file("a.py");
    assert_eq!(file.status, ApplyStatus::FailedInvariant);
    assert!(file.reason.as_deref().unwrap().contains("canary"));
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(repo.read("src/b.ts"), TS_APPLIED);
}

#[test]
fn a_file_modified_between_phase_a_and_its_rename_is_failed_io() {
    let repo = two_file_repo();
    let target = repo.abs("a.py");
    // The hook runs in Phase A, after the drift check; an editor saving
    // the file then is what Phase B's re-hash must catch.
    let hooks = hook_on_a_py(move |text, plan, inventory| {
        fs::write(&target, "saved = 'by an editor'\n").unwrap();
        real_edit(text, plan, inventory)
    });
    let run = repo.run_with(&[], Some(REPORT), hooks);
    assert_eq!(run.exit_code(), 6);
    let file = run.file("a.py");
    assert_eq!(file.status, ApplyStatus::FailedIo);
    let reason = file.reason.as_deref().unwrap();
    assert!(reason.contains("changed during apply"), "{reason}");
    assert_eq!(
        repo.read("a.py"),
        "saved = 'by an editor'\n",
        "the save survives"
    );
    assert_eq!(repo.read("src/b.ts"), TS_APPLIED);
    let hint = run.ok().result.partial_apply_hint.as_deref();
    assert_eq!(hint, Some("munge-comments apply plan.json --only a.py"));
    assert!(
        run.log
            .iter()
            .any(|line| line.contains("munge-comments apply plan.json --only a.py")),
        "the hint is logged: {:?}",
        run.log
    );
    let report = repo.read(REPORT);
    assert!(
        report.contains("munge-comments apply plan.json --only a.py"),
        "the hint is in the report:\n{report}"
    );
}

#[test]
fn no_hint_without_a_written_file() {
    let mut repo = Repo::new();
    repo.add("a.py", PY, &[("# add the numbers", Remove, None)]);
    let target = repo.abs("a.py");
    let hooks = hook_on_a_py(move |text, plan, inventory| {
        fs::write(&target, "saved = 'by an editor'\n").unwrap();
        real_edit(text, plan, inventory)
    });
    let run = repo.run_with(&[], Some(REPORT), hooks);
    assert_eq!(run.exit_code(), 6);
    assert_eq!(run.status("a.py"), ApplyStatus::FailedIo);
    assert_eq!(
        run.ok().result.partial_apply_hint,
        None,
        "nothing was written, so a plain re-run is the recovery"
    );
}

#[test]
fn invariant_failures_stay_out_of_the_hint() {
    let mut repo = two_file_repo();
    repo.add("c.py", PY, &[("# sum", Remove, None)]);
    let target = repo.abs("c.py");
    let hooks = ApplyHooks {
        apply_file: Some(Box::new(move |text, plan, inventory| {
            match plan.path.as_str() {
                "a.py" => Err(Error::Invariant("canary".to_owned())),
                "c.py" => {
                    fs::write(&target, "saved = 'by an editor'\n").unwrap();
                    real_edit(text, plan, inventory)
                }
                _ => real_edit(text, plan, inventory),
            }
        })),
    };
    let run = repo.run_with(&[], Some(REPORT), hooks);
    assert_eq!(run.exit_code(), 7);
    assert_eq!(run.status("a.py"), ApplyStatus::FailedInvariant);
    assert_eq!(run.status("c.py"), ApplyStatus::FailedIo);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::Applied);
    assert_eq!(
        run.ok().result.partial_apply_hint.as_deref(),
        Some("munge-comments apply plan.json --only c.py")
    );
}

// ---------------------------------------------------------------------
// Dry runs
// ---------------------------------------------------------------------

const TS_DIFF: &str = "\
--- a/src/b.ts
+++ b/src/b.ts
@@ -1,4 +1,3 @@
-// increments the counter
 export function inc(n: number): number {
   return n + 1;
 }
";

#[test]
fn a_dry_run_prints_the_diffs_and_writes_nothing() {
    let repo = two_file_repo();
    let (run, stdout) = repo.run_dry(ApplyHooks::default());
    assert_eq!(run.exit_code(), 0, "{:?}", run.ok().result);
    assert_eq!(run.status("a.py"), ApplyStatus::DryRun);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::DryRun);
    assert!(
        stdout.starts_with("--- a/a.py\n+++ b/a.py\n"),
        "diffs follow artifact order:\n{stdout}"
    );
    assert!(stdout.ends_with(TS_DIFF), "{stdout}");
    assert!(stdout.contains("-    # add the numbers\n"), "{stdout}");
    assert!(stdout.contains("+    # Multiply a by b.\n"), "{stdout}");
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(repo.read("src/b.ts"), TS);
    assert!(!repo.report_exists(), "a dry run writes no report");
    assert!(
        run.log.iter().any(|line| line == "apply: 2 dry-run"),
        "{:?}",
        run.log
    );
    assert_eq!(run.ok().result.partial_apply_hint, None);
}

#[test]
fn a_dry_run_with_a_verify_failing_file_prints_the_others_and_returns_4() {
    let repo = two_file_repo();
    let hooks = hook_on_a_py(|text, plan, inventory| {
        Ok(real_edit(text, plan, inventory)?.replace("a + b", "a - b"))
    });
    let (run, stdout) = repo.run_dry(hooks);
    assert_eq!(run.exit_code(), 4, "the exit code a real run would return");
    assert_eq!(run.status("a.py"), ApplyStatus::SkippedVerify);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::DryRun);
    assert_eq!(stdout, TS_DIFF, "only would-write files are diffed");
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(repo.read("src/b.ts"), TS);
    assert!(!repo.report_exists());
}

#[test]
fn a_dry_run_creates_no_report_directory() {
    let repo = two_file_repo();
    repo.write_artifact();
    let log = CapturingLog::new();
    let mut stdout: Vec<u8> = Vec::new();
    let outcome = run_apply(
        ApplyRequest {
            artifact: PathBuf::from("plan.json"),
            only: Vec::new(),
            dry_run: true,
            report_path: Some(PathBuf::from("reports/apply.md")),
        },
        ApplyDeps {
            log: &log,
            stdout: &mut stdout,
            cwd: repo.root(),
            hooks: ApplyHooks::default(),
        },
    )
    .expect("the dry run completes");
    assert_eq!(outcome.exit_code, 0);
    assert!(!repo.abs("reports").exists());
}

#[test]
fn a_hooked_dry_run_still_diffs_the_hooks_text() {
    // The hook returns text only; the diff is rendered from the edit
    // spanning the difference.
    let repo = two_file_repo();
    let (run, stdout) = repo.run_dry(hook_on_a_py(real_edit));
    assert_eq!(run.exit_code(), 0);
    assert_eq!(run.status("a.py"), ApplyStatus::DryRun);
    assert!(
        stdout.starts_with("--- a/a.py\n+++ b/a.py\n@@ -1,"),
        "{stdout}"
    );
    assert!(stdout.contains("-    # add the numbers\n"), "{stdout}");
    assert!(stdout.contains("+    # Multiply a by b.\n"), "{stdout}");
    assert!(stdout.ends_with(TS_DIFF), "{stdout}");
}

#[cfg(unix)]
#[test]
fn an_unreadable_source_file_is_failed_io_with_exit_6() {
    use std::os::unix::fs::PermissionsExt;
    let repo = two_file_repo();
    let target = repo.abs("a.py");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&target).is_ok() {
        // Privileged users bypass file modes; the precondition cannot be
        // set up.
        eprintln!("skipped: file modes do not restrict this user");
        return;
    }
    let run = repo.run(&[]);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(run.exit_code(), 6, "{:?}", run.ok().result);
    assert_eq!(run.status("a.py"), ApplyStatus::FailedIo);
    assert_eq!(repo.read("a.py"), PY);
    assert_eq!(run.status("src/b.ts"), ApplyStatus::Applied);
}

#[cfg(unix)]
#[test]
fn an_executable_script_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let mut repo = Repo::new();
    repo.add("tool.py", SHEBANG_PY, &[("# a note", Remove, None)]);
    let target = repo.abs("tool.py");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let run = repo.run(&[]);
    assert_eq!(run.exit_code(), 0);
    assert_eq!(
        repo.read("tool.py"),
        "#!/usr/bin/env python3\nx = 1  # trailing note\n"
    );
    let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

// ---------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------

fn result_of(statuses: &[ApplyStatus]) -> ApplyResult {
    ApplyResult {
        files: statuses
            .iter()
            .enumerate()
            .map(|(index, status)| ApplyFileResult {
                path: format!("f{index}.py"),
                status: *status,
                reason: None,
                skipped_edits: Vec::new(),
            })
            .collect(),
        partial_apply_hint: None,
    }
}

#[test]
fn exit_code_for_follows_the_precedence_table() {
    use ApplyStatus::*;
    assert_eq!(exit_code_for(&result_of(&[])), 0);
    assert_eq!(
        exit_code_for(&result_of(&[
            Applied,
            Unchanged,
            DryRun,
            NotSelected,
            SkippedPlanStatus
        ])),
        0,
        "these statuses are exit-code-neutral"
    );
    assert_eq!(exit_code_for(&result_of(&[Applied, FailedIo])), 6);
    assert_eq!(exit_code_for(&result_of(&[FailedIo, SkippedVerify])), 4);
    assert_eq!(exit_code_for(&result_of(&[SkippedFirewall, FailedIo])), 4);
    assert_eq!(
        exit_code_for(&result_of(&[FailedIo, SkippedFirewall, FailedInvariant])),
        7
    );
}
