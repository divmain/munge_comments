//! Call #2 orchestration (`engine::judge`), driven by `FakeLlm`.
//!
//! Every test runs the real pipeline up to the judge — inventory, call #1
//! (redact mode) and the draft records it yields — then `judge_file`.
//! Covered: pass-through, routing and per-packet allowed sets, batching
//! (one call per batch, never split by set), truncation halving, each
//! post-judge rail, overlapping rails, set violations and adversarial
//! replies.

use std::sync::{Mutex, PoisonError};

use futures::future::BoxFuture;
use munge_comments::artifact::types::{
    CommentRecord, DecidedBy, Decision, FileWarning, FileWarningKind,
};
use munge_comments::comments::types::Classification;
use munge_comments::engine::heuristics::FULL_SET;
use munge_comments::engine::judge::{CONFIDENCE_FLOOR, JudgeOutput, SANITIZE_FAILED, judge_file};
use munge_comments::engine::{FileInput, MAX_SPLIT_DEPTH, regenerate_file};
use munge_comments::llm::{CallKind, ChatRequest, LlmMeta, Packet, RawResponse, render_packet};
use munge_comments::{
    CapturingLog, DraftRecord, Error, FakeLlm, FileRef, Inventory, LanguageId, LlmClient,
    LlmErrorReason, Mode, PlanConfig, Scenario, inventory_file,
};
use serde_json::{Value, json};

const PATH: &str = "src/app.js";

/// Rewrites the content of a successful judge reply.
type Edit = Box<dyn Fn(&LlmMeta, String) -> String + Send + Sync>;

/// A [`FakeLlm`] whose successful judge replies pass through an edit (to
/// model adversarial output), recording every request it sees.
struct Recording {
    fake: FakeLlm,
    edit: Edit,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Recording {
    fn new(scenario: Scenario, edit: Edit) -> Recording {
        Recording {
            fake: FakeLlm::new(scenario),
            edit,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn honest(scenario: Scenario) -> Recording {
        Recording::new(scenario, Box::new(|_, content| content))
    }

    /// The judge requests, in call order (truncated ones included).
    fn judge_requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|req| req.meta.call == CallKind::Judge)
            .cloned()
            .collect()
    }

    /// The packet ids of each judge request, in call order.
    fn judge_ids(&self) -> Vec<Vec<String>> {
        self.judge_requests()
            .into_iter()
            .map(|req| req.meta.ids)
            .collect()
    }
}

impl LlmClient for Recording {
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
            if req.meta.call == CallKind::Judge && raw.finish_reason.as_deref() != Some("length") {
                raw.content = raw.content.map(|content| (self.edit)(&req.meta, content));
            }
            Ok(raw)
        })
    }
}

fn scenario(value: Value) -> Scenario {
    serde_json::from_value(value).expect("a valid scenario")
}

fn config(max_file_chars: u64) -> PlanConfig {
    PlanConfig {
        mode: Mode::Redact,
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

fn inventory(text: &str) -> Inventory {
    inventory_file(
        text,
        FileRef {
            language: LanguageId::Javascript,
            file_name: "app.js".to_owned(),
        },
    )
    .expect("test sources inventory cleanly")
}

/// Call #1 (redact mode, one chunk) over `text`: the drafts the judge
/// receives.
async fn drafts(text: &str, inventory: &Inventory, llm: &dyn LlmClient) -> Vec<DraftRecord> {
    let input = FileInput {
        path: PATH,
        language: LanguageId::Javascript,
        text,
        inventory,
    };
    let output = regenerate_file(input, &config(120_000), llm, &CapturingLog::new())
        .await
        .expect("call #1 succeeds");
    assert_eq!(output.effective_mode, Mode::Redact);
    output.records
}

/// Call #1 then the judge over `text`, the judge batching to
/// `max_file_chars`.
async fn judge_with(
    text: &str,
    max_file_chars: u64,
    llm: &dyn LlmClient,
) -> (Result<JudgeOutput, Error>, CapturingLog) {
    let inventory = inventory(text);
    let drafts = drafts(text, &inventory, llm).await;
    run_judge(text, &inventory, drafts, max_file_chars, llm).await
}

async fn run_judge(
    text: &str,
    inventory: &Inventory,
    drafts: Vec<DraftRecord>,
    max_file_chars: u64,
    llm: &dyn LlmClient,
) -> (Result<JudgeOutput, Error>, CapturingLog) {
    let input = FileInput {
        path: PATH,
        language: LanguageId::Javascript,
        text,
        inventory,
    };
    let log = CapturingLog::new();
    let output = judge_file(
        input,
        Mode::Redact,
        drafts,
        &config(max_file_chars),
        llm,
        &log,
    )
    .await;
    (output, log)
}

async fn judge(text: &str, llm: &dyn LlmClient) -> JudgeOutput {
    judge_with(text, 120_000, llm)
        .await
        .0
        .expect("the judge succeeds")
}

fn record<'r>(output: &'r JudgeOutput, id: &str) -> &'r CommentRecord {
    output
        .records
        .iter()
        .find(|record| record.id == id)
        .unwrap_or_else(|| panic!("no record {id}"))
}

