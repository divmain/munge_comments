//! The prompt pack: pure builders for the messages of the two model calls.
//!
//! [`build_regeneration_messages`] renders call #1 (blind regeneration) in
//! either mode, and [`build_judge_messages`] renders call #2 (the judge).
//! Each returns a system message (the role, the rules, the difficulty
//! rubric and the JSON contract) followed by a user message (the file's
//! data). Every template contains the literal word `JSON` and embeds the
//! pretty-printed strict schema of its response type — the same schema
//! [`complete_typed`](crate::llm::call::complete_typed) sends in
//! `response_format` — so the prompt stays self-describing after the
//! `json_object` fallback.
//!
//! Rendering is deterministic: the same input always yields byte-identical
//! messages.

use std::fmt::Write as _;

use schemars::JsonSchema;

use crate::comments::inventory::StyleStats;
use crate::comments::types::CommentKind;
use crate::config::Mode;
use crate::lang::ids::LanguageId;
use crate::llm::call::Packet;
use crate::llm::client::Message;
use crate::llm::schemas::{
    JudgeResponse, RegenerationResponse, RemoveModeRegenerationResponse, strict_schema,
};

/// The input of call #1 for one file or chunk.
#[derive(Debug, Clone, Copy)]
pub struct RegenPromptInput<'a> {
    /// The file's effective mode; selects the template and response shape.
    pub mode: Mode,
    /// The file's language.
    pub language: LanguageId,
    /// Root-relative POSIX path.
    pub path: &'a str,
    /// The blind (redact) or clean (remove) text of the file or chunk.
    pub prompt_text: &'a str,
    /// The file outline for a chunked file; empty otherwise.
    pub outline: &'a str,
    /// The file's comment-style statistics.
    pub style_stats: StyleStats,
    /// The raw text of each functional comment in `prompt_text`, which the
    /// model sees verbatim and must leave alone.
    pub functional_texts: &'a [String],
    /// The adapter's `declaration_kinds`: the remove-mode `nodeKind`
    /// vocabulary (`end_of_file` is added by the template). Unused in
    /// redact mode.
    pub declaration_kinds: &'a [&'a str],
    /// Redact mode: exactly the placeholder ids the model must answer (the
    /// chunk's targets, or a half of them on a truncation retry). Empty in
    /// remove mode, where it is not rendered.
    pub requested_ids: &'a [String],
}

/// The input of call #2 for one judge batch.
#[derive(Debug, Clone, Copy)]
pub struct JudgePromptInput<'a> {
    /// The file's language.
    pub language: LanguageId,
    /// Root-relative POSIX path.
    pub path: &'a str,
    /// The batch's packets, each with its own allowed decisions.
    pub packets: &'a [Packet],
}

/// The human name of a language, for the model-facing role line.
fn display_name(language: LanguageId) -> &'static str {
    match language {
        LanguageId::Typescript => "TypeScript",
        LanguageId::Tsx => "TypeScript (TSX)",
        LanguageId::Javascript => "JavaScript",
        LanguageId::Python => "Python",
        LanguageId::Go => "Go",
        LanguageId::Java => "Java",
        LanguageId::C => "C",
        LanguageId::Cpp => "C++",
        LanguageId::Rust => "Rust",
    }
}

/// `text` in a markdown code fence tagged `info`. The fence is one backtick
/// longer than the longest backtick run in `text` (and at least three), so
/// no content can close it early.
fn fenced(text: &str, info: &str) -> String {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest_run + 1).max(3));
    let body = text.strip_suffix('\n').unwrap_or(text);
    format!("{fence}{info}\n{body}\n{fence}")
}

/// The pretty-printed strict schema of `T`, fenced as JSON.
fn schema_block<T: JsonSchema>() -> String {
    let schema = strict_schema::<T>();
    // Serializing a `serde_json::Value` cannot fail.
    let pretty = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| schema.to_string());
    fenced(&pretty, "json")
}

