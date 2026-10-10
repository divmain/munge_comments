//! End-to-end `apply`: artifacts planned in-test (with
//! `common::file_plan_from_inventory`, hashes computed from the bytes on
//! disk) over a temp copy of `tests/fixtures/apply-e2e` — Python,
//! TypeScript, Go, Rust and JavaScript — applied through the CLI's
//! `run()`, or through `run_apply` where a test needs an `apply_file`
//! hook.

#![cfg(feature = "cli")]

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use munge_comments::apply::edits::{EditOpts, apply_edits, compute_edits};
use munge_comments::apply::types::ApplyStatus;
use munge_comments::artifact::types::{CommentRecord, DecidedBy, Decision, FilePlan};
use munge_comments::cli::{Io, run};
use munge_comments::comments::types::{Anchor, Classification, CommentKind, Position};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{
    ApplyDeps, ApplyHooks, ApplyOutcome, ApplyRequest, CapturingLog, Error, Inventory,
    InventoryComment, run_apply,
};
use tempfile::TempDir;

use common::SharedBuf;

/// The fixture tree every test copies.
const TREE: &str = "apply-e2e";

/// Every file of [`TREE`], in artifact (path) order.
const FILES: [&str; 5] = [
    "go/count.go",
    "js/asi.js",
    "py/calc.py",
    "rs/lib.rs",
    "ts/greet.ts",
];

/// The default report name of `apply`.
const REPORT: &str = "munge-comments-apply-report.md";

/// A temp copy of [`TREE`] and the plan being built for it.
struct E2e {
    tree: TempDir,
    files: Vec<FilePlan>,
    allow_add: bool,
}

impl E2e {
    /// A fresh copy, planned with [`common::remove_prose`] for every file.
    fn new() -> E2e {
        let mut e2e = E2e {
            tree: common::copy_fixture_tree(TREE),
            files: Vec::new(),
            allow_add: false,
        };
        for path in FILES {
            e2e.plan(path, common::remove_prose);
        }
        e2e
    }

    fn root(&self) -> &Path {
        self.tree.path()
    }

    fn abs(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.abs(path)).unwrap_or_else(|err| panic!("{path}: {err}"))
    }

    /// The pristine fixture text of `path`.
    fn pristine(path: &str) -> String {
        fs::read_to_string(common::fixture_path(Path::new(TREE).join(path))).unwrap()
    }

    /// (Re)plans `path` from its bytes on disk now.
    fn plan(
        &mut self,
        path: &str,
        decide: impl Fn(&InventoryComment) -> (Decision, Option<String>),
    ) {
        let plan = common::plan_on_disk(self.root(), path, decide);
        self.files.retain(|file| file.path != path);
        self.files.push(plan);
    }

    fn plan_mut(&mut self, path: &str) -> &mut FilePlan {
        self.files
            .iter_mut()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("no plan for {path}"))
    }

    /// The scanned record of `path` whose original text starts with
    /// `needle`.
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

    /// Writes the artifact at `plan.json` in the tree root.
    fn write_artifact(&self) {
        self.write_artifact_at(&self.abs("plan.json"), ".");
    }

    fn write_artifact_at(&self, artifact: &Path, root: &str) {
        common::write_plan_artifact(artifact, root, self.files.clone(), self.allow_add);
    }

    /// Writes the artifact and runs `munge-comments apply plan.json
    /// <args>` from the tree root.
    async fn cli(&self, args: &[&str]) -> Cli {
        self.write_artifact();
        let mut argv = vec!["apply", "plan.json"];
        argv.extend_from_slice(args);
        run_cli(self.root(), &argv).await
    }

    /// Writes the artifact and runs `run_apply` on it with `hooks` and the
    /// default report.
    fn run_hooked(&self, hooks: ApplyHooks) -> ApplyOutcome {
        self.write_artifact();
        let log = CapturingLog::new();
        let mut stdout: Vec<u8> = Vec::new();
        run_apply(
            ApplyRequest {
                artifact: PathBuf::from("plan.json"),
                only: Vec::new(),
                dry_run: false,
                report_path: Some(PathBuf::from(REPORT)),
            },
            ApplyDeps {
                log: &log,
                stdout: &mut stdout,
                cwd: self.root(),
                hooks,
            },
        )
        .unwrap_or_else(|err| panic!("the run aborted: {err}"))
    }

    /// Asserts every file is byte-identical to the fixture.
    fn assert_pristine(&self) {
        for path in FILES {
            assert_eq!(self.read(path), E2e::pristine(path), "{path} was modified");
        }
    }
}

