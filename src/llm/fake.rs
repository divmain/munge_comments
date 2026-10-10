//! [`FakeLlm`]: a deterministic, scenario-driven [`LlmClient`] for offline
//! tests — public, so library consumers can test offline too.
//!
//! The fake never parses prompts. It answers from [`ChatRequest::meta`]:
//!
//! - **Keys.** A [`Scenario`] entry is found by the `match_key` of the
//!   item's original comment text; an item with no entry under that key
//!   falls back to its anchor name. A `"<path>::<key>"` entry wins over the
//!   bare key. The first entry found decides; when it lacks the script the
//!   call needs, the default rule applies.
//! - **Routing.** [`LlmMeta::call`] routes regeneration vs judge; for
//!   regeneration, [`LlmMeta::mode`] selects the response shape.
//! - **Redact mode** answers exactly [`LlmMeta::ids`].
//! - **Remove mode** answers one citation per [`LlmMeta::items`] entry,
//!   anchored at the item's own anchor (a `dangling` original is cited as
//!   `leading`, the only remove-mode position that can express it; an
//!   `end_of_file` anchor is cited as-is), then appends the scenario's
//!   matching [`ExtraRegeneration`]s (which the engine records as NEW).
//! - **Judge** answers exactly [`LlmMeta::ids`].
//! - **Default rule** for unlisted comments: the regeneration echoes the
//!   original's `comment_body` rated `DIFFICULT`; the judge replies
//!   `KEEP_ORIGINAL` at confidence 0.9.
//! - **Failure injection** ([`FailureInjection`]) fails the first N calls
//!   for a path, or with path `"*"` the first N calls overall.
//! - **Call log.** [`FakeLlm::calls`] lists every call, failed ones
//!   included, in call order.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::artifact::types::{Difficulty, JudgeDecision};
use crate::comments::sanitize::{comment_body, match_key};
use crate::comments::types::{Anchor, Position};
use crate::config::Mode;
use crate::errors::{Error, LlmErrorReason};
use crate::lang::adapters::adapter_for;
use crate::llm::client::{CallKind, ChatRequest, LlmClient, LlmMeta, MetaItem, RawResponse};
use crate::llm::schemas::{
    CitedPosition, JudgeItem, JudgeResponse, RegeneratedComment, RegenerationResponse,
    RemoveModeComment, RemoveModeRegenerationResponse,
};

/// The rationale of every default-rule answer, and of scripts that omit one.
const SCRIPTED_RATIONALE: &str = "scripted by FakeLlm";

/// The judge's confidence under the default rule.
const DEFAULT_JUDGE_CONFIDENCE: f64 = 0.9;

/// The path of a [`FailureInjection`] that matches every call.
pub const ANY_PATH: &str = "*";

/// Scripted model behavior for [`FakeLlm`], usually loaded from a JSON file
/// with [`Scenario::from_json_file`]. Every field defaults to empty, so an
/// empty object (or [`Scenario::default`]) applies the default rule to
/// every comment.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Scenario {
    /// Scripts keyed by `match_key(original)` or anchor name, optionally
    /// path-qualified as `"<path>::<key>"`.
    pub by_key: BTreeMap<String, ScenarioEntry>,
    /// Remove mode: comments the model writes at locations that had none.
    pub extra_regenerations: Vec<ExtraRegeneration>,
    /// Calls that fail instead of answering.
    pub failures: Vec<FailureInjection>,
}

/// The scripts for one comment.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ScenarioEntry {
    /// Call #1's answer; `None` applies the default rule.
    pub regen: Option<RegenScript>,
    /// Call #2's answer; `None` applies the default rule.
    pub judge: Option<JudgeScript>,
}

/// A scripted regeneration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegenScript {
    /// The regenerated text, without delimiters.
    pub comment: String,
    /// The self-rated difficulty.
    pub difficulty: Difficulty,
    /// Whether the model would comment here (default `true`). In remove
    /// mode, `false` omits the citation altogether.
    #[serde(default = "default_true")]
    pub would_you_comment_here: bool,
    /// Why that difficulty.
    #[serde(default = "default_rationale")]
    pub rationale: String,
    /// What context would have helped.
    #[serde(default)]
    pub missing_context: Option<String>,
}