/// A comma-separated list of `items`, each in backticks.
fn code_list<S: AsRef<str>>(items: &[S]) -> String {
    items
        .iter()
        .map(|item| format!("`{}`", item.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The style section of a regeneration prompt: the file's comment-style
/// statistics (over its explanatory comments, which the model cannot see)
/// and the prevailing kind.
pub fn render_style_stats(stats: &StyleStats) -> String {
    let total = stats.line + stats.block + stats.doc;
    let mut out = String::from("## Comment style in this file\n");
    if total == 0 {
        out.push_str(
            "The file has no explanatory comments to match; write concise, idiomatic comments.",
        );
        return out;
    }
    let prevailing = match stats.prevailing() {
        CommentKind::Line => "line comments",
        CommentKind::Block => "block comments",
        CommentKind::Doc => "doc comments",
    };
    let _ = write!(
        out,
        "The file's {total} explanatory comments (hidden from you): {} line, {} block, {} doc; \
         average length {:.0} characters, delimiters included. Prevailing kind: {prevailing}. \
         Write comments of similar length, density and voice.",
        stats.line, stats.block, stats.doc, stats.avg_length,
    );
    out
}

/// The outline section of a chunk prompt; empty for an unchunked file
/// (empty `outline`), whose prompt shows the whole file anyway.
pub fn render_outline(outline: &str) -> String {
    if outline.is_empty() {
        return String::new();
    }
    format!(
        "## File outline\n\
         The source below is one part of a larger file. The whole file's top-level items, one \
         per line (`name: first line`, or just the first line when unnamed):\n{}",
        fenced(outline, "text"),
    )
}

/// The functional-comment section: the verbatim-directive warning and the
/// list of functional comments; empty when there are none.
fn render_functional(functional_texts: &[String], language: LanguageId) -> String {
    if functional_texts.is_empty() {
        return String::new();
    }
    let listing = functional_texts.join("\n");
    format!(
        "## Functional comments (keep verbatim)\n\
         These comments in the source are directives or otherwise load-bearing: tools or policy \
         depend on their exact text. They stay exactly as written. Do not reword, move or repeat \
         them, do not comment on them, and never put directive syntax in your own comments.\n{}",
        fenced(&listing, language.as_str()),
    )
}

/// The writing rules shared by both regeneration templates.
fn writing_rules(language: LanguageId) -> String {
    format!(
        "## How to write comments\n\
         - Explain why, not what: intent, constraints, trade-offs, invariants, non-obvious \
         consequences.\n\
         - Never restate what names, types or the code itself already express.\n\
         - Match the file's prevailing comment style (statistics below), idiomatically for {}.\n\
         - Write the comment text only: no comment delimiters or prefixes (`//`, `#`, `/*`, \
         `*/`, a leading `*`) on any line. Separate lines with \\n.\n\
         - Never write or modify a directive (lint or type-checker suppressions, build tags, \
         pragmas): directives are code, not prose.",
        display_name(language),
    )
}

/// The difficulty rubric table, restated verbatim.
const RUBRIC_TABLE: &str = "\
| Rating | Meaning |
|---|---|
| EASY | Fully reconstructable from immediately visible code; names/types are self-describing |
| MEDIUM | Required reading surrounding code in the file or inferring non-obvious intent |
| DIFFICULT | Requires knowledge absent from the codebase (history, external contracts, subtle invariants); the regenerated comment is guesswork |";

/// The regeneration prompts' rubric section: the rubric plus the
/// `rationale` and `missingContext` instructions.
fn regeneration_rubric() -> String {
    format!(
        "## Difficulty rubric\n\
         Rate every comment you write:\n\n\
         {RUBRIC_TABLE}\n\n\
         Rate honestly: when your comment is a guess, it is DIFFICULT. `rationale` says why you \
         chose the rating. `missingContext` names the knowledge you lacked, specifically (e.g. \
         why a limit has this value, which external system imposes a format); use null when \
         nothing was missing."
    )
}

/// The opening of a JSON contract section, ahead of the embedded schema.
fn json_contract<T: JsonSchema>(entries: &str) -> String {
    format!(
        "## JSON response\n\
         Reply with one JSON object and nothing else: no prose, no code fences. {entries} The \
         object must match this JSON Schema:\n{}",
        schema_block::<T>(),
    )
}

/// The header of every user message: language id and path.
fn file_header(language: LanguageId, path: &str) -> String {
    format!("Language id: {}\nPath: {path}", language.as_str())
}

/// Joins the non-empty `sections` with blank lines.
fn join_sections(sections: &[&str]) -> String {
    sections
        .iter()
        .filter(|section| !section.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Builds the messages of call #1 for one file or chunk: the redact-mode
/// template (answer by placeholder id) or the remove-mode template (cite
/// anchors), per `input.mode`.
pub fn build_regeneration_messages(input: &RegenPromptInput<'_>) -> Vec<Message> {
    match input.mode {
        Mode::Redact => redact_messages(input),
        Mode::Remove => remove_messages(input),
    }
}

fn redact_messages(input: &RegenPromptInput<'_>) -> Vec<Message> {
    let name = display_name(input.language);
    let role = format!(
        "You are an expert {name} engineer. In the {name} source file below, explanatory \
         comments have been replaced by placeholders. Without seeing the originals, write the \
         comment each requested placeholder should hold, as a careful maintainer of this code \
         would."
    );
    let placeholders = "\
## Placeholders
- A placeholder is a comment whose only content is a token of the form ⟦Cn⟧, such as ⟦C12⟧; \
its id is the part inside the brackets (`C12`). The closing delimiter of a block placeholder \
may sit on the next line.
- One placeholder stands for one whole comment, which may have spanned several lines.
- A placeholder on a line of its own comments on the code below it; a placeholder after code \
on the same line comments on that line.
- Never echo placeholders: no ⟦, ⟧ or placeholder ids in your comments.
- Comments still visible in the source were not hidden. Leave them alone and do not repeat \
them.";
    let needed = "\
## Is a comment needed here?
Set `wouldYouCommentHere` to false when you would leave this spot uncommented because the code \
speaks for itself. Still give your best comment; it may be empty only when you answer false.";
    let contract = json_contract::<RegenerationResponse>(
        "Give exactly one `comments` entry for each requested id (listed after the source), and \
         none for any other placeholder.",
    );
    let system = join_sections(&[
        &role,
        placeholders,
        needed,
        &writing_rules(input.language),
        &regeneration_rubric(),
        &contract,
    ]);
    let ids = format!(
        "## Requested ids\nReply with the JSON object described above, with exactly one entry \
         for each of these ids, and no others: {}",
        input.requested_ids.join(", "),
    );
    let user = join_sections(&[
        &file_header(input.language, input.path),
        &render_outline(input.outline),
        &render_functional(input.functional_texts, input.language),
        &render_style_stats(&input.style_stats),
        &format!(
            "## Source (comments replaced by placeholders)\n{}",
            fenced(input.prompt_text, input.language.as_str())
        ),
        &ids,
    ]);
    vec![Message::system(system), Message::user(user)]
}

fn remove_messages(input: &RegenPromptInput<'_>) -> Vec<Message> {
    let name = display_name(input.language);
    let role = format!(
        "You are an expert {name} engineer. The explanatory comments of the {name} source file \
         below have been removed. Write the comments a careful maintainer would put back, citing \
         for each one the code it annotates."
    );
    let placement = format!(
        "## Where to comment\n\
         - Comment only where a comment earns its place; leave self-explanatory code \
         uncommented. An empty `comments` list is a valid answer.\n\
         - Comments still in the source were kept. Do not repeat them or add a comment beside \
         one.\n\n\
         ## Citing the code\n\
         Each comment names its place with `anchor` and `position`:\n\
         - `position`: `leading` puts the comment on its own line(s) directly above the anchor; \
         `trailing` puts it at the end of the anchor's first line.\n\
         - `anchor.nodeKind`: the tree-sitter node type of the annotated code. The vocabulary \
         is the declaration kinds {}, plus `end_of_file` (see below). For code that is not a \
         declaration, use the actual statement kind (e.g. `if_statement`, \
         `expression_statement`) or a best guess.\n\
         - `anchor.name`: the declared name for a declaration; null otherwise.\n\
         - `anchor.snippet`: the first line(s), at most 3, of the annotated code, copied \
         character for character from the source, without comments.\n\
         - For a comment at the end of the file, after all code, cite `end_of_file` with an \
         empty snippet and a null name.",
        code_list(input.declaration_kinds),
    );
    let contract = json_contract::<RemoveModeRegenerationResponse>(
        "Give one `comments` entry per comment you would write.",
    );
    let system = join_sections(&[
        &role,
        &placement,
        &writing_rules(input.language),
        &regeneration_rubric(),
        &contract,
    ]);
    let user = join_sections(&[
        &file_header(input.language, input.path),
        &render_outline(input.outline),
        &render_functional(input.functional_texts, input.language),
        &render_style_stats(&input.style_stats),
        &format!(
            "## Source (explanatory comments removed)\n{}",
            fenced(input.prompt_text, input.language.as_str())
        ),
        "Reply with the JSON object described above.",
    ]);
    vec![Message::system(system), Message::user(user)]
}

/// The judge's instructions: the role, the packet format, the decision
/// definitions (verbatim), the decision matrix as guidance, and the rails
/// the engine enforces anyway.
fn judge_instructions(language: LanguageId) -> String {
    let name = display_name(language);
    format!(
        "You are an expert {name} reviewer, acting as the judge of a {name} file's comments. \
         Each packet holds one comment: the code it annotates, the original comment, and a \
         regeneration written by a model that saw the code but not the original. Decide which \
         comment, if any, the file should keep.\n\n\
         ## Packets\n\
         - `Allowed decisions`: the only decisions you may return for this packet.\n\
         - `Code`: the first lines of the annotated code, comments removed.\n\
         - `Original`: the comment as written, delimiters included.\n\
         - `Regenerated`: the regeneration, without delimiters; `none` when there is none.\n\
         - `Difficulty`: the regenerating model's rating of its own comment, by the rubric \
         below, with its rationale.\n\
         - `wouldYouCommentHere`: whether the regenerating model would comment there at all \
         (shown only when it was asked).\n\n\
         ## Decisions\n\
         - `REMOVE` — the comment earns no place; the code says it all.\n\
         - `KEEP_ORIGINAL` — the original is at least as good; leave it untouched.\n\
         - `KEEP_GENERATED` — the regeneration is strictly better and loses nothing the \
         original said; write it as-is (`finalComment` = the regeneration, verbatim or lightly \
         edited).\n\
         - `HYBRID` — each version carries something the other lacks; `finalComment` merges \
         them, keeping every fact from the original that is not visible in the code.\n\
         - `NEEDS_REVIEW` — the judge cannot decide safely; the original stays.\n\n\
         KEEP_GENERATED and HYBRID differ in what the original contributes: choose \
         KEEP_GENERATED only when the original adds nothing the regeneration lacks; when it \
         does, HYBRID must carry it over.\n\n\
         ## How to decide\n\
         By the packet's difficulty:\n\
         - EASY (allowed: REMOVE, KEEP_ORIGINAL, NEEDS_REVIEW): the comment's information is \
         reconstructable from the code. Decide only whether the comment earns its place, never \
         rewrite it; one that restates the code is REMOVE.\n\
         - MEDIUM: keep the version that carries the why, the reason behind the code; when both \
         carry part of it, merge them (HYBRID).\n\
         - DIFFICULT: the regeneration is guesswork and prone to confabulation, so lean strongly \
         to KEEP_ORIGINAL. Prefer the original when the regeneration asserts facts not visible \
         in the code. Any other decision on a DIFFICULT comment goes to human review instead of \
         being applied.\n\
         - No regeneration (`Regenerated: none`; allowed: REMOVE, KEEP_ORIGINAL, NEEDS_REVIEW): \
         judge the original on its own.\n\
         - `wouldYouCommentHere: no` hints that the code may speak for itself; it is evidence, \
         not proof.\n\n\
         In every case:\n\
         - Return only a decision from the packet's allowed decisions.\n\
         - Ties: when both versions are of comparable quality, choose KEEP_ORIGINAL. Never \
         rewrite for wording or style alone.\n\
         - Never lose a fact the code cannot show (history, external contracts, units, \
         invariants, the reason for an odd choice): an original that holds one is not REMOVE, \
         and any `finalComment` keeps it.\n\
         - `confidence` is your probability, from 0 to 1, that the decision is right. When it \
         would be below 0.6, choose NEEDS_REVIEW instead.\n\
         - `finalComment`: for KEEP_GENERATED and HYBRID, the exact comment text to write: no \
         comment delimiters or prefixes, lines separated by \\n, in the original's natural \
         language, never a directive or a ⟦…⟧ token. For every other decision, null.\n\
         - `rationale`: one or two sentences."
    )
}

/// One packet as a prompt section, exactly as [`build_judge_messages`]
/// renders it (the judge's batcher sizes packets by this text).
pub fn render_packet(packet: &Packet, language: LanguageId) -> String {
    let allowed: Vec<&str> = packet.allowed.iter().map(|d| d.as_str()).collect();
    let mut out = format!(
        "### {}\nAllowed decisions: {}\n",
        packet.id,
        allowed.join(", ")
    );
    match packet.difficulty {
        Some(difficulty) => {
            let _ = write!(out, "Difficulty: {}", difficulty.as_str());
            if let Some(rationale) = &packet.difficulty_rationale {
                let _ = write!(out, " — {rationale}");
            }
            out.push('\n');
        }
        None => out.push_str("Difficulty: not rated\n"),
    }
    if let Some(would) = packet.would_you_comment_here {
        let _ = writeln!(
            out,
            "wouldYouCommentHere: {}",
            if would { "yes" } else { "no" }
        );
    }
    if packet.anchor_snippet.is_empty() {
        out.push_str("Code: (end of file)\n");
    } else {
        let _ = writeln!(
            out,
            "Code:\n{}",
            fenced(&packet.anchor_snippet, language.as_str())
        );
    }
    let _ = writeln!(
        out,
        "Original:\n{}",
        fenced(&packet.original, language.as_str())
    );
    match &packet.regenerated {
        Some(regenerated) => {
            let _ = write!(out, "Regenerated:\n{}", fenced(regenerated, "text"));
        }
        None => out.push_str("Regenerated: none"),
    }
    out
}

/// Builds the messages of call #2 for one judge batch: the instructions,
/// the rubric and the JSON contract, then one section per packet (each
/// stating its own allowed decisions).
pub fn build_judge_messages(input: &JudgePromptInput<'_>) -> Vec<Message> {
    let contract = json_contract::<JudgeResponse>(
        "Give exactly one `decisions` entry for each packet id, and no others.",
    );
    let system = join_sections(&[
        &judge_instructions(input.language),
        &format!("## Difficulty rubric\n{RUBRIC_TABLE}"),
        &contract,
    ]);
    let ids: Vec<&str> = input.packets.iter().map(|p| p.id.as_str()).collect();
    let packets: Vec<String> = input
        .packets
        .iter()
        .map(|packet| render_packet(packet, input.language))
        .collect();
    let user = join_sections(&[
        &file_header(input.language, input.path),
        &format!("## Packets\n\n{}", packets.join("\n\n")),
        &format!(
            "Reply with the JSON object described above, with exactly one decision for each of \
             these ids, and no others: {}",
            ids.join(", "),
        ),
    ]);
    vec![Message::system(system), Message::user(user)]
}
