//! Call #1 orchestration (`engine::regenerate`), driven by `FakeLlm`.
//!
//! Redact mode: the zero-call skip, targets, the id join, `unknown-id`
//! ownership, unreturned ids, chunked files, truncation split-and-retry and
//! adversarial replies. Remove mode: alignment through synthesized anchors
//! (incl. `end_of_file`), chunked alignment across member-only sub-chunks,
//! the placeholder-collision fallback, NEW records, truncation re-chunking
//! and adversarial replies.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::sync::{Mutex, PoisonError};

use futures::future::BoxFuture;
use munge_comments::artifact::types::{
    DecidedBy, Decision, Difficulty, FileWarning, FileWarningKind,
};
use munge_comments::comments::sanitize::comment_body;
use munge_comments::comments::types::{Classification, CommentKind, Position};
use munge_comments::engine::heuristics::REDUCED_SET;
use munge_comments::engine::{FileInput, MAX_SPLIT_DEPTH, RegenOutput, regenerate_file};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::chunk::chunk_file;
use munge_comments::llm::{CallKind, ChatRequest, LlmMeta, RawResponse, Role};
use munge_comments::{
    CapturingLog, DraftRecord, Error, FakeLlm, FileRef, LanguageId, LlmClient, LlmErrorReason,
    Mode, PlanConfig, Route, Scenario, build_draft_records, inventory_file, route,
};
use serde_json::{Value, json};

/// Rewrites the content of a successful reply.
type Edit = Box<dyn Fn(&LlmMeta, String) -> String + Send + Sync>;

/// A [`FakeLlm`] whose successful replies pass through an edit (to model
/// adversarial output), recording every request it sees.
struct Tampered {
    fake: FakeLlm,
    edit: Edit,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Tampered {
    fn new(scenario: Scenario, edit: Edit) -> Tampered {
        Tampered {
            fake: FakeLlm::new(scenario),
            edit,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The fake, untouched.
    fn honest(scenario: Scenario) -> Tampered {
        Tampered::new(scenario, Box::new(|_, content| content))
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl LlmClient for Tampered {
    fn complete_json<'a>(
        &'a self,
        req: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<RawResponse, Error>> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(req.clone());
            let mut raw = self.fake.complete_json(req).await?;
            if raw.finish_reason.as_deref() != Some("length") {
                raw.content = raw.content.map(|content| (self.edit)(&req.meta, content));
            }
            Ok(raw)
        })
    }
}

/// Appends `extra` comments to a redact-mode reply.
fn with_extra_comments(content: &str, extra: &[Value]) -> String {
    let mut reply: Value = serde_json::from_str(content).expect("the fake replies with JSON");
    let comments = reply["comments"]
        .as_array_mut()
        .expect("a redact reply has comments");
    comments.extend(extra.iter().cloned());
    reply.to_string()
}

/// Drops the answer for `id` from a redact-mode reply.
fn without_comment(content: &str, id: &str) -> String {
    let mut reply: Value = serde_json::from_str(content).expect("the fake replies with JSON");
    reply["comments"]
        .as_array_mut()
        .expect("a redact reply has comments")
        .retain(|comment| comment["id"] != id);
    reply.to_string()
}

fn answer(id: &str, comment: &str) -> Value {
    json!({
        "id": id,
        "wouldYouCommentHere": true,
        "comment": comment,
        "difficulty": "MEDIUM",
        "rationale": "injected",
        "missingContext": null,
    })
}

fn scenario(value: Value) -> Scenario {
    serde_json::from_value(value).expect("a valid scenario")
}

fn config(mode: Mode, max_file_chars: u64) -> PlanConfig {
    PlanConfig {
        mode,
        model: "test-model".to_owned(),
        base_url: None,
        allow_add: false,
        similarity_threshold: 0.6,
        max_file_chars,
        concurrency: 1,
        offline: false,
        paths: vec![".".to_owned()],
        include: None,
        exclude: Vec::new(),
    }
}

/// Runs call #1 in redact mode over `text` (inventoried as `path` under
/// `language`).
async fn regen(
    path: &str,
    language: LanguageId,
    text: &str,
    max_file_chars: u64,
    llm: &dyn LlmClient,
) -> (Result<RegenOutput, Error>, CapturingLog) {
    regen_in(Mode::Redact, path, language, text, max_file_chars, llm).await
}

/// Runs call #1 in `mode` over `text` (inventoried as `path` under
/// `language`).
async fn regen_in(
    mode: Mode,
    path: &str,
    language: LanguageId,
    text: &str,
    max_file_chars: u64,
    llm: &dyn LlmClient,
) -> (Result<RegenOutput, Error>, CapturingLog) {
    let inventory = inventory_file(
        text,
        FileRef {
            language,
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        },
    )
    .expect("test sources inventory cleanly");
    let log = CapturingLog::new();
    let input = FileInput {
        path,
        language,
        text,
        inventory: &inventory,
    };
    let output = regenerate_file(input, &config(mode, max_file_chars), llm, &log).await;
    (output, log)
}

/// The ids `build_draft_records` leaves undecided.
fn undecided_ids(text: &str, language: LanguageId, file_name: &str) -> BTreeSet<String> {
    let inventory = inventory_file(
        text,
        FileRef {
            language,
            file_name: file_name.to_owned(),
        },
    )
    .expect("test sources inventory cleanly");
    build_draft_records(&inventory, adapter_for(language))
        .into_iter()
        .filter(|record| record.decision.is_none())
        .map(|record| record.id)
        .collect()
}

fn record<'r>(output: &'r RegenOutput, id: &str) -> &'r DraftRecord {
    output
        .records
        .iter()
        .find(|record| record.id == id)
        .unwrap_or_else(|| panic!("no record {id}"))
}

fn unknown_ids(warnings: &[FileWarning]) -> Vec<&FileWarning> {
    warnings
        .iter()
        .filter(|warning| warning.kind == FileWarningKind::UnknownId)
        .collect()
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|id| (*id).to_owned()).collect()
}