/// A source of `n` leading comments `// step <i> explains …`, one per
/// statement, ids `C1`…`Cn`.
fn steps(n: usize) -> String {
    (1..=n)
        .map(|i| format!("// step {i} explains why the queue drains here\nrun{i}();\n"))
        .collect()
}

/// The match key of step `i`'s comment.
fn step_key(i: usize) -> String {
    format!("step {i} explains why the queue drains here")
}

/// A call #1 script.
fn regen(comment: &str, difficulty: &str) -> Value {
    json!({ "comment": comment, "difficulty": difficulty })
}

/// A judge script.
fn verdict(decision: &str, final_comment: Option<&str>, confidence: f64) -> Value {
    json!({
        "decision": decision,
        "finalComment": final_comment,
        "confidence": confidence,
        "rationale": "scripted verdict",
    })
}

/// The single-comment source most rail tests use; its key is
/// [`ONE_KEY`].
const ONE: &str = "function total(xs) {\n  // sum the values with a running total\n  \
                   let sum = 0;\n  for (const x of xs) sum += x;\n  return sum;\n}\n";
const ONE_KEY: &str = "sum the values with a running total";

/// Judges [`ONE`] with call #1 answering `regen` and the judge `verdict`.
async fn judge_one(regen: Value, verdict: Value) -> CommentRecord {
    let llm = Recording::honest(scenario(json!({
        "byKey": { ONE_KEY: { "regen": regen, "judge": verdict } }
    })));
    let output = judge(ONE, &llm).await;
    assert_eq!(llm.judge_ids(), [["C1"]], "one packet, one call");
    assert!(output.warnings.is_empty(), "{:?}", output.warnings);
    record(&output, "C1").clone()
}

/// Asserts `record` was decided by a rail: by the engine at confidence 1,
/// its rationale naming `reason` and keeping the judge's own decision and
/// confidence.
fn assert_railed(record: &CommentRecord, decision: Decision, reason: &str, judged: &str) {
    assert_eq!(record.decision, decision, "{record:#?}");
    assert_eq!(record.decided_by, DecidedBy::Engine);
    assert_eq!(record.confidence, 1.0);
    let rationale = record.decision_rationale.as_deref().unwrap_or_default();
    assert!(rationale.starts_with("rail: "), "{rationale}");
    assert!(rationale.contains(reason), "{rationale}");
    assert!(rationale.contains(judged), "{rationale}");
    assert!(rationale.ends_with("scripted verdict"), "{rationale}");
}

// ---------------------------------------------------------------- rails

#[tokio::test]
async fn an_unrailed_verdict_is_the_judges() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict(
            "KEEP_GENERATED",
            Some("Accumulate the values into a sum before returning it."),
            0.85,
        ),
    )
    .await;
    assert_eq!(record.decision, Decision::KeepGenerated);
    assert_eq!(record.decided_by, DecidedBy::Judge);
    assert_eq!(record.confidence, 0.85);
    assert_eq!(
        record.decision_rationale.as_deref(),
        Some("scripted verdict")
    );
    assert_eq!(
        record.final_comment.as_deref(),
        Some("Accumulate the values into a sum before returning it.")
    );
    assert!(record.tags.is_empty(), "{:?}", record.tags);
}

