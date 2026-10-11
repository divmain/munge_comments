//! The noisy-repo ground-truth manifest (`tests/fixtures/noisy-repo/
//! ground-truth.json`) and the [`FakeLlm`](munge_comments::FakeLlm)
//! [`Scenario`] generated from it, so the two can never drift.
//!
//! The manifest maps each inventoried fixture comment — by root-relative
//! path, `match_key` of its raw text and anchor name, never by line — to
//! its ground-truth [`Label`]. The scenario models a plausible model per
//! label:
//!
//! | Label | Scripted behavior |
//! |---|---|
//! | `superfluous` | regeneration: an EASY restatement reusing the original's words (no judge script: row 5 is expected to decide it) |
//! | `valuable` | regeneration: a low-information DIFFICULT guess with `missingContext`; judge `KEEP_ORIGINAL` at 0.9 |
//! | `rewrite` | regeneration: the manifest's `rewriteTo`, MEDIUM; judge `KEEP_GENERATED` at 0.9 writing it. Pass 2 (keyed by the rewritten text): MEDIUM; judge `HYBRID` with a near-identical text, which the tie rail turns into `KEEP_ORIGINAL` |
//! | `functional` / `protected` | nothing: they never reach the model |
//!
//! Comments unknown to the manifest get the fake's default rule. Remove
//! mode additionally cites [`EXTRA_REGENERATIONS`] at declarations that
//! carry no comment (the engine records them as `NEW`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use munge_comments::artifact::types::{Difficulty, JudgeDecision};
use munge_comments::comments::sanitize::match_key;
use munge_comments::comments::types::Anchor;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::fake::{
    ExtraRegeneration, JudgeScript, RegenScript, Scenario, ScenarioEntry,
};
use munge_comments::llm::schemas::CitedPosition;
use munge_comments::{LanguageId, language_for};
use serde::Deserialize;

use super::fixture_path;

/// The fixture tree, relative to `tests/fixtures/`.
pub const NOISY_REPO: &str = "noisy-repo";

/// The manifest's file name at the fixture root. `.json` is not a source
/// extension, so discovery never picks it up.
pub const MANIFEST_FILE: &str = "ground-truth.json";

/// The `--similarity-threshold` the noisy-repo e2e plans with (the CLI
/// default). Every `superfluous` restatement reaches it.
pub const SIMILARITY_THRESHOLD: f64 = 0.6;

/// The confidence of every scripted judge verdict: comfortably above the
/// engine's 0.6 confidence floor.
pub const JUDGE_CONFIDENCE: f64 = 0.9;

/// The ground-truth label of one fixture comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Label {
    /// Restates the code (bloat included); should be removed.
    Superfluous,
    /// Carries information the code cannot; should be retained.
    Valuable,
    /// Worth keeping, but better said another way; should be rewritten.
    Rewrite,
    /// Functional (directives, pragmas, directive-adjacent prose, …);
    /// must be `KEEP_PROTECTED`.
    Functional,
    /// Doc-protected; must be `KEEP_PROTECTED`.
    Protected,
}

impl Label {
    /// Every label, in declaration order.
    pub const ALL: [Label; 5] = [
        Label::Superfluous,
        Label::Valuable,
        Label::Rewrite,
        Label::Functional,
        Label::Protected,
    ];

    /// The manifest spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Label::Superfluous => "superfluous",
            Label::Valuable => "valuable",
            Label::Rewrite => "rewrite",
            Label::Functional => "functional",
            Label::Protected => "protected",
        }
    }

    /// Whether comments with this label are prose (and so reach the
    /// engine's decision matrix past row 1).
    pub fn is_prose(self) -> bool {
        matches!(self, Label::Superfluous | Label::Valuable | Label::Rewrite)
    }
}

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestEntry {
    /// Root-relative POSIX path of the file.
    pub path: String,
    /// `match_key` of the comment's raw text.
    pub key: String,
    /// The comment's anchor name (`null` when the anchor has none).
    pub anchor_name: Option<String>,
    /// The ground-truth label.
    pub label: Label,
    /// For `rewrite` entries only: the improved text the scripted judge
    /// writes (delimiter-free).
    #[serde(default)]
    pub rewrite_to: Option<String>,
}