const JS: &str = "\
// Totals the order lines before tax.
function total(lines) {
  // TODO: support discounts
  return lines.reduce((sum, line) => sum + line.price, 0); // empty is zero
}

/* Formats a price for display. */
const format = (price) => `$${price.toFixed(2)}`;
";

#[tokio::test]
async fn redact_join_fills_every_model_field_and_targets_only_undecided_records() {
    let llm = Tampered::honest(scenario(json!({
        "byKey": {
            "Totals the order lines before tax.": { "regen": {
                "comment": "Sum the line prices.",
                "difficulty": "MEDIUM",
                "rationale": "the tax detail is not in the code",
                "missingContext": "whether tax is applied later"
            } },
            "empty is zero": { "regen": {
                "comment": "",
                "difficulty": "EASY",
                "wouldYouCommentHere": false
            } }
        }
    })));
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("regeneration succeeds");
    assert_eq!(output.effective_mode, Mode::Redact);
    assert_eq!(output.warnings, []);

    // One call, for the undecided ids only (the TODO is decided by row 2).
    let calls = llm.fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call, CallKind::Regenerate);
    assert_eq!(calls[0].chunk_index, 0);
    assert_eq!(calls[0].ids, ids(&["C1", "C3", "C4"]));

    // The meta carries the raw originals, the effective mode and language.
    let meta = &llm.requests()[0].meta;
    assert_eq!(meta.mode, Mode::Redact);
    assert_eq!(meta.language, LanguageId::Javascript);
    assert_eq!(meta.path, "src/total.js");
    let originals: Vec<_> = meta.items.iter().map(|i| i.original.as_deref()).collect();
    assert_eq!(
        originals,
        [
            Some("// Totals the order lines before tax."),
            Some("// empty is zero"),
            Some("/* Formats a price for display. */"),
        ]
    );

    let c1 = record(&output, "C1");
    assert_eq!(c1.regenerated.as_deref(), Some("Sum the line prices."));
    assert_eq!(c1.would_you_comment_here, Some(true));
    assert_eq!(c1.difficulty, Some(Difficulty::Medium));
    assert_eq!(
        c1.difficulty_rationale.as_deref(),
        Some("the tax detail is not in the code")
    );
    assert_eq!(
        c1.missing_context.as_deref(),
        Some("whether tax is applied later")
    );
    assert_eq!(c1.decision, None, "routing is a later stage");

    // Empty + would-not-comment: no regeneration, but rated, so row 4.
    let c3 = record(&output, "C3");
    assert_eq!(c3.regenerated, None);
    assert_eq!(c3.would_you_comment_here, Some(false));
    assert_eq!(c3.difficulty, Some(Difficulty::Easy));
    assert!(c3.difficulty_rationale.is_some());
    assert_eq!(
        route(c3, 0.6, adapter_for(LanguageId::Javascript)),
        Route::Decided {
            decision: Decision::Remove,
            tags: Vec::new()
        }
    );

    // The default rule echoes the body, rated DIFFICULT.
    let c4 = record(&output, "C4");
    assert_eq!(
        c4.regenerated.as_deref(),
        Some("Formats a price for display.")
    );
    assert_eq!(c4.difficulty, Some(Difficulty::Difficult));

    // The decided TODO is untouched.
    let c2 = record(&output, "C2");
    assert_eq!(c2.decision, Some(Decision::KeepOriginal));
    assert_eq!((c2.regenerated.as_ref(), c2.difficulty), (None, None));
}

#[tokio::test]
async fn the_prompt_hides_exactly_the_requested_ids() {
    let llm = Tampered::honest(Scenario::default());
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    output.expect("regeneration succeeds");
    let request = &llm.requests()[0];
    let user = request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    for placeholder in ["//⟦C1⟧", "//⟦C3⟧", "/*⟦C4⟧*/"] {
        assert!(
            user.content.contains(placeholder),
            "{placeholder} in\n{}",
            user.content
        );
    }
    assert!(user.content.contains("// TODO: support discounts"));
    assert!(!user.content.contains("Totals the order lines"));
    assert_eq!(request.model, "test-model");
}

#[tokio::test]
async fn a_file_with_nothing_undecided_makes_zero_calls() {
    let text = "// TODO: remove this shim\nconst shim = 1;\n//\nconst other = 2;\n";
    let llm = Tampered::honest(Scenario::default());
    let (output, _) = regen("src/shim.js", LanguageId::Javascript, text, 120_000, &llm).await;
    let output = output.expect("nothing to regenerate");
    assert!(llm.fake.calls().is_empty());
    assert_eq!(output.records.len(), 2);
    assert!(output.records.iter().all(|r| r.decision.is_some()));
    assert_eq!(output.warnings, []);
    assert_eq!(output.effective_mode, Mode::Redact);
}

#[tokio::test]
async fn a_comment_free_file_makes_zero_calls() {
    let llm = Tampered::honest(Scenario::default());
    let (output, _) = regen(
        "a.js",
        LanguageId::Javascript,
        "const a = 1;\n",
        120_000,
        &llm,
    )
    .await;
    assert_eq!(output.expect("nothing to regenerate").records, []);
    assert!(llm.fake.calls().is_empty());
}

#[tokio::test]
async fn python_placeholders_and_join() {
    let text = "\
import os

# Join the parts with the platform separator.
def join(parts):
    return os.sep.join(parts)  # never empty

# TODO: handle bytes
X = 1
";
    let llm = Tampered::honest(scenario(json!({
        "byKey": {
            "never empty": { "regen": {
                "comment": "Callers always pass at least one part.",
                "difficulty": "DIFFICULT"
            } }
        }
    })));
    let (output, _) = regen("pkg/paths.py", LanguageId::Python, text, 120_000, &llm).await;
    let output = output.expect("regeneration succeeds");
    let request = &llm.requests()[0];
    assert_eq!(request.meta.language, LanguageId::Python);
    assert_eq!(request.meta.ids, ids(&["C1", "C2"]));
    let user = request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    assert!(user.content.contains("#⟦C1⟧"), "{}", user.content);
    assert!(user.content.contains("#⟦C2⟧"), "{}", user.content);
    assert!(user.content.contains("# TODO: handle bytes"));

    let c1 = record(&output, "C1");
    assert_eq!(
        c1.regenerated.as_deref(),
        Some("Join the parts with the platform separator.")
    );
    let c2 = record(&output, "C2");
    assert_eq!(
        c2.regenerated.as_deref(),
        Some("Callers always pass at least one part.")
    );
    assert_eq!(c2.difficulty, Some(Difficulty::Difficult));
    assert_eq!(record(&output, "C3").decision, Some(Decision::KeepOriginal));
}