#[tokio::test]
async fn rail_1_holds_an_out_of_set_decision_for_review() {
    // EASY and dissimilar: the restricted set, which cannot rewrite.
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "EASY",
        ),
        verdict(
            "KEEP_GENERATED",
            Some("Accumulate the values into a sum before returning it."),
            0.95,
        ),
    )
    .await;
    assert_railed(
        &record,
        Decision::NeedsReview,
        "outside the allowed set [REMOVE, KEEP_ORIGINAL, NEEDS_REVIEW]",
        "the judge decided KEEP_GENERATED at confidence 0.95",
    );
    // The proposal survives, so a reviewer can approve it.
    assert_eq!(
        record.final_comment.as_deref(),
        Some("Accumulate the values into a sum before returning it.")
    );
}

#[tokio::test]
async fn rail_1_holds_a_null_rewrite_for_review_and_skips_the_later_rails() {
    // Low confidence too: rail 1 wins, and rail 5 does not relabel it.
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict("HYBRID", None, 0.2),
    )
    .await;
    assert_railed(
        &record,
        Decision::NeedsReview,
        "decision HYBRID has a null finalComment",
        "the judge decided HYBRID at confidence 0.2",
    );
    assert_eq!(record.final_comment, None);
}

#[tokio::test]
async fn rail_2_keeps_the_original_when_the_rewrite_fails_sanitization() {
    let leaked = "Accumulate the values, as ⟦C2⟧ explains.";
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict("KEEP_GENERATED", Some(leaked), 0.9),
    )
    .await;
    assert_railed(
        &record,
        Decision::KeepOriginal,
        "the finalComment contains redaction placeholder text",
        "the judge decided KEEP_GENERATED at confidence 0.9",
    );
    assert_eq!(record.tags, [SANITIZE_FAILED]);
    assert_eq!(record.final_comment.as_deref(), Some(leaked));
}

#[tokio::test]
async fn rail_2_validates_against_the_records_own_slot_kind() {
    // `*/` is harmless in a line slot but cannot be written into a block.
    let text = "function total(xs) {\n  /* sum the values with a running total */\n  \
                let sum = 0;\n  for (const x of xs) sum += x;\n  return sum;\n}\n";
    let rewrite = "Sums xs, e.g. lib/**/*.js sizes.";
    let script = || {
        scenario(json!({ "byKey": { ONE_KEY: {
            "regen": regen("Accumulate the values into a sum before returning it.", "MEDIUM"),
            "judge": verdict("HYBRID", Some(rewrite), 0.9),
        } } }))
    };
    let block = judge(text, &Recording::honest(script())).await;
    assert_railed(
        record(&block, "C1"),
        Decision::KeepOriginal,
        "the finalComment contains the block-comment closer",
        "the judge decided HYBRID at confidence 0.9",
    );
    assert_eq!(record(&block, "C1").tags, [SANITIZE_FAILED]);
    let line = judge(ONE, &Recording::honest(script())).await;
    assert_eq!(record(&line, "C1").decision, Decision::Hybrid);
    assert_eq!(record(&line, "C1").decided_by, DecidedBy::Judge);
}

#[tokio::test]
async fn rail_3_resolves_a_tie_to_the_original() {
    // The same match key …
    let same = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict(
            "KEEP_GENERATED",
            Some("sum the values  with a running total"),
            0.9,
        ),
    )
    .await;
    assert_railed(
        &same,
        Decision::KeepOriginal,
        "tie: the finalComment has the original's match key",
        "the judge decided KEEP_GENERATED at confidence 0.9",
    );
    // … or a near-identical merge.
    let near = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict(
            "HYBRID",
            Some("sum the values with a running total, then"),
            0.9,
        ),
    )
    .await;
    assert_railed(
        &near,
        Decision::KeepOriginal,
        "similarity to the original is 0.88",
        "the judge decided HYBRID at confidence 0.9",
    );
    assert!(same.tags.is_empty() && near.tags.is_empty());
}