impl ManifestEntry {
    /// The entry's language, from its path's extension.
    pub fn language(&self) -> LanguageId {
        language_for(Path::new(&self.path))
            .unwrap_or_else(|| panic!("manifest path {} has no source extension", self.path))
    }

    /// The `FakeLlm` scenario key of this entry.
    pub fn scenario_key(&self) -> String {
        scenario_key(&self.path, &self.key)
    }
}

/// The ground-truth manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    /// Every labeled comment.
    pub entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// The entry for `(path, key)`, if any (the pair is unique).
    pub fn entry(&self, path: &str, key: &str) -> Option<&ManifestEntry> {
        self.entries
            .iter()
            .find(|entry| entry.path == path && entry.key == key)
    }

    /// The entries carrying `label`.
    pub fn with_label(&self, label: Label) -> impl Iterator<Item = &ManifestEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.label == label)
    }
}

/// Reads `tests/fixtures/noisy-repo/ground-truth.json`.
pub fn load_manifest() -> Manifest {
    let path = fixture_path(Path::new(NOISY_REPO).join(MANIFEST_FILE));
    let bytes = fs::read(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("{} is not a valid manifest: {err}", path.display()))
}

/// A path-qualified `FakeLlm` scenario key.
pub fn scenario_key(path: &str, key: &str) -> String {
    format!("{path}::{key}")
}

/// What the scripted model writes for a `valuable` comment: it sees what
/// the code does, but not why, so it rates itself DIFFICULT.
pub const VALUABLE_MISSING_CONTEXT: &str =
    "The reason this code is written this way is not visible from the code itself.";

/// The scripted pass-1 judge's text for a `rewrite` entry.
pub fn rewrite_text(entry: &ManifestEntry) -> &str {
    entry
        .rewrite_to
        .as_deref()
        .unwrap_or_else(|| panic!("rewrite entry {} has no rewriteTo", entry.scenario_key()))
}

/// The pass-2 `match_key` of a rewritten comment: the key its applied text
/// has (`rewriteTo` is delimiter-free, so its body is itself).
pub fn rewritten_key(entry: &ManifestEntry) -> String {
    match_key(rewrite_text(entry), adapter_for(entry.language()))
}

/// The pass-2 judge's `HYBRID` merge of a rewritten comment with an
/// equivalent regeneration: the same words, only the closing period
/// toggled — near-identical (a different `match_key`, word similarity 1),
/// so the tie rail must fire on similarity.
pub fn near_identical(text: &str) -> String {
    match text.strip_suffix('.') {
        Some(stripped) => stripped.to_owned(),
        None => format!("{text}."),
    }
}

/// The low-information guess the scripted model regenerates for a
/// `valuable` comment it cannot reconstruct.
fn valuable_guess(entry: &ManifestEntry) -> String {
    match &entry.anchor_name {
        Some(name) => format!("Sets up {name}."),
        None => "Runs the following statement.".to_owned(),
    }
}

fn regen(comment: String, difficulty: Difficulty, rationale: &str) -> RegenScript {
    RegenScript {
        comment,
        difficulty,
        would_you_comment_here: true,
        rationale: rationale.to_owned(),
        missing_context: None,
    }
}

fn judge(decision: JudgeDecision, final_comment: Option<String>, rationale: &str) -> JudgeScript {
    JudgeScript {
        decision,
        final_comment,
        confidence: JUDGE_CONFIDENCE,
        rationale: rationale.to_owned(),
    }
}

fn insert_unique(by_key: &mut BTreeMap<String, ScenarioEntry>, key: String, entry: ScenarioEntry) {
    if by_key.insert(key.clone(), entry).is_some() {
        panic!("two scenario entries share the key {key:?}");
    }
}