#[tokio::test]
async fn leaked_ids_warn_exactly_once_each_and_never_touch_records() {
    let llm = Tampered::new(
        Scenario::default(),
        Box::new(|_, content| {
            with_extra_comments(
                &content,
                &[
                    answer("C2", "LEAKED into a decided record"),
                    answer("C99", "LEAKED"),
                ],
            )
        }),
    );
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("leakage is not fatal");
    let unknown = unknown_ids(&output.warnings);
    assert_eq!(unknown.len(), 2, "{:?}", output.warnings);
    assert!(unknown[0].detail.contains("\"C2\""));
    assert!(unknown[1].detail.contains("\"C99\""));
    assert_eq!(output.warnings.len(), 2);
    let c2 = record(&output, "C2");
    assert_eq!(c2.regenerated, None);
    assert_eq!(c2.difficulty, None);
    assert!(output.records.iter().all(|r| r.id != "C99"));
    for id in ["C1", "C3", "C4"] {
        assert!(record(&output, id).difficulty.is_some(), "{id} answered");
    }
}

#[tokio::test]
async fn an_unreturned_id_keeps_every_model_field_null_and_routes_to_the_reduced_set() {
    let llm = Tampered::new(
        Scenario::default(),
        Box::new(|_, content| without_comment(&content, "C3")),
    );
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("an unreturned id is not fatal");
    assert_eq!(output.warnings, []);
    let c3 = record(&output, "C3");
    assert_eq!(c3.regenerated, None);
    assert_eq!(c3.would_you_comment_here, None);
    assert_eq!(c3.difficulty, None);
    assert_eq!(c3.difficulty_rationale, None);
    assert_eq!(c3.missing_context, None);
    let Route::Judge { allowed } = route(c3, 0.6, adapter_for(LanguageId::Javascript)) else {
        panic!("an unreturned id goes to the judge");
    };
    assert_eq!(
        allowed,
        [
            Decision::Remove,
            Decision::KeepOriginal,
            Decision::NeedsReview
        ]
    );
    assert!(record(&output, "C1").regenerated.is_some());
    assert!(record(&output, "C4").regenerated.is_some());
}

fn truncating(times: Option<u32>) -> Scenario {
    scenario(json!({
        "failures": [{ "path": "*", "call": "regenerate", "error": "truncated", "times": times }]
    }))
}

#[tokio::test]
async fn a_truncated_reply_is_retried_as_halves_of_its_ids() {
    let llm = Tampered::honest(truncating(Some(1)));
    let (output, log) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("the halves succeed");
    let calls: Vec<_> = llm.fake.calls().into_iter().map(|c| c.ids).collect();
    assert_eq!(
        calls,
        [ids(&["C1", "C3", "C4"]), ids(&["C1", "C3"]), ids(&["C4"])]
    );
    // Every retry goes over the same chunk text, all three ids hidden.
    for request in llm.requests() {
        let user = &request.messages[1];
        assert_eq!(user.role, Role::User);
        for placeholder in ["//⟦C1⟧", "//⟦C3⟧", "/*⟦C4⟧*/"] {
            assert!(user.content.contains(placeholder), "{placeholder}");
        }
    }
    for id in ["C1", "C3", "C4"] {
        assert_eq!(
            record(&output, id).difficulty,
            Some(Difficulty::Difficult),
            "{id} answered"
        );
    }
    assert!(log.lines().iter().any(|line| line.contains("truncated")));
}

#[tokio::test]
async fn nested_truncations_split_recursively() {
    let llm = Tampered::honest(truncating(Some(2)));
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("the quarters succeed");
    let calls: Vec<_> = llm.fake.calls().into_iter().map(|c| c.ids).collect();
    assert_eq!(
        calls,
        [
            ids(&["C1", "C3", "C4"]),
            ids(&["C1", "C3"]),
            ids(&["C1"]),
            ids(&["C3"]),
            ids(&["C4"]),
        ]
    );
    assert!(
        output
            .records
            .iter()
            .all(|r| r.decision.is_some() || r.difficulty.is_some())
    );
}

#[tokio::test]
async fn leakage_between_split_halves_is_an_unknown_id() {
    let llm = Tampered::new(
        truncating(Some(1)),
        Box::new(|meta, content| {
            if meta.ids == ids(&["C1", "C3"]) {
                with_extra_comments(&content, &[answer("C4", "LEAKED")])
            } else {
                content
            }
        }),
    );
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("leakage is not fatal");
    let unknown = unknown_ids(&output.warnings);
    assert_eq!(unknown.len(), 1);
    assert!(unknown[0].detail.contains("\"C4\""));
    assert_eq!(
        record(&output, "C4").regenerated.as_deref(),
        Some("Formats a price for display.")
    );
}

#[tokio::test]
async fn truncating_a_single_id_fails_the_file() {
    let text = "// Totals the order lines before tax.\nfunction total(lines) {}\n";
    let llm = Tampered::honest(truncating(Some(1)));
    let (output, _) = regen("src/one.js", LanguageId::Javascript, text, 120_000, &llm).await;
    let error = output.expect_err("a single id cannot be split");
    assert!(
        matches!(&error, Error::Llm { reason: LlmErrorReason::Truncated, message } if message.contains("src/one.js")),
        "{error:?}"
    );
    assert_eq!(llm.fake.calls().len(), 1);
}

