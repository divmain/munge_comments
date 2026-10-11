//! The LLM boundary of the tool.
//!
//! Everything that talks to an endpoint sits behind the [`LlmClient`] trait,
//! so the engine stays fully testable with a fake. [`client`] holds the
//! trait and the built-in OpenAI-compatible transport; [`schemas`] the wire
//! response types and their strict JSON Schemas; [`call`] the typed call
//! helper ([`complete_typed`]) and the post-processing of its responses;
//! [`chunk`] the large-file chunker and per-chunk prompt texts; [`prompts`]
//! the message builders of both calls; [`fake`] the scenario-driven test
//! double [`FakeLlm`].

pub mod call;
pub mod chunk;
pub mod client;
pub mod fake;
pub mod prompts;
pub mod schemas;

pub use call::{
    JudgeOutcome, Packet, Regeneration, complete_typed, postprocess_regeneration, response_format,
    validate_judge_response,
};
pub use chunk::{Chunk, ChunkText, build_chunk_texts, chunk_file};
pub use client::{
    CallKind, ChatRequest, DEFAULT_MAX_COMPLETION_TOKENS, DEFAULT_TEMPERATURE, LlmClient, LlmMeta,
    Message, MetaItem, OpenAiCompatClient, OpenAiCompatConfig, ProxyMode, RawResponse, RetryPolicy,
    Role,
};
pub use fake::{
    CallRecord, ExtraRegeneration, FailureInjection, FakeLlm, InjectedFailure, JudgeScript, OnCall,
    RegenScript, Scenario, ScenarioEntry,
};
pub use prompts::{
    JudgePromptInput, RegenPromptInput, build_judge_messages, build_regeneration_messages,
    render_outline, render_packet, render_style_stats,
};
pub use schemas::{
    CitedPosition, JudgeItem, JudgeResponse, RegeneratedComment, RegenerationResponse,
    RemoveModeComment, RemoveModeRegenerationResponse, WireValidate, make_strict, strict_schema,
};