/// A scripted judge verdict.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JudgeScript {
    /// The decision (validated against the packet by the engine, not here).
    pub decision: JudgeDecision,
    /// The text to write for `KEEP_GENERATED` / `HYBRID`.
    #[serde(default)]
    pub final_comment: Option<String>,
    /// Confidence in `[0, 1]`. Required: it drives the engine's
    /// confidence rails.
    pub confidence: f64,
    /// Why this decision.
    #[serde(default = "default_rationale")]
    pub rationale: String,
}

/// A remove-mode comment at a location that had none.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtraRegeneration {
    /// Root-relative POSIX path of the file.
    pub path: String,
    /// The chunk whose regeneration call carries it (default 0). Citations
    /// resolve within their own chunk, so an extra is answered once, by
    /// the chunk that contains its anchor.
    #[serde(default)]
    pub chunk_index: usize,
    /// The cited anchor.
    pub anchor: Anchor,
    /// The cited position.
    pub position: CitedPosition,
    /// The comment text, without delimiters.
    pub comment: String,
    /// The self-rated difficulty.
    pub difficulty: Difficulty,
}

/// Makes matching calls fail.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailureInjection {
    /// Root-relative POSIX path, or [`ANY_PATH`] (`"*"`) for every call.
    pub path: String,
    /// Only calls of this kind match; `None` matches both.
    #[serde(default)]
    pub call: Option<CallKind>,
    /// How the call fails.
    pub error: InjectedFailure,
    /// Fails the first `times` matching calls (counted in call order);
    /// `None` fails every matching call.
    #[serde(default)]
    pub times: Option<u32>,
}

/// The failure an injected call produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjectedFailure {
    /// `Error::Llm { reason: Exhausted }`.
    Llm,
    /// A [`RawResponse`] with `finish_reason: "length"`.
    Truncated,
    /// `Error::Llm { reason: Auth }`.
    Auth,
}

fn default_true() -> bool {
    true
}

fn default_rationale() -> String {
    SCRIPTED_RATIONALE.to_string()
}

impl Scenario {
    /// Reads a scenario JSON file. The path must be absolute
    /// ([`Error::Usage`] otherwise, so the process cwd is never consulted);
    /// an unreadable file is [`Error::Fs`] and invalid JSON
    /// [`Error::Usage`].
    pub fn from_json_file(path: &Path) -> Result<Scenario, Error> {
        if !path.is_absolute() {
            return Err(Error::Usage(format!(
                "the scenario path must be absolute, got {}",
                path.display()
            )));
        }
        let bytes = fs::read(path).map_err(|source| Error::Fs {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|err| {
            Error::Usage(format!("{}: invalid scenario JSON: {err}", path.display()))
        })
    }

    /// The entry for `item` of `meta.path`: by `match_key(original)`, then
    /// by anchor name; path-qualified before bare at each step.
    fn entry(&self, meta: &LlmMeta, item: &MetaItem) -> Option<&ScenarioEntry> {
        let adapter = adapter_for(meta.language);
        let by_text = item.original.as_deref().map(|raw| match_key(raw, adapter));
        [by_text.as_deref(), item.anchor.name.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|key| {
                self.by_key
                    .get(&format!("{}::{key}", meta.path))
                    .or_else(|| self.by_key.get(key))
            })
    }
}

/// One call [`FakeLlm`] received, in call order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    /// [`LlmMeta::path`].
    pub path: String,
    /// [`LlmMeta::call`].
    pub call: CallKind,
    /// [`LlmMeta::chunk_index`].
    pub chunk_index: usize,
    /// [`LlmMeta::ids`].
    pub ids: Vec<String>,
}