/// The remove-mode regenerations at locations that had no comment. Each
/// cites a named declaration of its file that carries no comment; one is
/// DIFFICULT, which the engine must hold for review instead of inserting.
pub fn extra_regenerations() -> Vec<ExtraRegeneration> {
    let extra = |path: &str,
                 node_kind: &str,
                 name: &str,
                 snippet: &str,
                 comment: &str,
                 difficulty: Difficulty| ExtraRegeneration {
        path: path.to_owned(),
        chunk_index: 0,
        anchor: Anchor {
            node_kind: node_kind.to_owned(),
            name: Some(name.to_owned()),
            snippet: snippet.to_owned(),
        },
        position: CitedPosition::Leading,
        comment: comment.to_owned(),
        difficulty,
    };
    vec![
        extra(
            "web/cart.js",
            "function_declaration",
            "cartTotal",
            "export function cartTotal(cart) {",
            "Sums each line with its tax and formats the total as dollars.",
            Difficulty::Medium,
        ),
        extra(
            "native/ring_buffer.c",
            "function_definition",
            "ring_pop",
            "int ring_pop(struct ring *r, unsigned char *out) {",
            "Pops the oldest byte into out; returns -1 when the buffer is empty.",
            Difficulty::Medium,
        ),
        extra(
            "tools/ingest.py",
            "function_definition",
            "units_by_account",
            "def units_by_account(rows):",
            "Aggregates billed units per account for the nightly run.",
            Difficulty::Difficult,
        ),
    ]
}

/// The `FakeLlm` scenario for the noisy repo, generated from `manifest`:
/// every `superfluous`, `valuable` and `rewrite` entry is scripted under
/// its path-qualified key (plus a pass-2 entry per `rewrite`), and
/// [`extra_regenerations`] are attached. Panics when two scripts would
/// share a key.
pub fn noisy_scenario(manifest: &Manifest) -> Scenario {
    let mut by_key = BTreeMap::new();
    for entry in &manifest.entries {
        match entry.label {
            Label::Superfluous => insert_unique(
                &mut by_key,
                entry.scenario_key(),
                ScenarioEntry {
                    regen: Some(regen(
                        entry.key.clone(),
                        Difficulty::Easy,
                        "The code says exactly this.",
                    )),
                    judge: None,
                },
            ),
            Label::Valuable => insert_unique(
                &mut by_key,
                entry.scenario_key(),
                ScenarioEntry {
                    regen: Some(RegenScript {
                        missing_context: Some(VALUABLE_MISSING_CONTEXT.to_owned()),
                        ..regen(
                            valuable_guess(entry),
                            Difficulty::Difficult,
                            "The code shows what happens, not why.",
                        )
                    }),
                    judge: Some(judge(
                        JudgeDecision::KeepOriginal,
                        None,
                        "The original explains something the code cannot.",
                    )),
                },
            ),
            Label::Rewrite => {
                let rewritten = rewrite_text(entry).to_owned();
                insert_unique(
                    &mut by_key,
                    entry.scenario_key(),
                    ScenarioEntry {
                        regen: Some(regen(
                            rewritten.clone(),
                            Difficulty::Medium,
                            "The intent is inferable from the code and its names.",
                        )),
                        judge: Some(judge(
                            JudgeDecision::KeepGenerated,
                            Some(rewritten.clone()),
                            "Same intent, stated clearly.",
                        )),
                    },
                );
                insert_unique(
                    &mut by_key,
                    scenario_key(&entry.path, &rewritten_key(entry)),
                    ScenarioEntry {
                        regen: Some(regen(
                            rewritten.clone(),
                            Difficulty::Medium,
                            "The intent is inferable from the code and its names.",
                        )),
                        judge: Some(judge(
                            JudgeDecision::Hybrid,
                            Some(near_identical(&rewritten)),
                            "Both versions say the same thing.",
                        )),
                    },
                );
            }
            Label::Functional | Label::Protected => {}
        }
    }
    Scenario {
        by_key,
        extra_regenerations: extra_regenerations(),
        failures: Vec::new(),
    }
}