#[tokio::test]
async fn rail_4_downgrades_a_difficult_rewrite_and_keeps_its_text() {
    let rewrite = "Accumulate the values into a sum before returning it.";
    let record = judge_one(
        regen(rewrite, "DIFFICULT"),
        verdict("KEEP_GENERATED", Some(rewrite), 0.95),
    )
    .await;
    assert_railed(
        &record,
        Decision::NeedsReview,
        "a DIFFICULT comment keeps its original or goes to review",
        "the judge decided KEEP_GENERATED at confidence 0.95",
    );
    assert_eq!(record.final_comment.as_deref(), Some(rewrite));
}

#[tokio::test]
async fn rail_4_lets_a_difficult_keep_original_stand() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "DIFFICULT",
        ),
        verdict("KEEP_ORIGINAL", None, 0.9),
    )
    .await;
    assert_eq!(record.decision, Decision::KeepOriginal);
    assert_eq!(record.decided_by, DecidedBy::Judge);
    assert_eq!(record.confidence, 0.9);
}

#[tokio::test]
async fn rail_5_sends_a_low_confidence_verdict_to_review() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict("REMOVE", None, 0.5),
    )
    .await;
    assert_railed(
        &record,
        Decision::NeedsReview,
        &format!("confidence 0.5 is below the {CONFIDENCE_FLOOR} floor"),
        "the judge decided REMOVE at confidence 0.5",
    );
    // Exactly at the floor is confident enough.
    let at_floor = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict("REMOVE", None, CONFIDENCE_FLOOR),
    )
    .await;
    assert_eq!(at_floor.decision, Decision::Remove);
    assert_eq!(at_floor.decided_by, DecidedBy::Judge);
}

#[tokio::test]
async fn review_rails_leave_the_judges_own_needs_review_alone() {
    for difficulty in ["DIFFICULT", "MEDIUM"] {
        let record = judge_one(
            regen(
                "Accumulate the values into a sum before returning it.",
                difficulty,
            ),
            verdict("NEEDS_REVIEW", None, 0.3),
        )
        .await;
        assert_eq!(record.decision, Decision::NeedsReview);
        assert_eq!(record.decided_by, DecidedBy::Judge, "{difficulty}");
        assert_eq!(record.confidence, 0.3);
    }
}

// ------------------------------------------------------ overlapping rails

#[tokio::test]
async fn a_low_confidence_tie_keeps_the_original() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict(
            "KEEP_GENERATED",
            Some("Sum the values with a running total."),
            0.3,
        ),
    )
    .await;
    assert_railed(
        &record,
        Decision::KeepOriginal,
        "tie:",
        "the judge decided KEEP_GENERATED at confidence 0.3",
    );
}

#[tokio::test]
async fn a_difficult_tie_keeps_the_original() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "DIFFICULT",
        ),
        verdict("HYBRID", Some("Sum the values with a running total."), 0.9),
    )
    .await;
    assert_railed(
        &record,
        Decision::KeepOriginal,
        "tie:",
        "the judge decided HYBRID at confidence 0.9",
    );
}

#[tokio::test]
async fn a_difficult_low_confidence_remove_is_downgraded_by_rail_4() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "DIFFICULT",
        ),
        verdict("REMOVE", None, 0.4),
    )
    .await;
    assert_railed(
        &record,
        Decision::NeedsReview,
        "a DIFFICULT comment keeps its original or goes to review",
        "the judge decided REMOVE at confidence 0.4",
    );
    assert!(
        !record
            .decision_rationale
            .as_deref()
            .unwrap_or_default()
            .contains("floor")
    );
}