#[tokio::test]
async fn truncation_at_the_maximum_depth_fails_the_file() {
    let text: String = (0..16)
        .map(|i| format!("// Step {i} records the running value.\nconst v{i} = {i};\n"))
        .collect();
    let llm = Tampered::honest(truncating(None));
    let (output, _) = regen("src/steps.js", LanguageId::Javascript, &text, 120_000, &llm).await;
    let error = output.expect_err("the depth limit stops splitting");
    assert!(
        matches!(&error, Error::Llm { reason: LlmErrorReason::Truncated, message } if message.contains(&format!("depth {MAX_SPLIT_DEPTH}"))),
        "{error:?}"
    );
    let sizes: Vec<_> = llm.fake.calls().iter().map(|c| c.ids.len()).collect();
    assert_eq!(sizes, [16, 8, 4, 2]);
}

#[tokio::test]
async fn a_chunked_file_yields_the_union_of_its_chunk_results() {
    let text = fs::read_to_string(common::fixture_path("chunk/multi-decl.ts"))
        .expect("fixture is readable");
    let targets = undecided_ids(&text, LanguageId::Typescript, "multi-decl.ts");
    assert!(targets.len() > 5, "the fixture has many targets");

    let whole_llm = Tampered::honest(Scenario::default());
    let (whole, _) = regen(
        "multi-decl.ts",
        LanguageId::Typescript,
        &text,
        120_000,
        &whole_llm,
    )
    .await;
    let whole = whole.expect("unchunked regeneration succeeds");
    assert_eq!(whole_llm.fake.calls().len(), 1);

    let chunked_llm = Tampered::honest(Scenario::default());
    let (chunked, _) = regen(
        "multi-decl.ts",
        LanguageId::Typescript,
        &text,
        300,
        &chunked_llm,
    )
    .await;
    let chunked = chunked.expect("chunked regeneration succeeds");
    let calls = chunked_llm.fake.calls();
    let chunk_indices: BTreeSet<_> = calls.iter().map(|c| c.chunk_index).collect();
    assert!(
        chunk_indices.len() > 2,
        "the file is chunked: {chunk_indices:?}"
    );
    assert_eq!(chunk_indices.len(), calls.len(), "one call per chunk");

    // No duplicate ids, no lost comments.
    let requested: Vec<String> = calls.into_iter().flat_map(|c| c.ids).collect();
    let unique: BTreeSet<String> = requested.iter().cloned().collect();
    assert_eq!(unique.len(), requested.len(), "no id is requested twice");
    assert_eq!(unique, targets);

    // Each chunk call's meta covers exactly its requested ids.
    for request in chunked_llm.requests() {
        let item_ids: Vec<_> = request.meta.items.iter().map(|i| i.id.clone()).collect();
        assert_eq!(item_ids, request.meta.ids);
        assert!(
            !request.meta.ids.is_empty(),
            "no call for a chunk without targets"
        );
    }

    // The union equals the unchunked result, record for record.
    assert_eq!(chunked.records, whole.records);
    let adapter = adapter_for(LanguageId::Typescript);
    for record in chunked.records.iter().filter(|r| targets.contains(&r.id)) {
        let original = record.original.as_deref().expect("scanned");
        assert_eq!(
            record.regenerated.as_deref(),
            Some(comment_body(original, adapter).as_str()),
            "{}",
            record.id
        );
    }
    assert!(
        chunked
            .warnings
            .iter()
            .all(|w| w.kind == FileWarningKind::OversizedChunk),
        "{:?}",
        chunked.warnings
    );
}