/// A code-only test hook: runs before each call is answered, with the
/// call's 0-based global index and its meta.
pub type OnCall = Arc<dyn Fn(usize, &LlmMeta) + Send + Sync>;

/// Mutable state, behind one lock so call indices, failure counters and
/// the log always agree on the call order.
#[derive(Default)]
struct State {
    calls: Vec<CallRecord>,
    /// Per [`Scenario::failures`] entry: how many calls it has matched.
    failure_matches: Vec<u32>,
}

/// A deterministic [`LlmClient`] answering from a [`Scenario`]; see the
/// module docs for the rules.
pub struct FakeLlm {
    scenario: Scenario,
    on_call: Option<OnCall>,
    state: Mutex<State>,
}

impl FakeLlm {
    /// A fake answering from `scenario`.
    pub fn new(scenario: Scenario) -> FakeLlm {
        let failure_matches = vec![0; scenario.failures.len()];
        FakeLlm {
            scenario,
            on_call: None,
            state: Mutex::new(State {
                calls: Vec::new(),
                failure_matches,
            }),
        }
    }

    /// Installs a hook that runs before each call is answered (e.g. to
    /// cancel a run deterministically after N calls).
    pub fn with_on_call(mut self, hook: OnCall) -> FakeLlm {
        self.on_call = Some(hook);
        self
    }

    /// Every call received so far, failed ones included, in call order.
    pub fn calls(&self) -> Vec<CallRecord> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Logs the call and settles its failure injection under one lock;
    /// returns the call's global index and the failure to inject, if any.
    fn register(&self, meta: &LlmMeta) -> (usize, Option<InjectedFailure>) {
        let mut state = self.lock();
        let index = state.calls.len();
        state.calls.push(CallRecord {
            path: meta.path.clone(),
            call: meta.call,
            chunk_index: meta.chunk_index,
            ids: meta.ids.clone(),
        });
        let mut injected = None;
        for (injection, matched) in self
            .scenario
            .failures
            .iter()
            .zip(state.failure_matches.iter_mut())
        {
            let path_matches = injection.path == ANY_PATH || injection.path == meta.path;
            let call_matches = injection.call.is_none_or(|call| call == meta.call);
            if !(path_matches && call_matches) {
                continue;
            }
            *matched = matched.saturating_add(1);
            let within_budget = injection.times.is_none_or(|times| *matched <= times);
            if injected.is_none() && within_budget {
                injected = Some(injection.error);
            }
        }
        (index, injected)
    }

    fn answer(&self, meta: &LlmMeta) -> Result<RawResponse, Error> {
        let content = match (meta.call, meta.mode) {
            (CallKind::Regenerate, Mode::Redact) => serde_json::to_string(&self.redact(meta)),
            (CallKind::Regenerate, Mode::Remove) => serde_json::to_string(&self.remove(meta)),
            (CallKind::Judge, _) => serde_json::to_string(&self.judge(meta)),
        }
        .map_err(|err| Error::Invariant(format!("FakeLlm could not serialize a reply: {err}")))?;
        Ok(RawResponse {
            content: Some(content),
            finish_reason: Some("stop".to_string()),
            refusal: None,
        })
    }