/// One CLI run's exit code and streams.
struct Cli {
    code: u8,
    out: String,
    err: String,
}

async fn run_cli(cwd: &Path, args: &[&str]) -> Cli {
    let out = SharedBuf::new();
    let err = SharedBuf::new();
    let io = Io::new(
        Box::new(out.clone()),
        Box::new(err.clone()),
        cwd.to_path_buf(),
        HashMap::new(),
    );
    let argv = std::iter::once("munge-comments").chain(args.iter().copied());
    let code = run(argv, io).await;
    Cli {
        code,
        out: out.contents(),
        err: err.contents(),
    }
}

/// The production edit computation, for hooks that wrap it.
fn real_edit(text: &str, plan: &FilePlan, inventory: &Inventory) -> Result<String, Error> {
    let computed = compute_edits(
        text,
        plan,
        inventory,
        adapter_for(plan.language),
        EditOpts { allow_add: false },
    )?;
    apply_edits(text, &computed.edits)
}

/// A hook running the real computation everywhere except `path`, where
/// `on_path` decides.
fn hook_on(
    path: &'static str,
    on_path: impl Fn(&str, &FilePlan, &Inventory) -> Result<String, Error> + 'static,
) -> ApplyHooks {
    ApplyHooks {
        apply_file: Some(Box::new(move |text, plan, inventory| {
            if plan.path == path {
                on_path(text, plan, inventory)
            } else {
                real_edit(text, plan, inventory)
            }
        })),
    }
}

fn status_of(outcome: &ApplyOutcome, path: &str) -> ApplyStatus {
    outcome
        .result
        .files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("no result for {path}"))
        .status
}

/// Each file after the clean remove-the-prose apply.
fn applied(path: &str) -> &'static str {
    match path {
        "go/count.go" => {
            "package count\n\nfunc Count(xs []int) int {\n\tn := 0\n\tfor range xs {\n\t\tn++\n\t}\n\treturn n\n}\n"
        }
        // The removed span held the only line break between `return`
        // and `x`, so one line terminator stays in its place (ASI).
        "js/asi.js" => "export function pick(x) {\n  return\nx;\n}\n",
        "py/calc.py" => {
            "\"\"\"Small calculator helpers.\"\"\"\nimport os  # noqa: F401\n\n\ndef add(a, b):\n    return a + b\n\n\ndef scale(x, k):\n    return x * k\n"
        }
        "rs/lib.rs" => "pub fn twice(x: i32) -> i32 {\n    x * 2\n}\n",
        "ts/greet.ts" => {
            "export function greet(name: string): string {\n  return \"hello, \" + name;\n}\n"
        }
        _ => panic!("not a fixture file: {path}"),
    }
}

// ---------------------------------------------------------------------
// Clean runs
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_clean_apply_writes_every_file_and_the_report() {
    let e2e = E2e::new();
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    assert_eq!(cli.out, "");
    assert!(cli.err.contains("apply: 5 applied"), "{}", cli.err);
    for path in FILES {
        assert_eq!(e2e.read(path), applied(path), "{path}");
    }
    let report = e2e.read(REPORT);
    for path in FILES {
        assert!(report.contains(path), "{report}");
    }
}