#[tokio::test]
async fn a_sanitize_failure_at_low_confidence_keeps_the_original() {
    let record = judge_one(
        regen(
            "Accumulate the values into a sum before returning it.",
            "MEDIUM",
        ),
        verdict(
            "KEEP_GENERATED",
            Some("Accumulate \u{202E}the values."),
            0.2,
        ),
    )
    .await;
    assert_railed(
        &record,
        Decision::KeepOriginal,
        "the finalComment contains the forbidden character U+202E",
        "the judge decided KEEP_GENERATED at confidence 0.2",
    );
    assert_eq!(record.tags, [SANITIZE_FAILED]);
}

// ------------------------------------------------- routing and packet sets

/// Steps 1–6 routed six ways, then a tag-flagged and a protected comment.
fn routing_source() -> String {
    format!(
        "{}// TODO: drain in batches\nlater();\n// eslint-disable-next-line no-console\n\
         console.log(1);\n",
        steps(6)
    )
}

fn routing_scenario() -> Scenario {
    let dissimilar = "Flush pending work before the next tick.";
    scenario(json!({ "byKey": {
        // Row 6: EASY, dissimilar → restricted.
        step_key(1): { "regen": regen(dissimilar, "EASY") },
        // Rows 7–8 → full.
        step_key(2): { "regen": regen(dissimilar, "MEDIUM") },
        step_key(3): { "regen": regen(dissimilar, "DIFFICULT") },
        // Row 9: no regeneration → reduced, whatever the difficulty.
        step_key(4): { "regen": {
            "comment": "", "difficulty": "MEDIUM", "wouldYouCommentHere": false
        } },
        // Row 4: EASY + would not comment here → REMOVE.
        step_key(5): { "regen": {
            "comment": dissimilar, "difficulty": "EASY", "wouldYouCommentHere": false
        } },
        // Row 5: EASY + a restatement → REMOVE.
        step_key(6): { "regen": regen(&step_key(6), "EASY") },
    } }))
}

#[tokio::test]
async fn undecided_drafts_are_routed_and_decided_drafts_pass_through() {
    let text = routing_source();
    let llm = Recording::honest(routing_scenario());
    let inventory = inventory(&text);
    let drafts = drafts(&text, &inventory, &llm).await;
    let (output, _) = run_judge(&text, &inventory, drafts.clone(), 120_000, &llm).await;
    let output = output.expect("the judge succeeds");

    // Every draft yields one record, in draft order.
    let ids: Vec<&str> = output.records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"]);
    // Drafts decided before the judge are finalized untouched.
    for id in ["C7", "C8"] {
        let draft = drafts.iter().find(|draft| draft.id == id).expect("draft");
        assert!(draft.decision.is_some());
        let finalized =
            munge_comments::finalize_record(draft.clone()).expect("a decided draft finalizes");
        assert_eq!(record(&output, id), &finalized);
    }
    assert_eq!(record(&output, "C7").decided_by, DecidedBy::Heuristic);
    assert_eq!(record(&output, "C8").decision, Decision::KeepProtected);
    assert_eq!(
        record(&output, "C8").classification,
        Classification::Functional
    );
    // Short-circuits are the heuristic's, at confidence 1.
    for id in ["C5", "C6"] {
        let short = record(&output, id);
        assert_eq!(short.decision, Decision::Remove, "{id}");
        assert_eq!(short.decided_by, DecidedBy::Heuristic);
        assert_eq!(short.confidence, 1.0);
    }
    // Everything else went to the judge (the fake's default verdict).
    for id in ["C1", "C2", "C3", "C4"] {
        let judged = record(&output, id);
        assert_eq!(judged.decision, Decision::KeepOriginal, "{id}");
        assert_eq!(judged.decided_by, DecidedBy::Judge);
        assert_eq!(judged.confidence, 0.9);
    }
    assert_eq!(llm.judge_ids(), [["C1", "C2", "C3", "C4"]]);
}

