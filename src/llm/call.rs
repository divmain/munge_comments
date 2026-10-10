//! Typed model calls and the post-processing of their responses.
//!
//! [`complete_typed`] is the only way the engine talks to a model: it pins
//! the strict response schema, interprets truncation and refusal, parses and
//! validates the reply, and spends at most one repair turn. The helpers
//! below turn validated wire responses into engine-ready values:
//! [`validate_judge_response`] enforces rail 1 per judge packet, and
//! [`postprocess_regeneration`] normalizes call #1 output and owns its
//! `unknown-id` warnings.

use std::collections::{BTreeMap, HashMap, HashSet};

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::artifact::types::{
    DecidedBy, Decision, Difficulty, FileWarning, FileWarningKind, JudgeDecision,
};
use crate::comments::sanitize::normalize_comment_text;
use crate::errors::{Error, LlmErrorReason};
use crate::lang::adapter::LanguageAdapter;
use crate::llm::client::{ChatRequest, LlmClient, Message, RawResponse};
use crate::llm::schemas::{JudgeResponse, RegenerationResponse, WireValidate, strict_schema};

/// The `response_format` for wire type `T`: a strict `json_schema` object
/// carrying [`strict_schema::<T>`](strict_schema).
pub fn response_format<T: JsonSchema>() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": T::schema_name(),
            "strict": true,
            "schema": strict_schema::<T>(),
        }
    })
}

/// Sends `req` and returns its validated, typed response.
///
/// 1. `response_format` is set to [`response_format::<T>`](response_format).
/// 2. Some message must contain the literal word `JSON` (the `json_object`
///    fallback requires it); otherwise `Error::Invariant` — a prompt-builder
///    bug.
/// 3. `finish_reason == "length"` → `Error::Llm { reason: Truncated }`; a
///    non-null `refusal` → `reason: Refusal`. Neither spends a repair.
/// 4. The content (markdown fences stripped) is deserialized into `T` and
///    checked with [`WireValidate::validate`].
/// 5. On a parse or validation failure, the request is retried once with
///    the raw reply and a user message listing the errors appended; a
///    second failure → `Error::Llm { reason: Invalid }`.
pub async fn complete_typed<T>(client: &dyn LlmClient, mut req: ChatRequest) -> Result<T, Error>
where
    T: DeserializeOwned + JsonSchema + WireValidate,
{
    req.response_format = response_format::<T>();
    if !req.messages.iter().any(|m| m.content.contains("JSON")) {
        return Err(Error::Invariant(format!(
            "no prompt message for the {} call contains the word \"JSON\"",
            T::schema_name()
        )));
    }
    let mut repaired = false;
    loop {
        let raw = client.complete_json(&req).await?;
        let content = interpret(&raw)?;
        let errors = match parse_and_validate::<T>(content) {
            Ok(value) => return Ok(value),
            Err(errors) => errors,
        };
        if repaired {
            return Err(Error::Llm {
                reason: LlmErrorReason::Invalid,
                message: format!(
                    "{} response invalid after one repair: {}",
                    T::schema_name(),
                    errors.join("; ")
                ),
            });
        }
        repaired = true;
        req.messages.push(Message::assistant(content));
        req.messages.push(Message::user(repair_prompt(&errors)));
    }
}

/// Maps truncation and refusal to their errors; otherwise yields the
/// content (empty when absent, which then fails parsing and is repaired).
fn interpret(raw: &RawResponse) -> Result<&str, Error> {
    if raw.finish_reason.as_deref() == Some("length") {
        return Err(Error::Llm {
            reason: LlmErrorReason::Truncated,
            message: "the response hit the token limit (finish_reason: length)".to_string(),
        });
    }
    if let Some(refusal) = &raw.refusal {
        return Err(Error::Llm {
            reason: LlmErrorReason::Refusal,
            message: format!("the model refused: {refusal}"),
        });
    }
    Ok(raw.content.as_deref().unwrap_or(""))
}

