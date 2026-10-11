//! `FakeLlm` + `Scenario`: keying, routing, the default rule, failure
//! injection, the call log and the `on_call` hook. Every reply goes through
//! `complete_typed`, so each test also proves the fake speaks the wire
//! schema.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use munge_comments::artifact::types::{Difficulty, JudgeDecision};
use munge_comments::comments::types::{Anchor, Position};
use munge_comments::llm::{
    CallKind, CallRecord, ChatRequest, CitedPosition, JudgeResponse, LlmMeta, Message, MetaItem,
    RegenerationResponse, RemoveModeRegenerationResponse, complete_typed,
};
use munge_comments::{Error, FakeLlm, LanguageId, LlmErrorReason, Mode, Scenario};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

fn sample() -> Scenario {
    Scenario::from_json_file(&common::fixture_path("scenarios/sample.json"))
        .expect("the sample scenario loads")
}

fn scenario(json: serde_json::Value) -> Scenario {
    serde_json::from_value(json).expect("a valid scenario")
}

fn anchor(node_kind: &str, name: Option<&str>, snippet: &str) -> Anchor {
    Anchor {
        node_kind: node_kind.to_string(),
        name: name.map(str::to_string),
        snippet: snippet.to_string(),
    }
}

fn item(id: &str, original: Option<&str>, name: Option<&str>, position: Position) -> MetaItem {
    MetaItem {
        id: id.to_string(),
        original: original.map(str::to_string),
        regenerated: None,
        anchor: anchor("function_declaration", name, "function f() {}"),
        position,
    }
}

fn meta(call: CallKind, mode: Mode, path: &str, language: LanguageId) -> LlmMeta {
    LlmMeta {
        call,
        mode,
        path: path.to_string(),
        language,
        chunk_index: 0,
        ids: Vec::new(),
        items: Vec::new(),
    }
}

fn request(meta: LlmMeta) -> ChatRequest {
    ChatRequest {
        model: "fake-model".to_string(),
        messages: vec![
            Message::system("Answer in JSON."),
            Message::user("(prompt text is never read by FakeLlm)"),
        ],
        response_format: serde_json::Value::Null,
        temperature: Some(0.2),
        max_completion_tokens: 16_384,
        meta,
    }
}