#[tokio::test]
async fn a_hand_authored_remove_on_a_multiline_return_comment_keeps_its_newline() {
    let mut e2e = E2e::new();
    // Only the JS file, kept as is but for one record decided by hand.
    e2e.files.retain(|file| file.path == "js/asi.js");
    e2e.plan("js/asi.js", |_| (Decision::KeepOriginal, None));
    let record = e2e.record_mut("js/asi.js", "/*");
    record.decision = Decision::Remove;
    record.decided_by = DecidedBy::Human;
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    assert_eq!(
        e2e.read("js/asi.js"),
        "export function pick(x) {\n  return\nx;\n}\n"
    );
}

#[tokio::test]
async fn a_dry_run_passes_git_apply_check_and_matches_the_real_run() {
    let e2e = E2e::new();
    let cli = e2e.cli(&["--dry-run"]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    e2e.assert_pristine();
    assert!(!e2e.abs(REPORT).exists(), "a dry run writes no report");
    for path in FILES {
        assert!(cli.out.contains(&format!("--- a/{path}\n")), "{}", cli.out);
    }

    // Apply the diff to a pristine copy with git.
    let copy = common::copy_fixture_tree(TREE);
    let git = |args: &[&str]| {
        use std::io::Write as _;
        let mut child = std::process::Command::new("git")
            .args(args)
            .current_dir(copy.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(cli.out.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} rejected the diff: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            cli.out
        );
    };
    git(&["apply", "--check", "-"]);
    git(&["apply", "-"]);
    for path in FILES {
        let patched = fs::read_to_string(copy.path().join(path)).unwrap();
        assert_eq!(
            patched,
            applied(path),
            "the diff of {path} is the real edit"
        );
    }
}

#[tokio::test]
async fn only_skips_a_drifted_unselected_file_and_writes_the_selected_one() {
    let e2e = E2e::new();
    fs::write(e2e.abs("ts/greet.ts"), "export const drifted = 1;\n").unwrap();
    let cli = e2e.cli(&["--only", "py/calc.py"]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    assert_eq!(e2e.read("py/calc.py"), applied("py/calc.py"));
    assert_eq!(e2e.read("ts/greet.ts"), "export const drifted = 1;\n");
    assert_eq!(e2e.read("go/count.go"), E2e::pristine("go/count.go"));
}

#[tokio::test]
async fn a_drifted_file_without_actionable_edits_does_not_abort() {
    let mut e2e = E2e::new();
    e2e.plan("rs/lib.rs", |_| (Decision::KeepOriginal, None));
    fs::write(e2e.abs("rs/lib.rs"), "pub fn drifted() {}\n").unwrap();
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    assert!(cli.err.contains("4 applied, 1 unchanged"), "{}", cli.err);
    assert_eq!(e2e.read("rs/lib.rs"), "pub fn drifted() {}\n");
    assert_eq!(e2e.read("ts/greet.ts"), applied("ts/greet.ts"));
}

// ---------------------------------------------------------------------
// Artifacts outside the source tree
// ---------------------------------------------------------------------

/// Writes the artifact into a sibling temp directory, its `root` reaching
/// the tree through `..`; returns that directory.
fn artifact_beside(e2e: &E2e) -> TempDir {
    let plans = common::tempdir();
    assert_eq!(
        plans.path().parent(),
        e2e.root().parent(),
        "temp directories share a parent"
    );
    let name = e2e.root().file_name().unwrap().to_str().unwrap();
    e2e.write_artifact_at(&plans.path().join("plan.json"), &format!("../{name}"));
    plans
}

#[tokio::test]
async fn an_artifact_outside_the_tree_applies_through_a_dotdot_root() {
    let e2e = E2e::new();
    let plans = artifact_beside(&e2e);
    let cli = run_cli(plans.path(), &["apply", "plan.json"]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    for path in FILES {
        assert_eq!(e2e.read(path), applied(path), "{path}");
    }
    assert!(
        plans.path().join(REPORT).is_file(),
        "the report lands in the cwd"
    );
}

#[tokio::test]
async fn an_absolute_non_canonical_artifact_path_applies() {
    let e2e = E2e::new();
    let plans = artifact_beside(&e2e);
    let artifact = plans.path().join("plan.json");
    if cfg!(target_os = "macos") {
        // macOS temp directories live under the `/var` -> `/private/var`
        // symlink, so this path is absolute but not canonical.
        assert!(artifact.starts_with("/var/"), "{}", artifact.display());
        assert_ne!(fs::canonicalize(&artifact).unwrap(), artifact);
    }
    // Run from the tree, not the artifact's directory.
    let cli = run_cli(e2e.root(), &["apply", artifact.to_str().unwrap()]).await;
    assert_eq!(cli.code, 0, "{}", cli.err);
    for path in FILES {
        assert_eq!(e2e.read(path), applied(path), "{path}");
    }
}

/// Re-points `py/calc.py`'s plan at `escape(e2e)` (which may set up the
/// tree; the plan still describes
/// the fixture's bytes, which `outside` holds a copy of) and asserts the
/// run aborts with exit 2 before anything is written.
async fn assert_escape_aborts(escape: impl Fn(&E2e) -> String, outside: &Path) {
    let mut e2e = E2e::new();
    let escape = escape(&e2e);
    e2e.plan_mut("py/calc.py").path = escape.clone();
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 2, "{escape}: {}", cli.err);
    assert!(cli.err.contains("artifact error"), "{escape}: {}", cli.err);
    e2e.assert_pristine();
    assert_eq!(
        fs::read_to_string(outside.join("calc.py")).unwrap(),
        E2e::pristine("py/calc.py"),
        "{escape}: the outside file is untouched"
    );
    assert!(!e2e.abs(REPORT).exists());
}

/// A directory outside every tree holding a copy of `py/calc.py`.
fn outside_copy() -> TempDir {
    let outside = common::tempdir();
    fs::copy(
        common::fixture_path("apply-e2e/py/calc.py"),
        outside.path().join("calc.py"),
    )
    .unwrap();
    outside
}

#[tokio::test]
async fn an_absolute_artifact_path_aborts_with_exit_2() {
    let outside = outside_copy();
    let target = outside.path().join("calc.py");
    assert_escape_aborts(|_| target.to_str().unwrap().to_owned(), outside.path()).await;
}

#[tokio::test]
async fn a_dotdot_artifact_path_aborts_with_exit_2() {
    let outside = outside_copy();
    let name = outside
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_escape_aborts(|_| format!("../{name}/calc.py"), outside.path()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_artifact_path_through_a_symlinked_directory_aborts_with_exit_2() {
    let outside = outside_copy();
    let link_to = outside.path().to_path_buf();
    assert_escape_aborts(
        move |e2e| {
            std::os::unix::fs::symlink(&link_to, e2e.abs("link")).unwrap();
            "link/calc.py".to_owned()
        },
        outside.path(),
    )
    .await;
}

// ---------------------------------------------------------------------
// Run-level aborts: nothing is written
// ---------------------------------------------------------------------

#[tokio::test]
async fn drift_aborts_the_whole_apply() {
    let e2e = E2e::new();
    let drifted = format!("{}// a later edit\n", E2e::pristine("rs/lib.rs"));
    fs::write(e2e.abs("rs/lib.rs"), &drifted).unwrap();
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 3, "{}", cli.err);
    assert!(cli.err.contains("rs/lib.rs"), "{}", cli.err);
    for path in FILES.iter().filter(|path| **path != "rs/lib.rs") {
        assert_eq!(e2e.read(path), E2e::pristine(path), "{path}");
    }
    assert_eq!(e2e.read("rs/lib.rs"), drifted);
    assert!(!e2e.abs(REPORT).exists());
}

#[tokio::test]
async fn a_range_tampered_record_in_the_last_file_aborts_before_any_write() {
    let mut e2e = E2e::new();
    assert_eq!(FILES.last(), Some(&"ts/greet.ts"), "the last selected file");
    // Shifted by one byte: still well-formed, but no longer the comment.
    let record = e2e.record_mut("ts/greet.ts", "// join the parts");
    record.start += 1;
    record.end += 1;
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 2, "{}", cli.err);
    assert!(
        cli.err.contains("ts/greet.ts") && cli.err.contains("matches no comment"),
        "{}",
        cli.err
    );
    e2e.assert_pristine();
    assert!(!e2e.abs(REPORT).exists());
}

/// A NEW-origin record decided `NEW` at `offset`.
fn new_record(id: &str, offset: usize, text: &str) -> CommentRecord {
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
        decision: Decision::New,
        final_comment: Some(text.to_owned()),
        confidence: 1.0,
        decision_rationale: None,
        decided_by: DecidedBy::Human,
    }
}

#[tokio::test]
async fn a_hand_edited_new_out_of_bounds_aborts_with_exit_2() {
    let mut e2e = E2e::new();
    e2e.allow_add = true;
    let len = E2e::pristine("py/calc.py").len();
    e2e.plan_mut("py/calc.py")
        .comments
        .push(new_record("N1", len + 10, "added"));
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 2, "{}", cli.err);
    assert!(
        cli.err.contains("N1") && cli.err.contains("beyond the file length"),
        "{}",
        cli.err
    );
    e2e.assert_pristine();
}