#[tokio::test]
async fn each_packet_carries_its_own_allowed_set_in_one_mixed_batch() {
    let text = routing_source();
    let llm = Recording::honest(routing_scenario());
    judge(&text, &llm).await;
    let requests = llm.judge_requests();
    assert_eq!(requests.len(), 1, "batches are never split by decision set");
    let prompt: String = requests[0]
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let restricted = "REMOVE, KEEP_ORIGINAL, NEEDS_REVIEW";
    let full = "REMOVE, KEEP_ORIGINAL, KEEP_GENERATED, HYBRID, NEEDS_REVIEW";
    for (id, allowed) in [
        ("C1", restricted),
        ("C2", full),
        ("C3", full),
        ("C4", restricted),
    ] {
        let section = format!("### {id}\nAllowed decisions: {allowed}\n");
        assert!(prompt.contains(&section), "{id}: {prompt}");
    }
    assert!(prompt.contains(
        "### C4\nAllowed decisions: REMOVE, KEEP_ORIGINAL, NEEDS_REVIEW\nDifficulty: MEDIUM"
    ));
    assert!(
        prompt.contains("Regenerated: none"),
        "C4 has no regeneration"
    );

    // The meta lets a double answer without parsing the prompt.
    let meta = &requests[0].meta;
    assert_eq!(
        (
            meta.call,
            meta.mode,
            meta.path.as_str(),
            meta.language,
            meta.chunk_index
        ),
        (
            CallKind::Judge,
            Mode::Redact,
            PATH,
            LanguageId::Javascript,
            0
        )
    );
    let item_ids: Vec<&str> = meta.items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(item_ids, meta.ids);
    let first = &meta.items[0];
    assert_eq!(
        first.original.as_deref(),
        Some("// step 1 explains why the queue drains here")
    );
    assert_eq!(
        first.regenerated.as_deref(),
        Some("Flush pending work before the next tick.")
    );
    // The snippet excises comments: the next lines of code follow.
    assert_eq!(first.anchor.snippet, "run1();\nrun2();");
    assert_eq!(meta.items[3].regenerated, None);
}

#[tokio::test]
async fn a_set_violation_on_each_set_is_held_for_review() {
    let rewrite = "Flush pending work before the next tick.";
    let llm = Recording::honest(scenario(json!({ "byKey": {
        // Restricted: no rewrites.
        step_key(1): { "regen": regen(rewrite, "EASY"),
                       "judge": verdict("HYBRID", Some(rewrite), 0.9) },
        // Reduced: nothing to keep.
        step_key(2): { "regen": {
                           "comment": "", "difficulty": "MEDIUM", "wouldYouCommentHere": false
                       },
                       "judge": verdict("KEEP_GENERATED", Some(rewrite), 0.9) },
        // Full: the same decision is allowed.
        step_key(3): { "regen": regen(rewrite, "MEDIUM"),
                       "judge": verdict("KEEP_GENERATED", Some(rewrite), 0.9) },
    } })));
    let output = judge(&steps(3), &llm).await;
    assert_railed(
        record(&output, "C1"),
        Decision::NeedsReview,
        "decision HYBRID is outside the allowed set",
        "the judge decided HYBRID at confidence 0.9",
    );
    assert_railed(
        record(&output, "C2"),
        Decision::NeedsReview,
        "decision KEEP_GENERATED is outside the allowed set",
        "the judge decided KEEP_GENERATED at confidence 0.9",
    );
    assert_eq!(record(&output, "C3").decision, Decision::KeepGenerated);
    assert_eq!(record(&output, "C3").decided_by, DecidedBy::Judge);
}

#[tokio::test]
async fn a_file_whose_drafts_all_short_circuit_makes_no_judge_call() {
    let llm = Recording::honest(scenario(json!({ "byKey": {
        step_key(1): { "regen": regen(&step_key(1), "EASY") },
        step_key(2): { "regen": {
            "comment": "", "difficulty": "EASY", "wouldYouCommentHere": false
        } },
    } })));
    let output = judge(&steps(2), &llm).await;
    assert!(llm.judge_requests().is_empty());
    assert!(
        output
            .records
            .iter()
            .all(|r| r.decision == Decision::Remove && r.decided_by == DecidedBy::Heuristic)
    );
}

