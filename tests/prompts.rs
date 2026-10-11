//! The prompt pack (`llm::prompts`): snapshots of the rendered regeneration
//! prompts (both modes) and judge prompts in TypeScript, Python and Go, the
//! JSON-contract invariants every template must satisfy, and the token
//! budget of the redact-mode prompt for a 500-line file.

mod common;

use std::fs;

use munge_comments::artifact::types::{Decision, Difficulty};
use munge_comments::comments::types::Classification;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::{
    ChunkText, JudgePromptInput, JudgeResponse, Message, Packet, RegenPromptInput,
    RegenerationResponse, RemoveModeRegenerationResponse, Role, build_chunk_texts,
    build_judge_messages, build_regeneration_messages, chunk_file, render_outline,
    render_style_stats, strict_schema,
};
use munge_comments::{FileRef, Inventory, LanguageId, Mode, inventory_file};

const TS_SOURCE: &str = r#"import { readFile } from "node:fs/promises";

// Retry budget agreed with the upstream team; they rate-limit at 10 req/s.
const MAX_RETRIES = 3;

/**
 * Loads the user's settings file.
 */
export async function loadSettings(path: string): Promise<Settings> {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const raw: any = JSON.parse(await readFile(path, "utf8"));
  return { theme: raw.theme ?? "light" }; // older files predate the theme key
}
"#;

const PY_SOURCE: &str = r#"import os

# Seconds; the load balancer drops idle connections after 60.
KEEPALIVE = 55


def connect(host):
    """Open a connection."""
    # Port comes from the environment so staging can reuse the same image.
    port = int(os.environ.get("PORT", "8080"))  # type: ignore[arg-type]
    return (host, port)
"#;

const GO_SOURCE: &str = r#"package cache

import "sync"

//go:generate stringer -type=State

// State is the lifecycle state of an entry.
type State int

// Cache guards entries with a single mutex; contention was negligible in benchmarks.
type Cache struct {
	mu      sync.Mutex
	entries map[string]State
}

// Get returns the state for key.
func (c *Cache) Get(key string) State {
	c.mu.Lock()
	defer c.mu.Unlock() //nolint:gocritic
	return c.entries[key] // zero value means "absent"
}
"#;

fn file_name(language: LanguageId) -> &'static str {
    match language {
        LanguageId::Typescript => "src/settings.ts",
        LanguageId::Python => "app/net.py",
        LanguageId::Go => "internal/cache/cache.go",
        other => panic!("no sample file for {other:?}"),
    }
}

fn inventory(text: &str, language: LanguageId, path: &str) -> Inventory {
    inventory_file(
        text,
        FileRef {
            language,
            file_name: path.to_string(),
        },
    )
    .expect("test sources inventory cleanly")
}

/// Chunks `text` at `max_chars` and builds the chunk texts for `mode`.
fn chunk_texts(
    text: &str,
    language: LanguageId,
    inventory: &Inventory,
    mode: Mode,
    max_chars: usize,
) -> Vec<ChunkText> {
    let (chunks, _warnings) =
        chunk_file(text, language, inventory, max_chars, None).expect("chunks");
    build_chunk_texts(text, inventory, adapter_for(language), &chunks, mode, None)
        .expect("chunk texts")
}

/// The raw text of every functional comment in `chunk`.
fn functional_texts(inventory: &Inventory, chunk: &ChunkText) -> Vec<String> {
    inventory
        .comments
        .iter()
        .filter(|comment| chunk.chunk.comment_ids.contains(&comment.id))
        .filter(|comment| comment.classification == Classification::Functional)
        .map(|comment| comment.text.clone())
        .collect()
}

/// The placeholder ids of a redact-mode chunk; empty in remove mode.
fn requested_ids(chunk: &ChunkText) -> Vec<String> {
    chunk
        .id_map
        .as_ref()
        .map(|ids| ids.keys().cloned().collect())
        .unwrap_or_default()
}