// ---------------------------------------------------------------------
// Per-file failures: only that file is skipped
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_firewall_rejection_skips_the_file_with_exit_4() {
    let mut e2e = E2e::new();
    // Route a REMOVE into the functional `# noqa` by relabeling it.
    let record = e2e.record_mut("py/calc.py", "# noqa");
    record.classification = Classification::Prose;
    record.tags.clear();
    record.decision = Decision::Remove;
    let cli = e2e.cli(&[]).await;
    assert_eq!(cli.code, 4, "{}", cli.err);
    assert!(
        cli.err.contains("skipped-firewall py/calc.py"),
        "{}",
        cli.err
    );
    assert_eq!(e2e.read("py/calc.py"), E2e::pristine("py/calc.py"));
    for path in FILES.iter().filter(|path| **path != "py/calc.py") {
        assert_eq!(e2e.read(path), applied(path), "{path}");
    }
    assert!(e2e.abs(REPORT).is_file());
}

#[test]
fn a_tampered_apply_file_hook_fails_verification_with_exit_4() {
    let e2e = E2e::new();
    let hooks = hook_on("go/count.go", |text, plan, inventory| {
        Ok(real_edit(text, plan, inventory)?.replace("n := 0", "n := 1"))
    });
    let outcome = e2e.run_hooked(hooks);
    assert_eq!(outcome.exit_code, 4);
    assert_eq!(
        status_of(&outcome, "go/count.go"),
        ApplyStatus::SkippedVerify
    );
    assert_eq!(e2e.read("go/count.go"), E2e::pristine("go/count.go"));
    for path in FILES.iter().filter(|path| **path != "go/count.go") {
        assert_eq!(status_of(&outcome, path), ApplyStatus::Applied, "{path}");
        assert_eq!(e2e.read(path), applied(path), "{path}");
    }
}

#[test]
fn an_invariant_failure_skips_the_file_with_exit_7() {
    let e2e = E2e::new();
    let hooks = hook_on("rs/lib.rs", |_, _, _| {
        Err(Error::Invariant("canary".to_owned()))
    });
    let outcome = e2e.run_hooked(hooks);
    assert_eq!(outcome.exit_code, 7);
    assert_eq!(
        status_of(&outcome, "rs/lib.rs"),
        ApplyStatus::FailedInvariant
    );
    assert_eq!(e2e.read("rs/lib.rs"), E2e::pristine("rs/lib.rs"));
    for path in FILES.iter().filter(|path| **path != "rs/lib.rs") {
        assert_eq!(e2e.read(path), applied(path), "{path}");
    }
    assert_eq!(
        outcome.result.partial_apply_hint, None,
        "invariant failures are not retried"
    );
}