#[tokio::test]
async fn new_origin_records_pass_through_and_must_arrive_decided() {
    let text = steps(1);
    let llm = Recording::honest(Scenario::default());
    let inventory = inventory(&text);
    let mut drafts = drafts(&text, &inventory, &llm).await;
    let mut new = drafts[0].clone();
    new.id = "N1".to_owned();
    new.original = None;
    new.final_comment = Some("Drains the queue.".to_owned());
    new.decision = None;
    new.decide(Decision::New, DecidedBy::Engine, 1.0);
    drafts.push(new.clone());

    let (output, _) = run_judge(&text, &inventory, drafts.clone(), 120_000, &llm).await;
    let output = output.expect("the judge succeeds");
    assert_eq!(llm.judge_ids(), [["C1"]], "the NEW record is no packet");
    assert_eq!(
        record(&output, "N1"),
        &munge_comments::finalize_record(new.clone()).expect("decided")
    );

    let mut undecided = drafts;
    undecided[1].decision = None;
    let (output, _) = run_judge(&text, &inventory, undecided, 120_000, &llm).await;
    assert!(
        matches!(output, Err(Error::Invariant(ref message)) if message.contains("N1")),
        "{output:?}"
    );
}

// ------------------------------------------------------------- batching

/// The estimated token cost of each judge packet of `drafts`, as the
/// batcher measures it: `ceil(chars / 4)` of its rendered prompt section.
fn packet_costs(drafts: &[DraftRecord]) -> Vec<(String, usize)> {
    drafts
        .iter()
        .filter(|draft| draft.decision.is_none())
        .map(|draft| {
            let packet = Packet {
                id: draft.id.clone(),
                anchor_snippet: draft.anchor.snippet.clone(),
                original: draft.original.clone().expect("a scanned draft"),
                regenerated: draft.regenerated.clone(),
                difficulty: draft.difficulty,
                difficulty_rationale: draft.difficulty_rationale.clone(),
                would_you_comment_here: draft.would_you_comment_here,
                allowed: FULL_SET.to_vec(),
            };
            let chars = render_packet(&packet, LanguageId::Javascript)
                .chars()
                .count();
            (draft.id.clone(), chars.div_ceil(4))
        })
        .collect()
}

#[tokio::test]
async fn packets_are_batched_by_token_estimate_with_one_call_per_batch() {
    let text = steps(5);
    let llm = Recording::honest(Scenario::default());
    let inventory = inventory(&text);
    let drafts = drafts(&text, &inventory, &llm).await;
    let costs = packet_costs(&drafts);
    assert_eq!(costs.len(), 5);
    // A budget of exactly C1 + C2; C3 + C4 fit it too (the same size).
    assert_eq!(
        costs[0].1 + costs[1].1,
        costs[2].1 + costs[3].1,
        "{costs:?}"
    );
    let max_file_chars = u64::try_from(4 * (costs[0].1 + costs[1].1)).expect("small");
    let (output, _) = run_judge(&text, &inventory, drafts, max_file_chars, &llm).await;
    let output = output.expect("the judge succeeds");
    assert_eq!(
        llm.judge_ids(),
        [vec!["C1", "C2"], vec!["C3", "C4"], vec!["C5"]]
    );
    let batch_indices: Vec<usize> = llm
        .judge_requests()
        .iter()
        .map(|req| req.meta.chunk_index)
        .collect();
    assert_eq!(batch_indices, [0, 1, 2]);
    assert!(
        output
            .records
            .iter()
            .all(|r| r.decision == Decision::KeepOriginal && r.decided_by == DecidedBy::Judge)
    );
}

#[tokio::test]
async fn a_packet_over_the_budget_is_a_batch_of_its_own() {
    let llm = Recording::honest(Scenario::default());
    let (output, _) = judge_with(&steps(3), 4, &llm).await;
    output.expect("the judge succeeds");
    assert_eq!(llm.judge_ids(), [["C1"], ["C2"], ["C3"]]);
}

fn truncating(times: Option<u32>) -> Scenario {
    scenario(json!({
        "failures": [{ "path": PATH, "call": "judge", "error": "truncated", "times": times }]
    }))
}