    fn item<'m>(meta: &'m LlmMeta, id: &str) -> Option<&'m MetaItem> {
        meta.items.iter().find(|item| item.id == id)
    }

    /// The scripted regeneration of `item`, or the default rule's echo;
    /// `None` when there is neither a script nor an original to echo.
    fn regen_script(&self, meta: &LlmMeta, item: &MetaItem) -> Option<RegenScript> {
        if let Some(script) = self
            .scenario
            .entry(meta, item)
            .and_then(|e| e.regen.clone())
        {
            return Some(script);
        }
        let original = item.original.as_deref()?;
        Some(RegenScript {
            comment: comment_body(original, adapter_for(meta.language)),
            difficulty: Difficulty::Difficult,
            would_you_comment_here: true,
            rationale: default_rationale(),
            missing_context: None,
        })
    }

    fn redact(&self, meta: &LlmMeta) -> RegenerationResponse {
        let comments = meta
            .ids
            .iter()
            .map(|id| {
                let script = Self::item(meta, id).and_then(|item| self.regen_script(meta, item));
                match script {
                    Some(script) => RegeneratedComment {
                        id: id.clone(),
                        would_you_comment_here: script.would_you_comment_here,
                        comment: script.comment,
                        difficulty: script.difficulty,
                        rationale: script.rationale,
                        missing_context: script.missing_context,
                    },
                    None => RegeneratedComment {
                        id: id.clone(),
                        would_you_comment_here: false,
                        comment: String::new(),
                        difficulty: Difficulty::Difficult,
                        rationale: default_rationale(),
                        missing_context: None,
                    },
                }
            })
            .collect();
        RegenerationResponse { comments }
    }

    fn remove(&self, meta: &LlmMeta) -> RemoveModeRegenerationResponse {
        let cited = meta.items.iter().filter_map(|item| {
            let script = self.regen_script(meta, item)?;
            script.would_you_comment_here.then(|| RemoveModeComment {
                anchor: item.anchor.clone(),
                position: match item.position {
                    Position::Trailing => CitedPosition::Trailing,
                    Position::Leading | Position::Dangling => CitedPosition::Leading,
                },
                comment: script.comment,
                difficulty: script.difficulty,
                rationale: script.rationale,
                missing_context: script.missing_context,
            })
        });
        let extras = self
            .scenario
            .extra_regenerations
            .iter()
            .filter(|extra| extra.path == meta.path && extra.chunk_index == meta.chunk_index)
            .map(|extra| RemoveModeComment {
                anchor: extra.anchor.clone(),
                position: extra.position,
                comment: extra.comment.clone(),
                difficulty: extra.difficulty,
                rationale: default_rationale(),
                missing_context: None,
            });
        RemoveModeRegenerationResponse {
            comments: cited.chain(extras).collect(),
        }
    }

    fn judge(&self, meta: &LlmMeta) -> JudgeResponse {
        let decisions = meta
            .ids
            .iter()
            .map(|id| {
                let script = Self::item(meta, id)
                    .and_then(|item| self.scenario.entry(meta, item))
                    .and_then(|entry| entry.judge.clone());
                match script {
                    Some(script) => JudgeItem {
                        id: id.clone(),
                        decision: script.decision,
                        final_comment: script.final_comment,
                        confidence: script.confidence,
                        rationale: script.rationale,
                    },
                    None => JudgeItem {
                        id: id.clone(),
                        decision: JudgeDecision::KeepOriginal,
                        final_comment: None,
                        confidence: DEFAULT_JUDGE_CONFIDENCE,
                        rationale: default_rationale(),
                    },
                }
            })
            .collect();
        JudgeResponse { decisions }
    }
}

impl LlmClient for FakeLlm {
    fn complete_json<'a>(
        &'a self,
        req: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<RawResponse, Error>> {
        Box::pin(async move {
            let meta = &req.meta;
            let (index, injected) = self.register(meta);
            if let Some(hook) = &self.on_call {
                hook(index, meta);
            }
            let call = match meta.call {
                CallKind::Regenerate => "regenerate",
                CallKind::Judge => "judge",
            };
            match injected {
                None => self.answer(meta),
                Some(InjectedFailure::Truncated) => Ok(RawResponse {
                    content: Some("{\"".to_string()),
                    finish_reason: Some("length".to_string()),
                    refusal: None,
                }),
                Some(InjectedFailure::Llm) => Err(Error::Llm {
                    reason: LlmErrorReason::Exhausted,
                    message: format!(
                        "FakeLlm: injected failure ({call} call #{index}, {})",
                        meta.path
                    ),
                }),
                Some(InjectedFailure::Auth) => Err(Error::Llm {
                    reason: LlmErrorReason::Auth,
                    message: format!(
                        "FakeLlm: injected auth failure ({call} call #{index}, {})",
                        meta.path
                    ),
                }),
            }
        })
    }
}