/// Replies the fake never sends; each must fail the call cleanly.
fn adversarial_edits() -> Vec<(&'static str, Edit)> {
    vec![
        (
            "not JSON",
            Box::new(|_, _| "Sure! Here are the comments.".to_owned()),
        ),
        ("empty", Box::new(|_, _| String::new())),
        ("an array", Box::new(|_, _| "[]".to_owned())),
        (
            "a wrong type",
            Box::new(|_, _| r#"{"comments": 5}"#.to_owned()),
        ),
        (
            "a null comment list",
            Box::new(|_, _| r#"{"comments": null}"#.to_owned()),
        ),
        (
            "a duplicate id",
            Box::new(|_, content| with_extra_comments(&content, &[answer("C1", "again")])),
        ),
        (
            "an empty comment the model would write",
            Box::new(|_, content| with_extra_comments(&content, &[answer("C99", "  ")])),
        ),
        (
            "an unknown difficulty",
            Box::new(|_, content| {
                let mut extra = answer("C99", "text");
                extra["difficulty"] = json!("IMPOSSIBLE");
                with_extra_comments(&content, &[extra])
            }),
        ),
        (
            "a missing field",
            Box::new(|_, content| {
                let mut extra = answer("C99", "text");
                extra
                    .as_object_mut()
                    .expect("an object")
                    .remove("rationale");
                with_extra_comments(&content, &[extra])
            }),
        ),
        (
            "an unterminated object",
            Box::new(|_, content| {
                let half = content.chars().count() / 2;
                content.chars().take(half).collect()
            }),
        ),
    ]
}

#[tokio::test]
async fn adversarial_replies_fail_the_file_without_panicking() {
    for (name, edit) in adversarial_edits() {
        let llm = Tampered::new(Scenario::default(), edit);
        let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
        let error = output.expect_err(name);
        assert!(
            matches!(
                error,
                Error::Llm {
                    reason: LlmErrorReason::Invalid,
                    ..
                }
            ),
            "{name}: {error:?}"
        );
        // The first reply and its one repair turn.
        assert_eq!(llm.fake.calls().len(), 2, "{name}");
    }
}

#[tokio::test]
async fn a_fenced_reply_is_accepted_without_a_repair() {
    let llm = Tampered::new(
        Scenario::default(),
        Box::new(|_, content| format!("```json\n{content}\n```")),
    );
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    let output = output.expect("fences are stripped locally");
    assert_eq!(llm.fake.calls().len(), 1);
    assert!(record(&output, "C1").regenerated.is_some());
}

#[tokio::test]
async fn other_llm_failures_propagate_unchanged() {
    let llm = Tampered::honest(scenario(json!({
        "failures": [{ "path": "src/total.js", "error": "auth" }]
    })));
    let (output, _) = regen("src/total.js", LanguageId::Javascript, JS, 120_000, &llm).await;
    assert!(matches!(
        output.expect_err("auth fails the file"),
        Error::Llm {
            reason: LlmErrorReason::Auth,
            ..
        }
    ));
    assert_eq!(llm.fake.calls().len(), 1, "no split for a non-truncation");
}

// ---------------------------------------------------------------------------
// Remove mode
// ---------------------------------------------------------------------------

/// [`JS`] plus a dangling comment at end of file.
const JS_EOF: &str = "\
// Totals the order lines before tax.
function total(lines) {
  // TODO: support discounts
  return lines.reduce((sum, line) => sum + line.price, 0); // empty is zero
}

/* Formats a price for display. */
const format = (price) => `$${price.toFixed(2)}`;

// End of the pricing helpers.
";

async fn remove(
    path: &str,
    language: LanguageId,
    text: &str,
    max_file_chars: u64,
    llm: &dyn LlmClient,
) -> (Result<RegenOutput, Error>, CapturingLog) {
    regen_in(Mode::Remove, path, language, text, max_file_chars, llm).await
}

fn warnings_of(output: &RegenOutput, kind: FileWarningKind) -> Vec<&FileWarning> {
    output.warnings.iter().filter(|w| w.kind == kind).collect()
}

#[tokio::test]
async fn remove_mode_aligns_synthesized_anchors_including_end_of_file() {
    let llm = Tampered::honest(scenario(json!({
        "byKey": {
            "Totals the order lines before tax.": { "regen": {
                "comment": "// Sum the line prices.",
                "difficulty": "MEDIUM",
                "rationale": "the tax detail is not in the code",
                "missingContext": "whether tax is applied later"
            } },
            "empty is zero": { "regen": {
                "comment": "unused",
                "difficulty": "EASY",
                "wouldYouCommentHere": false
            } }
        }
    })));
    let (output, _) = remove(
        "src/total.js",
        LanguageId::Javascript,
        JS_EOF,
        120_000,
        &llm,
    )
    .await;
    let output = output.expect("regeneration succeeds");
    assert_eq!(output.effective_mode, Mode::Remove);
    assert_eq!(output.warnings, []);

    // One call: no requested ids, one item per hidden target.
    let calls = llm.fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].ids, Vec::<String>::new());
    let meta = &llm.requests()[0].meta;
    assert_eq!(meta.mode, Mode::Remove);
    let items: Vec<_> = meta.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(items, ["C1", "C3", "C4", "C5"]);

    // The prompt is the clean text: targets gone, the decided TODO kept.
    let user = &llm.requests()[0].messages[1];
    assert_eq!(user.role, Role::User);
    assert!(user.content.contains("// TODO: support discounts"));
    for gone in [
        "Totals the order",
        "empty is zero",
        "Formats a price",
        "End of the",
    ] {
        assert!(!user.content.contains(gone), "{gone} is removed");
    }
    assert!(
        !user.content.contains('⟦'),
        "no placeholders in remove mode"
    );

    // A named declaration pairs by (name, kind); the model's text is
    // normalized.
    let c1 = record(&output, "C1");
    assert_eq!(c1.regenerated.as_deref(), Some("Sum the line prices."));
    assert_eq!(c1.would_you_comment_here, None, "null in remove mode");
    assert_eq!(c1.difficulty, Some(Difficulty::Medium));
    assert_eq!(
        c1.difficulty_rationale.as_deref(),
        Some("the tax detail is not in the code")
    );
    assert_eq!(
        c1.missing_context.as_deref(),
        Some("whether tax is applied later")
    );
    assert_eq!(c1.decision, None, "routing is a later stage");

    // Not cited: an unmatched original keeps every model field null and
    // reaches the judge with the reduced set.
    let c3 = record(&output, "C3");
    assert_eq!(
        (
            c3.regenerated.as_ref(),
            c3.difficulty,
            c3.would_you_comment_here
        ),
        (None, None, None)
    );
    assert_eq!(
        route(c3, 0.6, adapter_for(LanguageId::Javascript)),
        Route::Judge {
            allowed: REDUCED_SET.to_vec()
        }
    );

    let c4 = record(&output, "C4");
    assert_eq!(
        c4.regenerated.as_deref(),
        Some("Formats a price for display.")
    );

    // The dangling EOF comment pairs through the `end_of_file` anchor.
    let c5 = record(&output, "C5");
    assert_eq!(c5.anchor.node_kind, "end_of_file");
    assert_eq!(
        c5.regenerated.as_deref(),
        Some("End of the pricing helpers.")
    );
    assert_eq!(c5.difficulty, Some(Difficulty::Difficult));

    // No NEW records, and the decided TODO is untouched.
    assert!(output.records.iter().all(|r| !r.is_new_origin()));
    let c2 = record(&output, "C2");
    assert_eq!(c2.decision, Some(Decision::KeepOriginal));
    assert_eq!((c2.regenerated.as_ref(), c2.difficulty), (None, None));
}