fn parse_and_validate<T: DeserializeOwned + WireValidate>(content: &str) -> Result<T, Vec<String>> {
    let body = strip_fences(content);
    if body.is_empty() {
        return Err(vec![
            "the reply was empty; expected a JSON object".to_string(),
        ]);
    }
    let value: T = serde_json::from_str(body).map_err(|error| {
        vec![format!(
            "the reply is not valid JSON for the schema: {error}"
        )]
    })?;
    value.validate()?;
    Ok(value)
}

/// Strips a markdown code fence the model wrapped around its JSON: when the
/// trimmed content starts with ```` ``` ````, the first line (with any
/// language tag) and a trailing ```` ``` ```` are dropped.
fn strip_fences(content: &str) -> &str {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let body = trimmed.split_once('\n').map_or("", |(_, rest)| rest);
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
}

fn repair_prompt(errors: &[String]) -> String {
    let mut prompt = String::from("Your previous reply could not be accepted:\n");
    for error in errors {
        prompt.push_str("- ");
        prompt.push_str(error);
        prompt.push('\n');
    }
    prompt.push_str(
        "Reply again with the corrected JSON only: a single JSON object matching the \
         schema, with no prose and no code fences.",
    );
    prompt
}

/// One comment presented to the judge, with the decisions it may receive.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Packet {
    /// The comment id, e.g. `C12`.
    pub id: String,
    /// The anchored code snippet.
    pub anchor_snippet: String,
    /// The original comment text.
    pub original: String,
    /// The regenerated comment; `None` puts the packet in the reduced set.
    pub regenerated: Option<String>,
    /// Call #1's difficulty rating, if it rated this comment.
    pub difficulty: Option<Difficulty>,
    /// Why that difficulty.
    pub difficulty_rationale: Option<String>,
    /// Redact mode only; `None` in remove mode or when call #1 never
    /// answered this id.
    pub would_you_comment_here: Option<bool>,
    /// The decisions this packet may receive (full, restricted or reduced).
    pub allowed: Vec<Decision>,
}

/// The judge's verdict on one packet after rail 1.
///
/// `decided_by` is [`DecidedBy::Judge`] when the verdict is the judge's own,
/// and [`DecidedBy::Engine`] when rail 1 replaced it with `NEEDS_REVIEW`.
/// Such a rail outcome has confidence 1, keeps the judge's proposed
/// `final_comment`, and records the judge's decision and confidence in
/// `rationale`, so later rails are skipped (first match wins) and a reviewer
/// can still approve the proposed rewrite.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeOutcome {
    /// The decision, after rail 1.
    pub decision: JudgeDecision,
    /// The judge's proposed text (kept even when railed).
    pub final_comment: Option<String>,
    /// The judge's confidence, or 1 for a rail outcome.
    pub confidence: f64,
    /// The judge's rationale, or the rail's explanation.
    pub rationale: String,
    /// Who decided: the judge, or the engine (rail 1).
    pub decided_by: DecidedBy,
}

impl JudgeOutcome {
    fn railed(reason: String, final_comment: Option<String>) -> JudgeOutcome {
        JudgeOutcome {
            decision: JudgeDecision::NeedsReview,
            final_comment,
            confidence: 1.0,
            rationale: reason,
            decided_by: DecidedBy::Engine,
        }
    }
}