/// Renders the regeneration messages of every chunk of `text`.
fn regeneration_prompts(
    text: &str,
    language: LanguageId,
    mode: Mode,
    max_chars: usize,
) -> Vec<Vec<Message>> {
    let path = file_name(language);
    let inventory = inventory(text, language, path);
    let adapter = adapter_for(language);
    chunk_texts(text, language, &inventory, mode, max_chars)
        .iter()
        .map(|chunk| {
            let functional = functional_texts(&inventory, chunk);
            let ids = requested_ids(chunk);
            build_regeneration_messages(&RegenPromptInput {
                mode,
                language,
                path,
                prompt_text: &chunk.prompt_text,
                outline: &chunk.chunk.outline,
                style_stats: inventory.style_stats,
                functional_texts: &functional,
                declaration_kinds: adapter.declaration_kinds,
                requested_ids: &ids,
            })
        })
        .collect()
}

/// One snapshot-friendly string per message list.
fn render(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            format!("===== {role} =====\n{}", message.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `ceil(chars / 4)`, the Design's token estimate.
fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn pretty<T: schemars::JsonSchema>() -> String {
    serde_json::to_string_pretty(&strict_schema::<T>()).expect("schemas serialize")
}

const RESTRICTED: [Decision; 3] = [
    Decision::Remove,
    Decision::KeepOriginal,
    Decision::NeedsReview,
];
const FULL: [Decision; 5] = [
    Decision::Remove,
    Decision::KeepOriginal,
    Decision::KeepGenerated,
    Decision::Hybrid,
    Decision::NeedsReview,
];

/// A batch mixing the three allowed sets: full (MEDIUM), restricted
/// (EASY) and reduced (no regeneration).
fn judge_packets(language: LanguageId) -> Vec<Packet> {
    let (snippet, original, regenerated, easy_original, easy_snippet, easy_regenerated) =
        match language {
            LanguageId::Typescript => (
                "const MAX_RETRIES = 3;",
                "// Retry budget agreed with the upstream team; they rate-limit at 10 req/s.",
                "Retry up to three times before giving up on the upstream call.",
                "/**\n * Loads the user's settings file.\n */",
                "export async function loadSettings(path: string): Promise<Settings> {",
                "Reads and parses the settings file at the given path.",
            ),
            LanguageId::Python => (
                "KEEPALIVE = 55",
                "# Seconds; the load balancer drops idle connections after 60.",
                "Keepalive interval in seconds, kept below the server timeout.",
                "# Port comes from the environment so staging can reuse the same image.",
                "port = int(os.environ.get(\"PORT\", \"8080\"))",
                "Read the port from the environment, defaulting to 8080.",
            ),
            LanguageId::Go => (
                "type Cache struct {\n\tmu      sync.Mutex\n\tentries map[string]State",
                "// Cache guards entries with a single mutex; contention was negligible in benchmarks.",
                "Cache is a mutex-protected map of entry states.",
                "// Get returns the state for key.",
                "func (c *Cache) Get(key string) State {",
                "Get looks up the state of key under the lock.",
            ),
            other => panic!("no packets for {other:?}"),
        };
    vec![
        Packet {
            id: "C2".to_string(),
            anchor_snippet: snippet.to_string(),
            original: original.to_string(),
            regenerated: Some(regenerated.to_string()),
            difficulty: Some(Difficulty::Medium),
            difficulty_rationale: Some("The value's origin is not visible in the code.".into()),
            would_you_comment_here: Some(true),
            allowed: FULL.to_vec(),
        },
        Packet {
            id: "C3".to_string(),
            anchor_snippet: easy_snippet.to_string(),
            original: easy_original.to_string(),
            regenerated: Some(easy_regenerated.to_string()),
            difficulty: Some(Difficulty::Easy),
            difficulty_rationale: Some("The signature says what it does.".to_string()),
            would_you_comment_here: Some(false),
            allowed: RESTRICTED.to_vec(),
        },
        Packet {
            id: "C5".to_string(),
            anchor_snippet: String::new(),
            original: "// end of generated section".to_string(),
            regenerated: None,
            difficulty: None,
            difficulty_rationale: None,
            would_you_comment_here: None,
            allowed: RESTRICTED.to_vec(),
        },
    ]
}

fn judge_prompt(language: LanguageId) -> Vec<Message> {
    let packets = judge_packets(language);
    build_judge_messages(&JudgePromptInput {
        language,
        path: file_name(language),
        packets: &packets,
    })
}

const SAMPLES: [(LanguageId, &str, &str); 3] = [
    (LanguageId::Typescript, "typescript", TS_SOURCE),
    (LanguageId::Python, "python", PY_SOURCE),
    (LanguageId::Go, "go", GO_SOURCE),
];

#[test]
fn regeneration_prompts_match_their_snapshots() {
    for (language, name, source) in SAMPLES {
        for (mode, mode_name) in [(Mode::Redact, "redact"), (Mode::Remove, "remove")] {
            let prompts = regeneration_prompts(source, language, mode, 120_000);
            assert_eq!(prompts.len(), 1, "{name}: an unchunked file has one prompt");
            insta::assert_snapshot!(format!("regen_{mode_name}_{name}"), render(&prompts[0]));
        }
    }
}

#[test]
fn judge_prompts_match_their_snapshots() {
    for (language, name, _) in SAMPLES {
        insta::assert_snapshot!(format!("judge_{name}"), render(&judge_prompt(language)));
    }
}

#[test]
fn chunked_prompt_carries_the_outline() {
    let prompts = regeneration_prompts(GO_SOURCE, LanguageId::Go, Mode::Redact, 200);
    assert!(
        prompts.len() > 1,
        "the Go sample splits at a 200-char budget"
    );
    for prompt in &prompts {
        assert!(prompt[1].content.contains("## File outline"));
    }
    insta::assert_snapshot!("regen_redact_go_chunk_0", render(&prompts[0]));
}

#[test]
fn rendering_is_deterministic() {
    for (language, _, source) in SAMPLES {
        for mode in [Mode::Redact, Mode::Remove] {
            assert_eq!(
                regeneration_prompts(source, language, mode, 120_000),
                regeneration_prompts(source, language, mode, 120_000),
            );
        }
        assert_eq!(judge_prompt(language), judge_prompt(language));
    }
}

#[test]
fn every_template_is_a_system_then_user_pair_embedding_its_strict_schema() {
    let cases: Vec<(Vec<Message>, String)> = SAMPLES
        .iter()
        .flat_map(|&(language, _, source)| {
            [
                (
                    regeneration_prompts(source, language, Mode::Redact, 120_000).remove(0),
                    pretty::<RegenerationResponse>(),
                ),
                (
                    regeneration_prompts(source, language, Mode::Remove, 120_000).remove(0),
                    pretty::<RemoveModeRegenerationResponse>(),
                ),
                (judge_prompt(language), pretty::<JudgeResponse>()),
            ]
        })
        .collect();
    for (messages, schema) in cases {
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].role, Role::User);
        let all = render(&messages);
        assert!(
            all.contains("JSON"),
            "the json_object fallback needs the word JSON"
        );
        assert!(
            all.contains(&schema),
            "the strict schema is embedded verbatim"
        );
    }
}