#[tokio::test]
async fn chunked_remove_mode_aligns_member_only_sub_chunks() {
    let text = fs::read_to_string(common::fixture_path("golden/java/large-class.java"))
        .expect("fixture is readable");
    let targets = undecided_ids(&text, LanguageId::Java, "large-class.java");
    assert!(targets.len() > 5, "the fixture has many targets");

    let whole_llm = Tampered::honest(Scenario::default());
    let (whole, _) = remove(
        "LargeClass.java",
        LanguageId::Java,
        &text,
        120_000,
        &whole_llm,
    )
    .await;
    let whole = whole.expect("unchunked regeneration succeeds");
    assert_eq!(whole_llm.fake.calls().len(), 1);

    let chunked_llm = Tampered::honest(Scenario::default());
    let (chunked, _) = remove(
        "LargeClass.java",
        LanguageId::Java,
        &text,
        400,
        &chunked_llm,
    )
    .await;
    let chunked = chunked.expect("chunked regeneration succeeds");
    let requests = chunked_llm.requests();
    assert!(requests.len() > 2, "the file is chunked");

    // Some chunk is member-only: neither the class header nor its close.
    let inventory = inventory_file(
        &text,
        FileRef {
            language: LanguageId::Java,
            file_name: "LargeClass.java".to_owned(),
        },
    )
    .expect("the fixture inventories");
    let (chunks, _) =
        chunk_file(&text, LanguageId::Java, &inventory, 400, None).expect("the fixture chunks");
    let header_end = text.find('{').expect("a class body") + 1;
    let member_only: Vec<usize> = chunks
        .iter()
        .filter(|c| c.original_start >= header_end && c.original_end < text.trim_end().len())
        .map(|c| c.index)
        .collect();
    assert!(!member_only.is_empty(), "{chunks:#?}");
    let called: BTreeSet<usize> = requests.iter().map(|r| r.meta.chunk_index).collect();
    assert!(
        member_only.iter().all(|index| called.contains(index)),
        "every member-only chunk is sent: {member_only:?} vs {called:?}"
    );

    // Every target is hidden in exactly one chunk.
    let items: Vec<String> = requests
        .iter()
        .flat_map(|r| r.meta.items.iter().map(|i| i.id.clone()))
        .collect();
    let unique: BTreeSet<String> = items.iter().cloned().collect();
    assert_eq!(unique.len(), items.len(), "no target is sent twice");
    assert_eq!(unique, targets);

    // Each echo pairs with its own original, exactly as unchunked.
    assert_eq!(chunked.records, whole.records);
    let adapter = adapter_for(LanguageId::Java);
    for record in chunked.records.iter().filter(|r| targets.contains(&r.id)) {
        let original = record.original.as_deref().expect("scanned");
        assert_eq!(
            record.regenerated.as_deref(),
            Some(comment_body(original, adapter).as_str()),
            "{}",
            record.id
        );
    }
    assert!(
        chunked
            .warnings
            .iter()
            .all(|w| w.kind == FileWarningKind::OversizedChunk),
        "{:?}",
        chunked.warnings
    );
}

#[tokio::test]
async fn a_placeholder_collision_falls_back_to_remove_mode() {
    let text = fs::read_to_string(common::fixture_path("collision/collision.ts"))
        .expect("fixture is readable");
    let targets = undecided_ids(&text, LanguageId::Typescript, "collision.ts");
    assert!(!targets.is_empty());
    let llm = Tampered::honest(Scenario::default());
    let (output, _) = regen(
        "src/collision.ts",
        LanguageId::Typescript,
        &text,
        120_000,
        &llm,
    )
    .await;
    let output = output.expect("the fallback succeeds");
    assert_eq!(output.effective_mode, Mode::Remove);
    assert_eq!(
        output.warnings.first().map(|w| w.kind),
        Some(FileWarningKind::PlaceholderCollision)
    );
    assert_eq!(
        warnings_of(&output, FileWarningKind::PlaceholderCollision).len(),
        1
    );

    // The calls are remove-mode calls over the clean text.
    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].meta.mode, Mode::Remove);
    assert!(requests[0].meta.ids.is_empty());
    assert!(
        requests[0].messages[1]
            .content
            .contains("const marker = \"⟦C1⟧\";")
    );

    // The records behave like remove-mode records.
    let adapter = adapter_for(LanguageId::Typescript);
    for record in output.records.iter().filter(|r| targets.contains(&r.id)) {
        let original = record.original.as_deref().expect("scanned");
        assert_eq!(record.would_you_comment_here, None, "{}", record.id);
        assert_eq!(
            record.regenerated.as_deref(),
            Some(comment_body(original, adapter).as_str()),
            "{}",
            record.id
        );
    }
}

#[tokio::test]
async fn a_zero_call_collision_file_still_records_remove_mode() {
    let text = "// TODO: replace ⟦C1⟧ markers\nconst marker = \"⟦C1⟧\";\n";
    let llm = Tampered::honest(Scenario::default());
    let (output, _) = regen("src/marker.ts", LanguageId::Typescript, text, 120_000, &llm).await;
    let output = output.expect("nothing to regenerate");
    assert!(llm.fake.calls().is_empty());
    assert_eq!(output.effective_mode, Mode::Remove);
    let kinds: Vec<_> = output.warnings.iter().map(|w| w.kind).collect();
    assert_eq!(kinds, [FileWarningKind::PlaceholderCollision]);
}

#[tokio::test]
async fn configured_remove_mode_ignores_placeholder_text() {
    let text = fs::read_to_string(common::fixture_path("collision/collision.ts"))
        .expect("fixture is readable");
    let llm = Tampered::honest(Scenario::default());
    let (output, _) = remove(
        "src/collision.ts",
        LanguageId::Typescript,
        &text,
        120_000,
        &llm,
    )
    .await;
    let output = output.expect("remove mode needs no fallback");
    assert_eq!(output.effective_mode, Mode::Remove);
    assert_eq!(output.warnings, [], "no collision warning in remove mode");
}

const JS_NEW: &str = "\
// Totals the order lines before tax.
function total(lines) {
  return lines.reduce((sum, line) => sum + line.price, 0);
}

const format = (price) => `$${price.toFixed(2)}`;

function round(value) {
  return Math.round(value * 100) / 100;
}
";

fn extra(anchor: Value, position: &str, comment: &str, difficulty: &str) -> Value {
    json!({
        "path": "src/money.js",
        "anchor": anchor,
        "position": position,
        "comment": comment,
        "difficulty": difficulty,
    })
}