#[tokio::test]
async fn a_truncated_batch_is_retried_as_halves_of_its_packets() {
    let llm = Recording::honest(truncating(Some(1)));
    let (output, log) = judge_with(&steps(5), 120_000, &llm).await;
    let output = output.expect("the halves succeed");
    assert_eq!(
        llm.judge_ids(),
        [
            vec!["C1", "C2", "C3", "C4", "C5"],
            vec!["C1", "C2", "C3"],
            vec!["C4", "C5"],
        ]
    );
    // The halves keep their batch's index.
    assert!(
        llm.judge_requests()
            .iter()
            .all(|req| req.meta.chunk_index == 0)
    );
    assert!(
        output
            .records
            .iter()
            .all(|r| r.decided_by == DecidedBy::Judge)
    );
    assert!(
        log.lines()
            .iter()
            .any(|line| line.contains("judge batch 0 truncated; retrying its 5 packets as 3 + 2")),
        "{:?}",
        log.lines()
    );
}

#[tokio::test]
async fn a_truncation_that_cannot_be_split_fails_the_file() {
    // A single packet cannot be halved.
    let llm = Recording::honest(truncating(None));
    let (output, _) = judge_with(&steps(1), 120_000, &llm).await;
    assert!(matches!(
        output,
        Err(Error::Llm {
            reason: LlmErrorReason::Truncated,
            ..
        })
    ));
    assert_eq!(llm.judge_ids().len(), 1);

    // Halving stops at the depth limit: 16 → 8 → 4 → 2, then fail.
    let llm = Recording::honest(truncating(None));
    let (output, _) = judge_with(&steps(16), 120_000, &llm).await;
    let sizes: Vec<usize> = llm.judge_ids().iter().map(Vec::len).collect();
    assert_eq!(sizes, [16, 8, 4, 2]);
    assert_eq!(
        sizes.len(),
        usize::try_from(MAX_SPLIT_DEPTH).expect("small") + 1
    );
    match output {
        Err(Error::Llm {
            reason: LlmErrorReason::Truncated,
            message,
        }) => assert!(message.contains("split depth 3"), "{message}"),
        other => panic!("expected a truncation failure, got {other:?}"),
    }
}

// --------------------------------------------------- adversarial replies

#[tokio::test]
async fn an_unknown_id_is_dropped_with_a_warning_and_a_missing_id_is_held() {
    let llm = Recording::new(
        Scenario::default(),
        Box::new(|_, content| {
            let mut reply: Value = serde_json::from_str(&content).expect("JSON");
            let decisions = reply["decisions"].as_array_mut().expect("decisions");
            decisions.retain(|decision| decision["id"] != "C2");
            decisions.push(json!({
                "id": "C9", "decision": "REMOVE", "finalComment": null,
                "confidence": 1.0, "rationale": "invented",
            }));
            reply.to_string()
        }),
    );
    let output = judge(&steps(3), &llm).await;
    assert_eq!(
        output.warnings,
        [FileWarning {
            kind: FileWarningKind::UnknownId,
            detail: "the judge returned unknown comment id \"C9\"".to_owned(),
        }]
    );
    let ids: Vec<&str> = output.records.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["C1", "C2", "C3"]);
    let missing = record(&output, "C2");
    assert_eq!(missing.decision, Decision::NeedsReview);
    assert_eq!(missing.decided_by, DecidedBy::Engine);
    assert_eq!(missing.confidence, 1.0);
    assert!(
        missing
            .decision_rationale
            .as_deref()
            .unwrap_or_default()
            .contains("returned no decision")
    );
    assert_eq!(record(&output, "C1").decided_by, DecidedBy::Judge);
}

#[tokio::test]
async fn a_judge_failure_fails_the_file() {
    let llm = Recording::honest(scenario(json!({
        "failures": [{ "path": PATH, "call": "judge", "error": "auth" }]
    })));
    let (output, _) = judge_with(&steps(2), 120_000, &llm).await;
    assert!(matches!(
        output,
        Err(Error::Llm {
            reason: LlmErrorReason::Auth,
            ..
        })
    ));
}