#[test]
fn redact_prompt_lists_exactly_the_requested_ids() {
    let path = file_name(LanguageId::Typescript);
    let inventory = inventory(TS_SOURCE, LanguageId::Typescript, path);
    let chunk = chunk_texts(
        TS_SOURCE,
        LanguageId::Typescript,
        &inventory,
        Mode::Redact,
        120_000,
    )
    .remove(0);
    // A truncation retry asks for a subset of the chunk's placeholders; requesting only half means a prompt that echoed every id would fail.
    let all_ids = requested_ids(&chunk);
    assert!(all_ids.len() >= 2, "the TS sample has several targets");
    let half = all_ids[..all_ids.len() / 2].to_vec();
    let messages = build_regeneration_messages(&RegenPromptInput {
        mode: Mode::Redact,
        language: LanguageId::Typescript,
        path,
        prompt_text: &chunk.prompt_text,
        outline: "",
        style_stats: inventory.style_stats,
        functional_texts: &[],
        declaration_kinds: &[],
        requested_ids: &half,
    });
    let user = &messages[1].content;
    let listed = format!("these ids, and no others: {}", half.join(", "));
    assert!(
        user.ends_with(&listed),
        "the id list closes the user message:\n{user}"
    );
    assert!(
        !user.contains("## File outline"),
        "no outline for an unchunked file"
    );
    assert!(
        !user.contains("## Functional comments"),
        "no empty functional section"
    );
}