/// Applies rail 1 to a validated judge response: every packet id gets an
/// outcome, and only ids of `packets` are kept.
///
/// - A packet the judge did not answer → `NEEDS_REVIEW`.
/// - An unknown (extra) id is dropped with an `unknown-id` warning.
/// - A decision outside the packet's allowed set → `NEEDS_REVIEW`.
/// - `KEEP_GENERATED`/`HYBRID` with a null `finalComment` →
///   `NEEDS_REVIEW`.
///
/// Ids are unique within a validated response ([`WireValidate`]).
pub fn validate_judge_response(
    resp: JudgeResponse,
    packets: &[Packet],
) -> (BTreeMap<String, JudgeOutcome>, Vec<FileWarning>) {
    let by_id: HashMap<&str, &Packet> = packets.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut outcomes = BTreeMap::new();
    let mut warnings = Vec::new();
    for item in resp.decisions {
        let Some(packet) = by_id.get(item.id.as_str()) else {
            warnings.push(FileWarning {
                kind: FileWarningKind::UnknownId,
                detail: format!("the judge returned unknown comment id \"{}\"", item.id),
            });
            continue;
        };
        let decision = Decision::from(item.decision);
        let rail = if !packet.allowed.contains(&decision) {
            Some(format!(
                "decision {} is outside the allowed set [{}]",
                decision.as_str(),
                packet
                    .allowed
                    .iter()
                    .map(|d| d.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else if decision.writes_final_comment() && item.final_comment.is_none() {
            Some(format!(
                "decision {} has a null finalComment",
                decision.as_str()
            ))
        } else {
            None
        };
        let outcome = match rail {
            Some(reason) => JudgeOutcome::railed(
                format!(
                    "rail: {reason}; the judge decided {} at confidence {}: {}",
                    decision.as_str(),
                    item.confidence,
                    item.rationale
                ),
                item.final_comment,
            ),
            None => JudgeOutcome {
                decision: item.decision,
                final_comment: item.final_comment,
                confidence: item.confidence,
                rationale: item.rationale,
                decided_by: DecidedBy::Judge,
            },
        };
        outcomes.insert(item.id, outcome);
    }
    for packet in packets {
        if !outcomes.contains_key(&packet.id) {
            outcomes.insert(
                packet.id.clone(),
                JudgeOutcome::railed(
                    "rail: the judge returned no decision for this id".to_string(),
                    None,
                ),
            );
        }
    }
    (outcomes, warnings)
}

/// One redact-mode regeneration after post-processing.
#[derive(Debug, Clone, PartialEq)]
pub struct Regeneration {
    /// The placeholder id (one of the requested ids).
    pub id: String,
    /// Whether the model would comment here at all.
    pub would_you_comment_here: bool,
    /// The normalized comment text; `None` when the model returned no
    /// content (allowed only with `wouldYouCommentHere: false`, or text that
    /// normalization reduced to nothing).
    pub regenerated: Option<String>,
    /// The difficulty rating.
    pub difficulty: Difficulty,
    /// Why that difficulty.
    pub rationale: String,
    /// What context would have helped, captured verbatim.
    pub missing_context: Option<String>,
}

/// Post-processes a validated redact-mode call #1 response.
///
/// Every comment's text goes through [`normalize_comment_text`]; a comment
/// that is empty after normalization becomes `regenerated: None`. Ids that
/// were not requested are dropped with one `unknown-id` warning each. This
/// is the single owner of `unknown-id` warnings for call #1: callers only
/// collect the warnings returned here.
pub fn postprocess_regeneration(
    resp: RegenerationResponse,
    requested_ids: &[String],
    adapter: &LanguageAdapter,
) -> (Vec<Regeneration>, Vec<FileWarning>) {
    let requested: HashSet<&str> = requested_ids.iter().map(String::as_str).collect();
    let mut regenerations = Vec::new();
    let mut warnings = Vec::new();
    for comment in resp.comments {
        if !requested.contains(comment.id.as_str()) {
            warnings.push(FileWarning {
                kind: FileWarningKind::UnknownId,
                detail: format!(
                    "the regeneration returned unrequested comment id \"{}\"",
                    comment.id
                ),
            });
            continue;
        }
        let normalized = normalize_comment_text(&comment.comment, adapter);
        let regenerated = (!normalized.trim().is_empty()).then_some(normalized);
        regenerations.push(Regeneration {
            id: comment.id,
            would_you_comment_here: comment.would_you_comment_here,
            regenerated,
            difficulty: comment.difficulty,
            rationale: comment.rationale,
            missing_context: comment.missing_context,
        });
    }
    (regenerations, warnings)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::future::BoxFuture;

    use super::*;
    use crate::config::Mode;
    use crate::lang::adapters::adapter_for;
    use crate::lang::ids::LanguageId;
    use crate::llm::client::{CallKind, LlmMeta, Role};
    use crate::llm::schemas::{JudgeItem, RegeneratedComment, RemoveModeRegenerationResponse};

    /// Replays scripted responses in order and records every request.
    struct ScriptedLlm {
        script: Mutex<VecDeque<Result<RawResponse, Error>>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedLlm {
        fn new(script: Vec<Result<RawResponse, Error>>) -> ScriptedLlm {
            ScriptedLlm {
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn replies(contents: &[&str]) -> ScriptedLlm {
            ScriptedLlm::new(contents.iter().map(|c| Ok(content(c))).collect())
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl LlmClient for ScriptedLlm {
        fn complete_json<'a>(
            &'a self,
            req: &'a ChatRequest,
        ) -> BoxFuture<'a, Result<RawResponse, Error>> {
            self.requests.lock().unwrap().push(req.clone());
            let next = self.script.lock().unwrap().pop_front();
            Box::pin(async move {
                next.unwrap_or_else(|| Err(Error::Invariant("script exhausted".to_string())))
            })
        }
    }

    fn content(text: &str) -> RawResponse {
        RawResponse {
            content: Some(text.to_string()),
            finish_reason: Some("stop".to_string()),
            refusal: None,
        }
    }

    fn request(call: CallKind) -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![
                Message::system("You review comments."),
                Message::user("Answer with JSON matching the schema."),
            ],
            response_format: Value::Null,
            temperature: Some(0.2),
            max_completion_tokens: 16_384,
            meta: LlmMeta {
                call,
                mode: Mode::Redact,
                path: "src/a.ts".to_string(),
                language: LanguageId::Typescript,
                chunk_index: 0,
                ids: vec!["C1".to_string()],
                items: Vec::new(),
            },
        }
    }

    const JUDGE_OK: &str = r#"{"decisions": [{"id": "C1", "decision": "REMOVE",
        "finalComment": null, "confidence": 0.9, "rationale": "obvious"}]}"#;

    async fn judge(llm: &ScriptedLlm) -> Result<JudgeResponse, Error> {
        complete_typed::<JudgeResponse>(llm, request(CallKind::Judge)).await
    }

    fn reason(result: Result<impl std::fmt::Debug, Error>) -> LlmErrorReason {
        match result {
            Err(Error::Llm { reason, .. }) => reason,
            other => panic!("expected an LLM error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_valid_reply_needs_one_call_and_carries_the_strict_schema() {
        let llm = ScriptedLlm::replies(&[JUDGE_OK]);
        let response = judge(&llm).await.unwrap();
        assert_eq!(response.decisions[0].decision, JudgeDecision::Remove);
        let requests = llm.requests();
        assert_eq!(requests.len(), 1);
        let format = &requests[0].response_format;
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["json_schema"]["name"], "JudgeResponse");
        assert_eq!(format["json_schema"]["strict"], true);
        assert_eq!(
            format["json_schema"]["schema"],
            strict_schema::<JudgeResponse>()
        );
    }

    #[tokio::test]
    async fn a_prompt_without_the_word_json_is_an_invariant_violation() {
        let llm = ScriptedLlm::replies(&[JUDGE_OK]);
        let mut req = request(CallKind::Judge);
        req.messages = vec![Message::user("answer in json please")];
        let result = complete_typed::<JudgeResponse>(&llm, req).await;
        assert!(matches!(result, Err(Error::Invariant(_))), "{result:?}");
        assert!(llm.requests().is_empty());
    }

    #[tokio::test]
    async fn truncation_maps_to_truncated_without_a_repair() {
        let llm = ScriptedLlm::new(vec![Ok(RawResponse {
            content: Some("{\"decisions\": [".to_string()),
            finish_reason: Some("length".to_string()),
            refusal: None,
        })]);
        assert_eq!(reason(judge(&llm).await), LlmErrorReason::Truncated);
        assert_eq!(llm.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_refusal_maps_to_refusal_without_a_repair() {
        let llm = ScriptedLlm::new(vec![Ok(RawResponse {
            content: None,
            finish_reason: Some("stop".to_string()),
            refusal: Some("I can't help with that.".to_string()),
        })]);
        assert_eq!(reason(judge(&llm).await), LlmErrorReason::Refusal);
        assert_eq!(llm.requests().len(), 1);
    }

    #[tokio::test]
    async fn transport_errors_propagate_unchanged() {
        let llm = ScriptedLlm::new(vec![Err(Error::Llm {
            reason: LlmErrorReason::Http,
            message: "no choices[0]".to_string(),
        })]);
        assert_eq!(reason(judge(&llm).await), LlmErrorReason::Http);
        assert_eq!(llm.requests().len(), 1);
    }

    #[tokio::test]
    async fn fenced_json_is_accepted_without_a_repair() {
        for fenced in [
            format!("```json\n{JUDGE_OK}\n```"),
            format!("  ```\n{JUDGE_OK}\n```  \n"),
        ] {
            let llm = ScriptedLlm::replies(&[&fenced]);
            assert!(judge(&llm).await.is_ok(), "{fenced}");
            assert_eq!(llm.requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn prose_before_json_is_repaired_with_the_raw_reply_and_errors() {
        let prose = format!("Here is my answer:\n{JUDGE_OK}");
        let llm = ScriptedLlm::replies(&[&prose, JUDGE_OK]);
        assert!(judge(&llm).await.is_ok());
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        let repair = &requests[1].messages;
        assert_eq!(repair.len(), 4);
        assert_eq!(repair[2].role, Role::Assistant);
        assert_eq!(repair[2].content, prose);
        assert_eq!(repair[3].role, Role::User);
        assert!(
            repair[3].content.contains("not valid JSON"),
            "{}",
            repair[3].content
        );
        assert!(repair[3].content.contains("corrected JSON only"));
        // The repair turn re-sends the same schema.
        assert_eq!(requests[1].response_format, requests[0].response_format);
    }

    /// Each malformed reply is repaired once; a second failure is `Invalid`.
    #[tokio::test]
    async fn malformed_replies_route_to_repair_then_invalid() {
        let malformed = [
            // wrong types
            r#"{"decisions": [{"id": "C1", "decision": "REMOVE", "finalComment": null,
                "confidence": "high", "rationale": "r"}]}"#,
            r#"{"decisions": {"id": "C1"}}"#,
            r#"["C1", "REMOVE"]"#,
            // missing fields
            r#"{"decisions": [{"id": "C1", "decision": "REMOVE", "confidence": 0.9}]}"#,
            r#"{}"#,
            // extra keys
            r#"{"decisions": [], "summary": "done"}"#,
            r#"{"decisions": [{"id": "C1", "decision": "REMOVE", "finalComment": null,
                "confidence": 0.9, "rationale": "r", "notes": "x"}]}"#,
            // out-of-vocabulary decision
            r#"{"decisions": [{"id": "C1", "decision": "NEW", "finalComment": "x",
                "confidence": 0.9, "rationale": "r"}]}"#,
            // semantic violations
            r#"{"decisions": [{"id": "C1", "decision": "REMOVE", "finalComment": null,
                "confidence": 1.5, "rationale": "r"}]}"#,
            r#"{"decisions": [{"id": "", "decision": "REMOVE", "finalComment": null,
                "confidence": 0.5, "rationale": "r"}]}"#,
            // prose only, and empty content
            "I think C1 should be removed.",
            "",
            "   ",
            "```json\n```",
        ];
        for reply in malformed {
            let repaired = ScriptedLlm::replies(&[reply, JUDGE_OK]);
            assert!(
                judge(&repaired).await.is_ok(),
                "repair failed for {reply:?}"
            );
            assert_eq!(repaired.requests().len(), 2, "{reply:?}");

            let invalid = ScriptedLlm::replies(&[reply, reply]);
            assert_eq!(
                reason(judge(&invalid).await),
                LlmErrorReason::Invalid,
                "{reply:?}"
            );
            assert_eq!(invalid.requests().len(), 2, "{reply:?}");
        }
    }

    #[tokio::test]
    async fn absent_content_is_repaired() {
        let none = RawResponse {
            content: None,
            finish_reason: Some("stop".to_string()),
            refusal: None,
        };
        let llm = ScriptedLlm::new(vec![Ok(none), Ok(content(JUDGE_OK))]);
        assert!(judge(&llm).await.is_ok());
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages[3].content.contains("empty"));
    }

    #[tokio::test]
    async fn truncation_on_the_repair_turn_is_still_truncated() {
        let truncated = RawResponse {
            content: Some("{".to_string()),
            finish_reason: Some("length".to_string()),
            refusal: None,
        };
        let llm = ScriptedLlm::new(vec![Ok(content("nope")), Ok(truncated)]);
        assert_eq!(reason(judge(&llm).await), LlmErrorReason::Truncated);
    }

    fn regeneration_reply(would: bool, comment: &str) -> String {
        serde_json::json!({"comments": [{
            "id": "C1", "wouldYouCommentHere": would, "comment": comment,
            "difficulty": "EASY", "rationale": "r", "missingContext": null
        }]})
        .to_string()
    }

    async fn regenerate(llm: &ScriptedLlm) -> Result<RegenerationResponse, Error> {
        complete_typed::<RegenerationResponse>(llm, request(CallKind::Regenerate)).await
    }

    #[tokio::test]
    async fn an_empty_comment_is_valid_only_when_the_model_would_not_comment() {
        let llm = ScriptedLlm::replies(&[&regeneration_reply(false, "")]);
        assert!(regenerate(&llm).await.is_ok());
        assert_eq!(llm.requests().len(), 1);

        let wanted = regeneration_reply(true, "");
        let fixed = regeneration_reply(true, "why");
        let llm = ScriptedLlm::replies(&[&wanted, &fixed]);
        let response = regenerate(&llm).await.unwrap();
        assert_eq!(response.comments[0].comment, "why");
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].messages[3]
                .content
                .contains("wouldYouCommentHere")
        );
    }

    #[tokio::test]
    async fn a_glob_in_a_regeneration_is_not_rejected_at_call_time() {
        let text = "Matches src/**/*.ts so generated files stay out.";
        let llm = ScriptedLlm::replies(&[&regeneration_reply(true, text)]);
        let response = regenerate(&llm).await.unwrap();
        assert_eq!(response.comments[0].comment, text);
        assert_eq!(llm.requests().len(), 1);
    }

    #[tokio::test]
    async fn remove_mode_replies_parse_and_validate() {
        let reply = serde_json::json!({"comments": [{
            "anchor": {"nodeKind": "function_declaration", "name": "f", "snippet": "function f() {}"},
            "position": "leading", "comment": "why", "difficulty": "MEDIUM",
            "rationale": "r", "missingContext": "history"
        }]})
        .to_string();
        let empty = reply.replace("\"why\"", "\"\"");
        let llm = ScriptedLlm::replies(&[&empty, &reply]);
        let response =
            complete_typed::<RemoveModeRegenerationResponse>(&llm, request(CallKind::Regenerate))
                .await
                .unwrap();
        assert_eq!(response.comments[0].anchor.name.as_deref(), Some("f"));
        assert_eq!(llm.requests().len(), 2);
        assert_eq!(
            llm.requests()[0].response_format["json_schema"]["name"],
            "RemoveModeRegenerationResponse"
        );
    }

    fn packet(id: &str, allowed: &[Decision]) -> Packet {
        Packet {
            id: id.to_string(),
            anchor_snippet: "fn f() {}".to_string(),
            original: "// why".to_string(),
            regenerated: Some("why".to_string()),
            difficulty: Some(Difficulty::Medium),
            difficulty_rationale: Some("r".to_string()),
            would_you_comment_here: Some(true),
            allowed: allowed.to_vec(),
        }
    }

    const FULL: [Decision; 5] = [
        Decision::Remove,
        Decision::KeepOriginal,
        Decision::KeepGenerated,
        Decision::Hybrid,
        Decision::NeedsReview,
    ];
    const REDUCED: [Decision; 3] = [
        Decision::Remove,
        Decision::KeepOriginal,
        Decision::NeedsReview,
    ];

    fn item(id: &str, decision: JudgeDecision, final_comment: Option<&str>) -> JudgeItem {
        JudgeItem {
            id: id.to_string(),
            decision,
            final_comment: final_comment.map(str::to_string),
            confidence: 0.8,
            rationale: "judged".to_string(),
        }
    }

    fn assert_railed(outcome: &JudgeOutcome) {
        assert_eq!(outcome.decision, JudgeDecision::NeedsReview);
        assert_eq!(outcome.decided_by, DecidedBy::Engine);
        assert_eq!(outcome.confidence, 1.0);
    }

    #[test]
    fn in_set_judge_decisions_pass_through() {
        let packets = [packet("C1", &FULL), packet("C2", &REDUCED)];
        let resp = JudgeResponse {
            decisions: vec![
                item("C1", JudgeDecision::Hybrid, Some("merged")),
                item("C2", JudgeDecision::KeepOriginal, None),
            ],
        };
        let (outcomes, warnings) = validate_judge_response(resp, &packets);
        assert!(warnings.is_empty());
        assert_eq!(
            outcomes["C1"],
            JudgeOutcome {
                decision: JudgeDecision::Hybrid,
                final_comment: Some("merged".to_string()),
                confidence: 0.8,
                rationale: "judged".to_string(),
                decided_by: DecidedBy::Judge,
            }
        );
        assert_eq!(outcomes["C2"].decision, JudgeDecision::KeepOriginal);
        assert_eq!(outcomes["C2"].decided_by, DecidedBy::Judge);
    }

    #[test]
    fn a_missing_id_needs_review() {
        let packets = [packet("C1", &FULL), packet("C2", &FULL)];
        let resp = JudgeResponse {
            decisions: vec![item("C1", JudgeDecision::Remove, None)],
        };
        let (outcomes, warnings) = validate_judge_response(resp, &packets);
        assert!(warnings.is_empty());
        assert_eq!(outcomes.len(), 2);
        assert_railed(&outcomes["C2"]);
        assert_eq!(outcomes["C2"].final_comment, None);
        assert!(outcomes["C2"].rationale.contains("no decision"));
    }

    #[test]
    fn an_unknown_id_is_dropped_with_one_warning() {
        let packets = [packet("C1", &FULL)];
        let resp = JudgeResponse {
            decisions: vec![
                item("C1", JudgeDecision::Remove, None),
                item("C9", JudgeDecision::Remove, None),
            ],
        };
        let (outcomes, warnings) = validate_judge_response(resp, &packets);
        assert_eq!(outcomes.keys().collect::<Vec<_>>(), ["C1"]);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, FileWarningKind::UnknownId);
        assert!(warnings[0].detail.contains("C9"));
    }

    #[test]
    fn an_out_of_set_decision_needs_review_and_keeps_the_proposal() {
        let packets = [packet("C1", &REDUCED)];
        let resp = JudgeResponse {
            decisions: vec![item("C1", JudgeDecision::KeepGenerated, Some("better"))],
        };
        let (outcomes, _) = validate_judge_response(resp, &packets);
        let outcome = &outcomes["C1"];
        assert_railed(outcome);
        assert_eq!(outcome.final_comment.as_deref(), Some("better"));
        assert!(outcome.rationale.contains("outside the allowed set"));
        assert!(
            outcome
                .rationale
                .contains("KEEP_GENERATED at confidence 0.8")
        );
    }

    #[test]
    fn a_writing_decision_without_final_comment_needs_review() {
        let packets = [packet("C1", &FULL), packet("C2", &FULL)];
        let resp = JudgeResponse {
            decisions: vec![
                item("C1", JudgeDecision::KeepGenerated, None),
                item("C2", JudgeDecision::Hybrid, None),
            ],
        };
        let (outcomes, _) = validate_judge_response(resp, &packets);
        for id in ["C1", "C2"] {
            assert_railed(&outcomes[id]);
            assert!(outcomes[id].rationale.contains("null finalComment"));
        }
    }

    fn regenerated(id: &str, would: bool, comment: &str) -> RegeneratedComment {
        RegeneratedComment {
            id: id.to_string(),
            would_you_comment_here: would,
            comment: comment.to_string(),
            difficulty: Difficulty::Easy,
            rationale: "r".to_string(),
            missing_context: Some("history".to_string()),
        }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn regenerations_are_normalized() {
        let adapter = adapter_for(LanguageId::Typescript);
        let resp = RegenerationResponse {
            comments: vec![
                regenerated("C1", true, "// first\r\n// second"),
                regenerated("C2", true, "#1234: workaround"),
            ],
        };
        let (out, warnings) = postprocess_regeneration(resp, &ids(&["C1", "C2"]), adapter);
        assert!(warnings.is_empty());
        assert_eq!(out[0].regenerated.as_deref(), Some("first\nsecond"));
        assert_eq!(out[1].regenerated.as_deref(), Some("#1234: workaround"));
        assert_eq!(out[0].missing_context.as_deref(), Some("history"));
        assert!(out[0].would_you_comment_here);
    }

    #[test]
    fn an_empty_comment_becomes_no_regeneration() {
        let adapter = adapter_for(LanguageId::Python);
        let resp = RegenerationResponse {
            comments: vec![regenerated("C1", false, "")],
        };
        let (out, warnings) = postprocess_regeneration(resp, &ids(&["C1"]), adapter);
        assert!(warnings.is_empty());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].regenerated, None);
        assert!(!out[0].would_you_comment_here);
        assert_eq!(out[0].difficulty, Difficulty::Easy);
    }

    #[test]
    fn unrequested_ids_are_dropped_with_one_warning_each() {
        let adapter = adapter_for(LanguageId::Typescript);
        let resp = RegenerationResponse {
            comments: vec![
                regenerated("C1", true, "why"),
                regenerated("C7", true, "leaked"),
                regenerated("C8", false, ""),
            ],
        };
        let (out, warnings) = postprocess_regeneration(resp, &ids(&["C1", "C2"]), adapter);
        assert_eq!(
            out.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["C1"]
        );
        assert_eq!(warnings.len(), 2);
        assert!(
            warnings
                .iter()
                .all(|w| w.kind == FileWarningKind::UnknownId)
        );
        assert!(warnings[0].detail.contains("C7") && warnings[1].detail.contains("C8"));
    }

    #[test]
    fn packets_serialize_camel_case() {
        let value = serde_json::to_value(packet("C1", &REDUCED)).unwrap();
        assert_eq!(value["anchorSnippet"], "fn f() {}");
        assert_eq!(value["wouldYouCommentHere"], true);
        assert_eq!(
            value["allowed"],
            serde_json::json!(["REMOVE", "KEEP_ORIGINAL", "NEEDS_REVIEW"])
        );
    }
}