#[tokio::test]
async fn new_records_are_numbered_by_offset_held_when_difficult_and_sanitized() {
    let round = json!({
        "nodeKind": "function_declaration",
        "name": "round",
        "snippet": "function round(value) {"
    });
    let format = json!({
        "nodeKind": "lexical_declaration",
        "name": "format",
        "snippet": "const format = (price) => `$${price.toFixed(2)}`;"
    });
    let rounding = json!({
        "nodeKind": "return_statement",
        "name": null,
        "snippet": "return Math.round(value * 100) / 100;"
    });
    let llm = Tampered::honest(scenario(json!({
        "extraRegenerations": [
            extra(round, "leading", "// Rounds to whole cents.", "MEDIUM"),
            extra(format, "leading", "Uses the dollar sign whatever the locale.", "DIFFICULT"),
            extra(rounding, "trailing", "eslint-disable-line no-magic-numbers", "EASY"),
        ]
    })));
    let (output, _) = remove(
        "src/money.js",
        LanguageId::Javascript,
        JS_NEW,
        120_000,
        &llm,
    )
    .await;
    let output = output.expect("regeneration succeeds");

    // The scanned record pairs as usual.
    assert_eq!(
        record(&output, "C1").regenerated.as_deref(),
        Some("Totals the order lines before tax.")
    );

    // The functional-looking text is dropped, with a warning.
    let dropped = warnings_of(&output, FileWarningKind::SanitizeDroppedNew);
    assert_eq!(dropped.len(), 1, "{:?}", output.warnings);
    assert!(
        dropped[0].detail.contains("functional"),
        "{}",
        dropped[0].detail
    );
    assert_eq!(output.warnings.len(), 1, "{:?}", output.warnings);

    // Ids follow insertion offsets, not citation order.
    let new: Vec<&DraftRecord> = output
        .records
        .iter()
        .filter(|r| r.is_new_origin())
        .collect();
    let new_ids: Vec<_> = new.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(new_ids, ["N1", "N2"]);

    let format_line = JS_NEW.find("const format").expect("format");
    let n1 = record(&output, "N1");
    assert_eq!((n1.start, n1.end), (format_line, format_line));
    assert_eq!((n1.line, n1.column), (6, 1));
    assert_eq!(n1.anchor.name.as_deref(), Some("format"));
    assert_eq!(n1.position, Position::Leading);
    assert_eq!(n1.kind, CommentKind::Line, "the prevailing NEW style");
    assert_eq!(n1.classification, Classification::Prose);
    assert_eq!(n1.original, None);
    assert_eq!(n1.tags, Vec::<String>::new());
    assert_eq!(n1.would_you_comment_here, None);
    assert_eq!(n1.difficulty, Some(Difficulty::Difficult));
    assert_eq!(
        n1.final_comment.as_deref(),
        Some("Uses the dollar sign whatever the locale.")
    );
    assert_eq!(n1.regenerated, n1.final_comment);
    // DIFFICULT: held for review, still NEW-origin.
    assert_eq!(n1.decision, Some(Decision::NeedsReview));
    assert_eq!(n1.decided_by, Some(DecidedBy::Engine));
    assert_eq!(n1.confidence, Some(1.0));
    assert!(n1.decision_rationale.is_some());

    let round_line = JS_NEW.find("function round").expect("round");
    let n2 = record(&output, "N2");
    assert_eq!((n2.start, n2.end), (round_line, round_line));
    assert_eq!((n2.line, n2.column), (8, 1));
    assert_eq!(n2.final_comment.as_deref(), Some("Rounds to whole cents."));
    assert_eq!(n2.difficulty, Some(Difficulty::Medium));
    assert_eq!(n2.decision, Some(Decision::New));
    assert_eq!(n2.decided_by, Some(DecidedBy::Engine));
    assert_eq!(n2.confidence, Some(1.0));

    // Records are sorted by (start, id).
    let starts: Vec<_> = output.records.iter().map(|r| r.start).collect();
    assert!(starts.windows(2).all(|w| w[0] <= w[1]), "{starts:?}");
    let all_ids: Vec<_> = output.records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(all_ids, ["C1", "N1", "N2"]);
}

#[tokio::test]
async fn new_records_take_the_prevailing_block_style() {
    let text = "/* Totals the lines. */\nfunction total(lines) { return 0; }\n\
                function round(v) { return v; }\n";
    let round = json!({
        "nodeKind": "function_declaration",
        "name": "round",
        "snippet": "function round(v) { return v; }"
    });
    let llm = Tampered::honest(scenario(json!({
        "extraRegenerations": [
            extra(round.clone(), "leading", "Rounds the value.", "EASY"),
            extra(round, "trailing", "keeps */ cents", "EASY"),
        ]
    })));
    let (output, _) = remove("src/money.js", LanguageId::Javascript, text, 120_000, &llm).await;
    let output = output.expect("regeneration succeeds");
    let n1 = record(&output, "N1");
    assert_eq!(n1.kind, CommentKind::Block);
    assert_eq!(n1.final_comment.as_deref(), Some("Rounds the value."));
    assert_eq!(n1.decision, Some(Decision::New));
    assert!(output.records.iter().all(|r| r.id != "N2"));
    let dropped = warnings_of(&output, FileWarningKind::SanitizeDroppedNew);
    assert_eq!(dropped.len(), 1, "{:?}", output.warnings);
    assert!(dropped[0].detail.contains("*/"), "{}", dropped[0].detail);
}

#[tokio::test]
async fn a_truncated_remove_reply_is_retried_over_re_cut_sub_chunks() {
    let whole_llm = Tampered::honest(Scenario::default());
    let (whole, _) = remove(
        "src/total.js",
        LanguageId::Javascript,
        JS_EOF,
        120_000,
        &whole_llm,
    )
    .await;
    let whole = whole.expect("regeneration succeeds");

    let llm = Tampered::honest(truncating(Some(1)));
    let (output, log) = remove(
        "src/total.js",
        LanguageId::Javascript,
        JS_EOF,
        120_000,
        &llm,
    )
    .await;
    let output = output.expect("the sub-chunks succeed");
    let requests = llm.requests();
    assert!(requests.len() > 2, "{} calls", requests.len());
    // Every retry keeps the top-level chunk's index; the sub-chunks
    // partition the targets between them.
    assert!(requests.iter().all(|r| r.meta.chunk_index == 0));
    let first: Vec<_> = requests[0]
        .meta
        .items
        .iter()
        .map(|i| i.id.clone())
        .collect();
    let retried: Vec<_> = requests[1..]
        .iter()
        .flat_map(|r| r.meta.items.iter().map(|i| i.id.clone()))
        .collect();
    assert_eq!(retried, first);
    // The sub-chunk prompts are chunk prompts (with the file outline);
    // the unchunked first one is not.
    assert!(!requests[0].messages[1].content.contains("## File outline"));
    for request in &requests[1..] {
        assert!(request.messages[1].content.contains("## File outline"));
    }
    // Alignment against the whole-file clean text is unaffected.
    assert_eq!(output.records, whole.records);
    assert!(log.lines().iter().any(|line| line.contains("truncated")));
}