async fn ask<T>(llm: &FakeLlm, meta: LlmMeta) -> Result<T, Error>
where
    T: DeserializeOwned + schemars::JsonSchema + munge_comments::llm::WireValidate,
{
    complete_typed::<T>(llm, request(meta)).await
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn reason<T: std::fmt::Debug>(result: Result<T, Error>) -> LlmErrorReason {
    match result {
        Err(Error::Llm { reason, .. }) => reason,
        other => panic!("expected an LLM error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------

#[test]
fn the_sample_scenario_loads() {
    let scenario = sample();
    assert_eq!(scenario.by_key.len(), 4);
    assert_eq!(scenario.extra_regenerations.len(), 1);
    assert_eq!(scenario.failures.len(), 2);
    assert_eq!(
        scenario.failures[1].times, None,
        "times defaults to every call"
    );
    assert_eq!(scenario.extra_regenerations[0].chunk_index, 0);
}

#[test]
fn a_relative_scenario_path_is_a_usage_error() {
    let err = Scenario::from_json_file(Path::new("tests/fixtures/scenarios/sample.json"))
        .expect_err("relative paths are rejected");
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
}

#[test]
fn a_missing_scenario_file_is_a_filesystem_error() {
    let dir = common::tempdir();
    let err = Scenario::from_json_file(&dir.path().join("absent.json"))
        .expect_err("a missing file fails");
    assert!(matches!(err, Error::Fs { .. }), "{err:?}");
}

#[test]
fn invalid_scenario_json_is_a_usage_error() {
    let dir = common::tempdir();
    let path = dir.path().join("typo.json");
    std::fs::write(&path, r#"{ "byKeys": {} }"#).expect("write the scenario");
    let err = Scenario::from_json_file(&path).expect_err("unknown fields are rejected");
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
}

#[test]
fn an_empty_object_is_the_default_scenario() {
    assert_eq!(scenario(serde_json::json!({})), Scenario::default());
}

// ---------------------------------------------------------------------
// Regeneration, redact mode
// ---------------------------------------------------------------------

fn redact_meta(path: &str, requested: &[&str], items: Vec<MetaItem>) -> LlmMeta {
    LlmMeta {
        ids: ids(requested),
        items,
        ..meta(
            CallKind::Regenerate,
            Mode::Redact,
            path,
            LanguageId::Typescript,
        )
    }
}

#[tokio::test]
async fn redact_mode_answers_exactly_the_requested_ids() {
    let llm = FakeLlm::new(sample());
    let items = vec![
        item(
            "C1",
            Some("// Increment   the counter."),
            None,
            Position::Leading,
        ),
        item(
            "C2",
            Some("/* Validate input. */"),
            None,
            Position::Trailing,
        ),
        item("C3", Some("// Not requested."), None, Position::Leading),
    ];
    let reply: RegenerationResponse =
        ask(&llm, redact_meta("src/a.ts", &["C2", "C1", "C9"], items))
            .await
            .expect("a valid reply");
    let answered: Vec<&str> = reply.comments.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(answered, ["C2", "C1", "C9"], "exactly meta.ids, in order");

    // Unlisted: the default rule echoes the comment body, rated DIFFICULT.
    let c2 = &reply.comments[0];
    assert!(c2.would_you_comment_here);
    assert_eq!(c2.comment, "Validate input.");
    assert_eq!(c2.difficulty, Difficulty::Difficult);

    // Keyed by match_key (whitespace-collapsed body), bare key.
    let c1 = &reply.comments[1];
    assert_eq!(c1.comment, "Increments the counter.");
    assert_eq!(c1.difficulty, Difficulty::Easy);
    assert_eq!(c1.rationale, "The code says it.");
    assert_eq!(c1.missing_context, None);

    // An id with no routing item has nothing to echo.
    let c9 = &reply.comments[2];
    assert!(!c9.would_you_comment_here);
    assert_eq!(c9.comment, "");
}

#[tokio::test]
async fn a_path_qualified_entry_wins_over_the_bare_key() {
    let llm = FakeLlm::new(sample());
    let items = vec![item(
        "C1",
        Some("// Increment the counter."),
        None,
        Position::Leading,
    )];
    let reply: RegenerationResponse = ask(&llm, redact_meta("src/billing.ts", &["C1"], items))
        .await
        .expect("a valid reply");
    let c1 = &reply.comments[0];
    assert_eq!(
        c1.comment,
        "Bump the invoice sequence; gaps are illegal in some jurisdictions."
    );
    assert_eq!(c1.difficulty, Difficulty::Medium);
    assert_eq!(
        c1.rationale, "scripted by FakeLlm",
        "omitted rationale defaults"
    );
}

#[tokio::test]
async fn a_script_may_decline_to_comment() {
    let llm = FakeLlm::new(sample());
    let original = "# Retries are capped because the upstream\n# rate-limits bursts.";
    let items = vec![item("C4", Some(original), None, Position::Leading)];
    let meta = LlmMeta {
        language: LanguageId::Python,
        ..redact_meta("src/worker.py", &["C4"], items)
    };
    let reply: RegenerationResponse = ask(&llm, meta).await.expect("a valid reply");
    let c4 = &reply.comments[0];
    assert!(!c4.would_you_comment_here);
    assert_eq!(c4.comment, "");
    assert_eq!(
        c4.missing_context.as_deref(),
        Some("The upstream's rate-limit policy.")
    );
}

#[tokio::test]
async fn keys_are_computed_with_the_language_of_the_file() {
    // A `#` line is a Python comment, so its match key drops the `#`.
    let llm = FakeLlm::new(scenario(serde_json::json!({
        "byKey": { "Hash comment.": { "regen": { "comment": "scripted", "difficulty": "EASY" } } }
    })));
    let items = vec![item("C1", Some("# Hash comment."), None, Position::Leading)];
    let meta = LlmMeta {
        language: LanguageId::Python,
        ..redact_meta("x.py", &["C1"], items)
    };
    let reply: RegenerationResponse = ask(&llm, meta).await.expect("a valid reply");
    assert_eq!(reply.comments[0].comment, "scripted");
}

// ---------------------------------------------------------------------
// Regeneration, remove mode
// ---------------------------------------------------------------------

fn remove_meta(path: &str, chunk_index: usize, items: Vec<MetaItem>) -> LlmMeta {
    LlmMeta {
        chunk_index,
        items,
        ..meta(CallKind::Regenerate, Mode::Remove, path, LanguageId::Python)
    }
}

#[tokio::test]
async fn remove_mode_cites_each_item_at_its_own_anchor() {
    let llm = FakeLlm::new(sample());
    let eof = MetaItem {
        anchor: anchor("end_of_file", None, ""),
        ..item("C4", Some("# The end."), None, Position::Dangling)
    };
    let items = vec![
        item(
            "C1",
            Some("# Leading note."),
            Some("load"),
            Position::Leading,
        ),
        item(
            "C2",
            Some("# Trailing note."),
            Some("save"),
            Position::Trailing,
        ),
        item("C3", Some("# Before a closer."), None, Position::Dangling),
        eof.clone(),
    ];
    let reply: RemoveModeRegenerationResponse =
        ask(&llm, remove_meta("src/other.py", 0, items.clone()))
            .await
            .expect("a valid reply");
    assert_eq!(reply.comments.len(), 4, "no extras for this path");
    let expected = [
        (&items[0].anchor, CitedPosition::Leading, "Leading note."),
        (&items[1].anchor, CitedPosition::Trailing, "Trailing note."),
        // `dangling` is only expressible as `leading` on the wire.
        (&items[2].anchor, CitedPosition::Leading, "Before a closer."),
        (&eof.anchor, CitedPosition::Leading, "The end."),
    ];
    for (comment, (anchor, position, text)) in reply.comments.iter().zip(expected) {
        assert_eq!(&comment.anchor, anchor);
        assert_eq!(comment.position, position);
        assert_eq!(comment.comment, text);
        assert_eq!(comment.difficulty, Difficulty::Difficult);
    }
}

#[tokio::test]
async fn remove_mode_omits_declined_comments_and_appends_extras() {
    let llm = FakeLlm::new(sample());
    let items = vec![
        item(
            "C1",
            Some("# Retries are capped because the upstream rate-limits bursts."),
            None,
            Position::Leading,
        ),
        item("C2", Some("# Kept."), None, Position::Leading),
    ];
    let reply: RemoveModeRegenerationResponse =
        ask(&llm, remove_meta("src/worker.py", 0, items.clone()))
            .await
            .expect("a valid reply");
    let texts: Vec<&str> = reply.comments.iter().map(|c| c.comment.as_str()).collect();
    assert_eq!(
        texts,
        ["Kept.", "Drains the queue until the sentinel arrives."],
        "the declined comment is omitted; the extra comes last"
    );
    let extra = &reply.comments[1];
    assert_eq!(
        extra.anchor,
        anchor("function_definition", Some("drain"), "def drain(queue):")
    );
    assert_eq!(extra.position, CitedPosition::Leading);
    assert_eq!(extra.difficulty, Difficulty::Medium);

    // Extras belong to their chunk (default 0) only.
    let reply: RemoveModeRegenerationResponse = ask(&llm, remove_meta("src/worker.py", 1, items))
        .await
        .expect("a valid reply");
    let texts: Vec<&str> = reply.comments.iter().map(|c| c.comment.as_str()).collect();
    assert_eq!(texts, ["Kept."]);
}

#[tokio::test]
async fn remove_mode_items_without_an_original_or_script_cite_nothing() {
    let llm = FakeLlm::new(Scenario::default());
    let items = vec![item("N1", None, Some("unscripted"), Position::Leading)];
    let reply: RemoveModeRegenerationResponse = ask(&llm, remove_meta("x.py", 0, items))
        .await
        .expect("a valid reply");
    assert!(reply.comments.is_empty());
}

// ---------------------------------------------------------------------
// Judge
// ---------------------------------------------------------------------

fn judge_meta(path: &str, requested: &[&str], items: Vec<MetaItem>) -> LlmMeta {
    LlmMeta {
        ids: ids(requested),
        items,
        ..meta(CallKind::Judge, Mode::Redact, path, LanguageId::Typescript)
    }
}

#[tokio::test]
async fn the_judge_answers_exactly_the_packet_ids() {
    let llm = FakeLlm::new(sample());
    let items = vec![
        item(
            "C1",
            Some("// Increment the counter."),
            None,
            Position::Leading,
        ),
        item("C2", Some("// Unlisted."), None, Position::Leading),
        // NEW-origin: no original, so the anchor name is the key.
        item("N1", None, Some("parseHeader"), Position::Leading),
        item("C3", Some("// Not a packet."), None, Position::Leading),
    ];
    let reply: JudgeResponse = ask(&llm, judge_meta("src/a.ts", &["C1", "C2", "N1"], items))
        .await
        .expect("a valid reply");
    let answered: Vec<&str> = reply.decisions.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(answered, ["C1", "C2", "N1"]);

    let c1 = &reply.decisions[0];
    assert_eq!(c1.decision, JudgeDecision::Remove);
    assert_eq!(c1.confidence, 0.95);
    assert_eq!(c1.final_comment, None);

    let c2 = &reply.decisions[1];
    assert_eq!(c2.decision, JudgeDecision::KeepOriginal, "the default rule");
    assert_eq!(c2.confidence, 0.9);
    assert_eq!(c2.final_comment, None);

    let n1 = &reply.decisions[2];
    assert_eq!(n1.decision, JudgeDecision::Hybrid);
    assert_eq!(
        n1.final_comment.as_deref(),
        Some("Parses the header; the magic bytes are checked first.")
    );
    assert_eq!(n1.confidence, 0.7);
}

#[tokio::test]
async fn the_match_key_wins_over_the_anchor_name() {
    let llm = FakeLlm::new(sample());
    let items = vec![item(
        "C1",
        Some("// Increment the counter."),
        Some("parseHeader"),
        Position::Leading,
    )];
    let reply: JudgeResponse = ask(&llm, judge_meta("src/billing.ts", &["C1"], items))
        .await
        .expect("a valid reply");
    let c1 = &reply.decisions[0];
    assert_eq!(c1.decision, JudgeDecision::KeepGenerated);
    assert_eq!(
        c1.final_comment.as_deref(),
        Some("Bump the invoice sequence; gaps are illegal in some jurisdictions.")
    );
}

#[tokio::test]
async fn an_entry_without_the_needed_script_applies_the_default_rule() {
    // The `parseHeader` entry scripts only the judge.
    let llm = FakeLlm::new(sample());
    let items = vec![item(
        "C1",
        Some("// Parse it."),
        Some("parseHeader"),
        Position::Leading,
    )];
    let reply: RegenerationResponse = ask(&llm, redact_meta("src/a.ts", &["C1"], items))
        .await
        .expect("a valid reply");
    assert_eq!(reply.comments[0].comment, "Parse it.");
    assert_eq!(reply.comments[0].difficulty, Difficulty::Difficult);
}

// ---------------------------------------------------------------------
// Call log
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_call_is_logged_in_call_order() {
    let llm = FakeLlm::new(sample());
    assert!(llm.calls().is_empty());
    let first = LlmMeta {
        chunk_index: 2,
        ..redact_meta("src/a.ts", &["C1", "C2"], Vec::new())
    };
    let _: RegenerationResponse = ask(&llm, first).await.expect("a valid reply");
    let _: JudgeResponse = ask(&llm, judge_meta("src/b.ts", &["C7"], Vec::new()))
        .await
        .expect("a valid reply");
    let _ = ask::<RegenerationResponse>(&llm, redact_meta("src/locked.go", &["C1"], Vec::new()))
        .await
        .expect_err("an injected failure");
    assert_eq!(
        llm.calls(),
        [
            CallRecord {
                path: "src/a.ts".to_string(),
                call: CallKind::Regenerate,
                chunk_index: 2,
                ids: ids(&["C1", "C2"]),
            },
            CallRecord {
                path: "src/b.ts".to_string(),
                call: CallKind::Judge,
                chunk_index: 0,
                ids: ids(&["C7"]),
            },
            CallRecord {
                path: "src/locked.go".to_string(),
                call: CallKind::Regenerate,
                chunk_index: 0,
                ids: ids(&["C1"]),
            },
        ],
        "failed calls are logged too"
    );
}

// ---------------------------------------------------------------------
// Failure injection
// ---------------------------------------------------------------------

#[tokio::test]
async fn truncation_fails_exactly_the_first_n_matching_calls() {
    // Sample: the first 2 *regenerate* calls for src/flaky.go truncate.
    let llm = FakeLlm::new(sample());
    let flaky = || redact_meta("src/flaky.go", &["C1"], Vec::new());

    let judge = ask::<JudgeResponse>(&llm, judge_meta("src/flaky.go", &["C1"], Vec::new())).await;
    assert!(
        judge.is_ok(),
        "judge calls do not match a regenerate-only injection"
    );
    let other =
        ask::<RegenerationResponse>(&llm, redact_meta("src/steady.go", &["C1"], Vec::new()));
    assert!(other.await.is_ok(), "other paths do not match");

    for attempt in 1..=2 {
        let result = ask::<RegenerationResponse>(&llm, flaky()).await;
        assert_eq!(
            reason(result),
            LlmErrorReason::Truncated,
            "attempt {attempt}"
        );
    }
    assert!(
        ask::<RegenerationResponse>(&llm, flaky()).await.is_ok(),
        "the third call answers"
    );
    assert_eq!(llm.calls().len(), 5, "truncation spends no repair turn");
}

#[tokio::test]
async fn an_injection_without_times_fails_every_matching_call() {
    let llm = FakeLlm::new(sample());
    for call in [CallKind::Regenerate, CallKind::Judge, CallKind::Regenerate] {
        let meta = LlmMeta {
            call,
            ..redact_meta("src/locked.go", &["C1"], Vec::new())
        };
        let result = complete_typed::<RegenerationResponse>(&llm, request(meta)).await;
        assert_eq!(reason(result), LlmErrorReason::Auth);
    }
}

#[tokio::test]
async fn llm_failures_map_to_exhausted() {
    let llm = FakeLlm::new(scenario(serde_json::json!({
        "failures": [{ "path": "a.ts", "error": "llm", "times": 1 }]
    })));
    let result = ask::<RegenerationResponse>(&llm, redact_meta("a.ts", &["C1"], Vec::new())).await;
    assert_eq!(reason(result), LlmErrorReason::Exhausted);
    let result = ask::<RegenerationResponse>(&llm, redact_meta("a.ts", &["C1"], Vec::new())).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn overlapping_injections_each_count_their_own_matches() {
    // Call 0 is claimed by the first injection; the second still counts
    // it, so its single failure is already spent on call 0.
    let llm = FakeLlm::new(scenario(serde_json::json!({
        "failures": [
            { "path": "a.ts", "error": "auth", "times": 1 },
            { "path": "*", "error": "llm", "times": 1 },
            { "path": "*", "error": "truncated", "times": 2 }
        ]
    })));
    let call = || ask::<RegenerationResponse>(&llm, redact_meta("a.ts", &["C1"], Vec::new()));
    assert_eq!(reason(call().await), LlmErrorReason::Auth);
    assert_eq!(reason(call().await), LlmErrorReason::Truncated);
    assert!(call().await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_any_path_injection_fails_exactly_the_first_call_overall() {
    let llm = Arc::new(FakeLlm::new(scenario(serde_json::json!({
        "failures": [{ "path": "*", "error": "llm", "times": 1 }]
    }))));
    let tasks: Vec<_> = (0..16)
        .map(|n| {
            let llm = Arc::clone(&llm);
            tokio::spawn(async move {
                let path = format!("src/file{n}.ts");
                let result =
                    ask::<RegenerationResponse>(&llm, redact_meta(&path, &["C1"], Vec::new()))
                        .await;
                (path, result)
            })
        })
        .collect();
    let mut failed = Vec::new();
    for task in tasks {
        let (path, result) = task.await.expect("the task completes");
        if let Err(err) = result {
            assert!(
                matches!(
                    err,
                    Error::Llm {
                        reason: LlmErrorReason::Exhausted,
                        ..
                    }
                ),
                "{err:?}"
            );
            failed.push(path);
        }
    }
    let calls = llm.calls();
    assert_eq!(calls.len(), 16);
    assert_eq!(failed, [calls[0].path.clone()], "only call #0 fails");
}

// ---------------------------------------------------------------------
// The on_call hook
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_call_sees_consecutive_indices_matching_the_log() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook_seen = Arc::clone(&seen);
    let llm = Arc::new(FakeLlm::new(Scenario::default()).with_on_call(Arc::new(
        move |index, meta: &LlmMeta| {
            hook_seen
                .lock()
                .expect("unpoisoned")
                .push((index, meta.path.clone()));
        },
    )));
    let tasks: Vec<_> = (0..12)
        .map(|n| {
            let llm = Arc::clone(&llm);
            tokio::spawn(async move {
                let meta = judge_meta(&format!("f{n}.ts"), &["C1"], Vec::new());
                ask::<JudgeResponse>(&llm, meta).await
            })
        })
        .collect();
    for task in tasks {
        task.await
            .expect("the task completes")
            .expect("a valid reply");
    }
    let mut seen = seen.lock().expect("unpoisoned").clone();
    seen.sort();
    let indices: Vec<usize> = seen.iter().map(|(index, _)| *index).collect();
    assert_eq!(indices, (0..12).collect::<Vec<_>>());
    let logged: Vec<String> = llm.calls().into_iter().map(|c| c.path).collect();
    let hooked: Vec<String> = seen.into_iter().map(|(_, path)| path).collect();
    assert_eq!(hooked, logged, "index i is the i-th logged call");
}

#[tokio::test]
async fn on_call_can_cancel_deterministically_after_n_calls() {
    let token = CancellationToken::new();
    let hook_token = token.clone();
    let llm =
        FakeLlm::new(Scenario::default()).with_on_call(Arc::new(move |index, _: &LlmMeta| {
            if index == 2 {
                hook_token.cancel();
            }
        }));
    for n in 0..3 {
        assert!(!token.is_cancelled(), "not cancelled before call {n}");
        let _: JudgeResponse = ask(&llm, judge_meta("a.ts", &["C1"], Vec::new()))
            .await
            .expect("a valid reply");
    }
    assert!(token.is_cancelled(), "cancelled by the third call");
}