#[test]
fn remove_prompt_states_the_node_kind_vocabulary() {
    let prompt = regeneration_prompts(GO_SOURCE, LanguageId::Go, Mode::Remove, 120_000).remove(0);
    let system = &prompt[0].content;
    for kind in adapter_for(LanguageId::Go).declaration_kinds {
        assert!(
            system.contains(&format!("`{kind}`")),
            "{kind} is in the vocabulary"
        );
    }
    assert!(system.contains("`end_of_file`"));
    assert!(!prompt[1].content.contains("## Requested ids"));
}

#[test]
fn judge_packets_state_their_own_allowed_decisions() {
    let prompt = judge_prompt(LanguageId::Typescript);
    let user = &prompt[1].content;
    assert!(user.contains(
        "### C2\nAllowed decisions: REMOVE, KEEP_ORIGINAL, KEEP_GENERATED, HYBRID, NEEDS_REVIEW\n"
    ));
    assert!(user.contains("### C3\nAllowed decisions: REMOVE, KEEP_ORIGINAL, NEEDS_REVIEW\n"));
    assert!(user.contains("Regenerated: none"));
    assert!(user.ends_with("these ids, and no others: C2, C3, C5"));
}

#[test]
fn source_containing_code_fences_cannot_close_its_fence() {
    let source = "// Example:\n// ```ts\n// f()\n// ```\nconst md = \"````\";\n";
    let prompt =
        regeneration_prompts(source, LanguageId::Typescript, Mode::Redact, 120_000).remove(0);
    let user = &prompt[1].content;
    assert!(
        user.contains("`````typescript\n"),
        "the fence outgrows the longest run:\n{user}"
    );
}

#[test]
fn helpers_render_style_stats_and_outlines() {
    let sample = inventory(TS_SOURCE, LanguageId::Typescript, "a.ts");
    let stats = render_style_stats(&sample.style_stats);
    assert!(stats.starts_with("## Comment style in this file\n"));
    assert!(stats.contains("Prevailing kind: line comments."));
    let empty = inventory("const x = 1;\n", LanguageId::Typescript, "b.ts");
    assert!(render_style_stats(&empty.style_stats).contains("no explanatory comments"));
    assert_eq!(render_outline(""), "");
    let outline = render_outline("foo: function foo() {\nbar: class bar {");
    assert!(outline.contains("```text\nfoo: function foo() {\nbar: class bar {\n```"));
}

/// The redact-mode prompt for a realistic 500-line file stays within 10k
/// tokens (`ceil(chars / 4)`), and the template overhead — everything but
/// the source text, including the embedded schema, the requested ids and
/// the functional comments — stays within 2.5k tokens, so even a source of
/// the full ~7.5k tokens (500 lines × ~60 chars) fits.
#[test]
fn redact_prompt_for_a_500_line_file_fits_the_token_budget() {
    let text = fs::read_to_string(common::fixture_path("prompt/large-500.ts")).expect("fixture");
    assert_eq!(text.lines().count(), 500);
    let prompts = regeneration_prompts(&text, LanguageId::Typescript, Mode::Redact, 120_000);
    assert_eq!(prompts.len(), 1);
    let total = tokens(&prompts[0][0].content) + tokens(&prompts[0][1].content);
    assert!(total <= 10_000, "redact prompt is {total} tokens");

    let path = file_name(LanguageId::Typescript);
    let inventory = inventory(&text, LanguageId::Typescript, path);
    let chunk = chunk_texts(
        &text,
        LanguageId::Typescript,
        &inventory,
        Mode::Redact,
        120_000,
    )
    .remove(0);
    let source_tokens = tokens(&chunk.prompt_text);
    let overhead = total - source_tokens;
    assert!(overhead <= 2_500, "template overhead is {overhead} tokens");
    // The raw source itself, unredacted, also fits beside the overhead — the worst case the budget must cover.
    assert!(tokens(&text) + overhead <= 10_000);
}