#[tokio::test]
async fn truncating_an_unsplittable_remove_chunk_fails_the_file() {
    let text = "// Totals the order lines before tax.\nfunction total(lines) {}\n";
    let llm = Tampered::honest(truncating(Some(1)));
    let (output, _) = remove("src/one.js", LanguageId::Javascript, text, 120_000, &llm).await;
    let error = output.expect_err("a single declaration cannot be re-cut");
    assert!(
        matches!(&error, Error::Llm { reason: LlmErrorReason::Truncated, message }
            if message.contains("src/one.js") && message.contains("depth 0")),
        "{error:?}"
    );
    assert_eq!(llm.fake.calls().len(), 1);
}

#[tokio::test]
async fn remove_truncation_at_the_maximum_depth_fails_the_file() {
    let text: String = (0..32)
        .map(|i| format!("// Step {i} records the running value.\nconst v{i} = {i};\n"))
        .collect();
    let llm = Tampered::honest(truncating(None));
    let (output, _) = remove("src/steps.js", LanguageId::Javascript, &text, 120_000, &llm).await;
    let error = output.expect_err("the depth limit stops re-cutting");
    assert!(
        matches!(&error, Error::Llm { reason: LlmErrorReason::Truncated, message }
            if message.contains(&format!("depth {MAX_SPLIT_DEPTH}"))),
        "{error:?}"
    );
    // Depth first: the first sub-chunk of each level is sent next.
    let sizes: Vec<_> = llm.requests().iter().map(|r| r.meta.items.len()).collect();
    assert_eq!(sizes.len(), 4, "{sizes:?}");
    assert!(sizes.windows(2).all(|w| w[1] < w[0]), "{sizes:?}");
}

fn cited(
    node_kind: &str,
    name: Option<&str>,
    snippet: &str,
    position: &str,
    comment: &str,
) -> Value {
    json!({
        "anchor": { "nodeKind": node_kind, "name": name, "snippet": snippet },
        "position": position,
        "comment": comment,
        "difficulty": "MEDIUM",
        "rationale": "injected",
        "missingContext": null,
    })
}

#[tokio::test]
async fn adversarial_remove_replies_fail_the_file_without_panicking() {
    let mut edits = adversarial_edits();
    edits.push((
        "a dangling citation",
        Box::new(|_, content| {
            with_extra_comments(&content, &[cited("end_of_file", None, "", "dangling", "x")])
        }),
    ));
    edits.push((
        "a blank comment",
        Box::new(|_, content| {
            with_extra_comments(&content, &[cited("end_of_file", None, "", "leading", " ")])
        }),
    ));
    for (name, edit) in edits {
        let llm = Tampered::new(Scenario::default(), edit);
        let (output, _) = remove(
            "src/total.js",
            LanguageId::Javascript,
            JS_EOF,
            120_000,
            &llm,
        )
        .await;
        let error = output.expect_err(name);
        assert!(
            matches!(
                error,
                Error::Llm {
                    reason: LlmErrorReason::Invalid,
                    ..
                }
            ),
            "{name}: {error:?}"
        );
        assert_eq!(llm.fake.calls().len(), 2, "{name}");
    }
}

#[tokio::test]
async fn adversarial_remove_citations_are_dropped_with_warnings() {
    let honest_llm = Tampered::honest(Scenario::default());
    let (honest, _) = remove(
        "src/total.js",
        LanguageId::Javascript,
        JS_EOF,
        120_000,
        &honest_llm,
    )
    .await;
    let honest = honest.expect("regeneration succeeds");

    let adversarial = vec![
        // A hallucinated declaration.
        cited(
            "function_declaration",
            Some("refund"),
            "function refund(order) {",
            "leading",
            "Refunds.",
        ),
        // An unknown kind with a blank snippet.
        cited("???", None, "   \n\n", "leading", "Nothing to cite."),
        // A snippet that is not in the file, with multi-byte text.
        cited(
            "expression_statement",
            None,
            "⟦C1⟧ — é",
            "trailing",
            "Hallucinated.",
        ),
        // A second end-of-file citation: never a NEW insertion point.
        cited("end_of_file", None, "", "trailing", "Closing remark."),
        // A second citation of an already-commented declaration.
        cited(
            "function_declaration",
            Some("total"),
            "function total(lines) {",
            "leading",
            "Again.",
        ),
        // A valid slot, but text no slot may hold.
        cited(
            "lexical_declaration",
            Some("format"),
            "const format = (price) => `$${price.toFixed(2)}`;",
            "trailing",
            "flips \u{202E} direction",
        ),
    ];
    let count = adversarial.len();
    let llm = Tampered::new(
        Scenario::default(),
        Box::new(move |_, content| with_extra_comments(&content, &adversarial)),
    );
    let (output, _) = remove(
        "src/total.js",
        LanguageId::Javascript,
        JS_EOF,
        120_000,
        &llm,
    )
    .await;
    let output = output.expect("bad citations are not fatal");
    assert_eq!(llm.fake.calls().len(), 1, "a valid reply needs no repair");
    assert_eq!(output.records, honest.records, "the pairs are unaffected");
    assert!(output.records.iter().all(|r| !r.is_new_origin()));
    assert_eq!(output.warnings.len(), count, "{:#?}", output.warnings);
    assert!(output.warnings.iter().all(|w| matches!(
        w.kind,
        FileWarningKind::UnresolvableAnchor
            | FileWarningKind::InvalidInsertion
            | FileWarningKind::SanitizeDroppedNew
    )));
    assert_eq!(
        warnings_of(&output, FileWarningKind::SanitizeDroppedNew).len(),
        1
    );
}
