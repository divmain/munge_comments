//! Wire response types for the two LLM calls, their semantic validation
//! ([`WireValidate`]), and the JSON Schema post-processor [`make_strict`]
//! that turns `schemars` output into an OpenAI strict-mode schema.
//!
//! Rules for wire structs (the opposite of the lenient envelope structs in
//! `client.rs`): `deny_unknown_fields`, `camelCase`, `Option<T>` only where
//! the field is genuinely nullable, no `#[serde(default)]` on non-`Option`
//! fields, no `skip_serializing_if`, and no integer fields (wire numbers are
//! `f64`). The shared enums and [`Anchor`] come from the artifact and comment
//! layers, so the wire and the artifact can never disagree on a name.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::artifact::types::{Difficulty, JudgeDecision};
use crate::comments::types::{Anchor, Position};

/// Semantic rules serde cannot express, checked after deserialization.
///
/// The error is the one sanctioned non-[`Error`](crate::errors::Error)
/// result of the library: its messages are prompt material for the repair
/// turn of [`complete_typed`](crate::llm::call::complete_typed), which turns
/// a second failure into `Error::Llm { reason: Invalid }`.
pub trait WireValidate {
    /// `Ok(())` when every rule holds; otherwise one message per violation.
    fn validate(&self) -> Result<(), Vec<String>>;
}

/// Call #1 (redact mode): one regenerated comment per placeholder id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "One regenerated comment per requested placeholder id.")]
pub struct RegenerationResponse {
    /// One entry per requested placeholder id.
    pub comments: Vec<RegeneratedComment>,
}

/// One blind regeneration for a redact-mode placeholder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "The comment you would write in place of one placeholder.")]
pub struct RegeneratedComment {
    /// The placeholder id, e.g. `C12`.
    pub id: String,
    /// Whether a comment is warranted here at all.
    pub would_you_comment_here: bool,
    /// The regenerated comment text, without delimiters. Non-empty when
    /// `wouldYouCommentHere` is true; may be empty when it is false.
    #[schemars(description = "The comment text, without delimiters. Non-empty when \
                       `wouldYouCommentHere` is true; may be empty when it is false.")]
    pub comment: String,
    /// How hard the comment's information is to recover from the code.
    pub difficulty: Difficulty,
    /// Why that difficulty.
    pub rationale: String,
    /// What context would have helped, if any.
    #[schemars(description = "What knowledge you lacked to write it confidently; null if none.")]
    pub missing_context: Option<String>,
}

/// Call #1 (remove mode): the model cites the code each comment annotates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "The comments you would write, each citing the code it annotates.")]
pub struct RemoveModeRegenerationResponse {
    /// One entry per comment the model would write.
    #[schemars(description = "One entry per comment you would write.")]
    pub comments: Vec<RemoveModeComment>,
}

/// One remove-mode regeneration, anchored by citation instead of by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "One comment, anchored by citing the code it annotates.")]
pub struct RemoveModeComment {
    /// The code the comment annotates, cited from the clean text.
    pub anchor: Anchor,
    /// Above the anchor (`leading`) or beside it (`trailing`).
    pub position: CitedPosition,
    /// The comment text, without delimiters.
    pub comment: String,
    /// How hard the comment's information is to recover from the code.
    pub difficulty: Difficulty,
    /// Why that difficulty.
    pub rationale: String,
    /// What context would have helped, if any.
    #[schemars(description = "What knowledge you lacked to write it confidently; null if none.")]
    pub missing_context: Option<String>,
}

/// The remove-mode citation subset of [`Position`]: a cited comment is
/// never `dangling` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Where the comment goes relative to the anchor.")]
pub enum CitedPosition {
    /// On its own line(s) above the anchor.
    Leading,
    /// On the anchor's line, after the code.
    Trailing,
}

impl From<CitedPosition> for Position {
    fn from(cited: CitedPosition) -> Self {
        match cited {
            CitedPosition::Leading => Position::Leading,
            CitedPosition::Trailing => Position::Trailing,
        }
    }
}

/// Call #2: one decision per judge packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "One decision per packet.")]
pub struct JudgeResponse {
    /// One entry per packet id.
    pub decisions: Vec<JudgeItem>,
}

/// The judge's verdict on one packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Your decision on one packet.")]
pub struct JudgeItem {
    /// The packet id, e.g. `C12`.
    pub id: String,
    /// One of the packet's allowed decisions.
    pub decision: JudgeDecision,
    /// The exact text to write; required for `KEEP_GENERATED` and
    /// `HYBRID`, ignored otherwise.
    #[schemars(
        description = "The exact comment text to write, without delimiters; required for \
                       `KEEP_GENERATED` and `HYBRID`, otherwise null."
    )]
    pub final_comment: Option<String>,
    /// Confidence in `[0, 1]`.
    pub confidence: f64,
    /// Why this decision.
    pub rationale: String,
}

/// Collects violations, then converts them into the `validate` result.
#[derive(Default)]
struct Violations(Vec<String>);

impl Violations {
    fn push(&mut self, message: String) {
        self.0.push(message);
    }

    /// Flags blank ids and ids repeated within one response.
    fn check_ids<'a>(&mut self, list: &str, ids: impl Iterator<Item = &'a str>) {
        let mut seen = HashSet::new();
        for (index, id) in ids.enumerate() {
            if id.trim().is_empty() {
                self.push(format!("{list}[{index}].id must be a non-empty string"));
            } else if !seen.insert(id) {
                self.push(format!(
                    "{list}[{index}].id \"{id}\" is a duplicate; answer each id exactly once"
                ));
            }
        }
    }

    fn into_result(self) -> Result<(), Vec<String>> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.0)
        }
    }
}

impl WireValidate for RegenerationResponse {
    fn validate(&self) -> Result<(), Vec<String>> {
        let mut violations = Violations::default();
        violations.check_ids("comments", self.comments.iter().map(|c| c.id.as_str()));
        for (index, comment) in self.comments.iter().enumerate() {
            if comment.would_you_comment_here && comment.comment.trim().is_empty() {
                violations.push(format!(
                    "comments[{index}].comment must be non-empty when wouldYouCommentHere is true"
                ));
            }
        }
        violations.into_result()
    }
}

impl WireValidate for RemoveModeRegenerationResponse {
    fn validate(&self) -> Result<(), Vec<String>> {
        let mut violations = Violations::default();
        for (index, comment) in self.comments.iter().enumerate() {
            if comment.comment.trim().is_empty() {
                violations.push(format!("comments[{index}].comment must be non-empty"));
            }
        }
        violations.into_result()
    }
}

impl WireValidate for JudgeResponse {
    fn validate(&self) -> Result<(), Vec<String>> {
        let mut violations = Violations::default();
        violations.check_ids("decisions", self.decisions.iter().map(|d| d.id.as_str()));
        for (index, item) in self.decisions.iter().enumerate() {
            // NaN fails the range check too.
            if !(0.0..=1.0).contains(&item.confidence) {
                violations.push(format!(
                    "decisions[{index}].confidence must be a number in [0, 1], got {}",
                    item.confidence
                ));
            }
        }
        violations.into_result()
    }
}

/// Keywords whose value is a single subschema.
const SCHEMA_KEYWORDS: [&str; 7] = [
    "items",
    "additionalProperties",
    "not",
    "if",
    "then",
    "else",
    "contains",
];
/// Keywords whose value is an array of subschemas.
const SCHEMA_ARRAY_KEYWORDS: [&str; 4] = ["anyOf", "oneOf", "allOf", "prefixItems"];
/// Keywords whose value maps names to subschemas.
const SCHEMA_MAP_KEYWORDS: [&str; 4] = ["properties", "$defs", "definitions", "patternProperties"];

/// Rewrites a `schemars` schema into an OpenAI strict-mode schema, in
/// place, walking every subschema (including `$defs`):
///
/// - every object schema gets `additionalProperties: false` and `required`
///   = all of its property keys (nullability is already in the types);
/// - `$schema`, `$id` and every `format` keyword are deleted;
/// - every `oneOf` becomes `anyOf` (schemars emits documented unit enum
///   variants as `oneOf` of `const`s; strict mode supports only `anyOf`);
/// - a schema holding `$ref` keeps only `$ref` (strict mode rejects
///   keywords beside `$ref`, e.g. the `description` of a documented field).
///
/// The walk is keyword-aware: it descends only into keywords whose values
/// are schemas, so a property *named* `format` or `$ref` is never touched.
pub fn make_strict(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return; // boolean schemas carry no keywords
    };
    strict_object(object);
}

fn strict_object(object: &mut Map<String, Value>) {
    if let Some(reference) = object.remove("$ref") {
        object.clear();
        object.insert("$ref".to_string(), reference);
        return;
    }
    object.remove("$schema");
    object.remove("$id");
    object.remove("format");
    if let Some(one_of) = object.remove("oneOf") {
        match object.remove("anyOf") {
            None => {
                object.insert("anyOf".to_string(), one_of);
            }
            // Never emitted by schemars for the wire types; strict mode cannot express both, so require each via `allOf`.
            Some(any_of) => {
                let both = Value::Array(vec![
                    Value::Object(Map::from_iter([("anyOf".to_string(), any_of)])),
                    Value::Object(Map::from_iter([("anyOf".to_string(), one_of)])),
                ]);
                object.insert("allOf".to_string(), both);
            }
        }
    }
    if is_object_schema(object) {
        let keys: Vec<Value> = match object.get("properties") {
            Some(Value::Object(properties)) => {
                properties.keys().cloned().map(Value::String).collect()
            }
            _ => Vec::new(),
        };
        object.insert("required".to_string(), Value::Array(keys));
        object.insert("additionalProperties".to_string(), Value::Bool(false));
    }
    for keyword in SCHEMA_KEYWORDS {
        if let Some(sub) = object.get_mut(keyword) {
            make_strict(sub);
        }
    }
    for keyword in SCHEMA_ARRAY_KEYWORDS {
        if let Some(Value::Array(subs)) = object.get_mut(keyword) {
            subs.iter_mut().for_each(make_strict);
        }
    }
    for keyword in SCHEMA_MAP_KEYWORDS {
        if let Some(Value::Object(subs)) = object.get_mut(keyword) {
            subs.values_mut().for_each(make_strict);
        }
    }
}

/// Whether a schema describes an object: `type: "object"`, a type list
/// containing `"object"`, or a `properties` keyword.
fn is_object_schema(object: &Map<String, Value>) -> bool {
    let typed_object = match object.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
        _ => false,
    };
    typed_object || object.contains_key("properties")
}

/// The strict schema of a wire type: `schema_for!(T)` through
/// [`make_strict`]. Sent in `response_format` and embedded in prompts.
pub fn strict_schema<T: JsonSchema>() -> Value {
    let mut schema = schemars::schema_for!(T).to_value();
    make_strict(&mut schema);
    schema
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn raw_schema<T: JsonSchema>() -> Value {
        schemars::schema_for!(T).to_value()
    }

    fn wire_schemas() -> Vec<(&'static str, Value)> {
        vec![
            ("regeneration", strict_schema::<RegenerationResponse>()),
            (
                "remove-mode",
                strict_schema::<RemoveModeRegenerationResponse>(),
            ),
            ("judge", strict_schema::<JudgeResponse>()),
        ]
    }

    /// Every JSON object anywhere in `value` (a superset of the subschemas).
    fn all_objects(value: &Value) -> Vec<&Map<String, Value>> {
        let mut out = Vec::new();
        let mut stack = vec![value];
        while let Some(current) = stack.pop() {
            match current {
                Value::Object(object) => {
                    out.push(object);
                    stack.extend(object.values());
                }
                Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
        out
    }

    fn has_key_anywhere(value: &Value, key: &str) -> bool {
        all_objects(value).iter().any(|o| o.contains_key(key))
    }

    #[test]
    fn every_object_schema_is_closed_and_requires_all_keys() {
        for (name, schema) in wire_schemas() {
            let mut checked = 0;
            for object in all_objects(&schema) {
                let Some(Value::Object(properties)) = object.get("properties") else {
                    continue;
                };
                checked += 1;
                assert_eq!(
                    object.get("additionalProperties"),
                    Some(&Value::Bool(false)),
                    "{name}: {object:?}"
                );
                let mut required: Vec<&str> = object["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| k.as_str().unwrap())
                    .collect();
                required.sort_unstable();
                let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
                keys.sort_unstable();
                assert_eq!(required, keys, "{name}");
            }
            assert!(checked >= 2, "{name}: root and at least one $defs object");
        }
    }

    #[test]
    fn defs_are_processed() {
        let schema = strict_schema::<JudgeResponse>();
        let item = &schema["$defs"]["JudgeItem"];
        // `finalComment` is an `Option`, which schemars leaves out of `required`; strict mode must list every key.
        assert!(
            !raw_schema::<JudgeResponse>()["$defs"]["JudgeItem"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("finalComment"))
        );
        assert!(
            item["required"]
                .as_array()
                .unwrap()
                .contains(&json!("finalComment"))
        );
        assert_eq!(item["properties"]["confidence"].get("format"), None);
    }

    #[test]
    fn no_banned_keywords_survive() {
        // The raw output really does carry them, so the rewrite is exercised.
        let raw = raw_schema::<JudgeResponse>();
        assert!(raw.get("$schema").is_some());
        assert!(has_key_anywhere(&raw, "format"));
        for (name, schema) in wire_schemas() {
            for key in ["$schema", "$id", "format", "oneOf"] {
                assert!(!has_key_anywhere(&schema, key), "{name} still has {key}");
            }
        }
    }

    #[test]
    fn documented_enum_variants_become_any_of() {
        // `Difficulty`'s variants carry `///` docs, so schemars emits `oneOf`.
        let raw = raw_schema::<RegenerationResponse>();
        assert!(raw["$defs"]["Difficulty"].get("oneOf").is_some());
        let strict = strict_schema::<RegenerationResponse>();
        let any_of = strict["$defs"]["Difficulty"]["anyOf"].as_array().unwrap();
        let names: Vec<&str> = any_of
            .iter()
            .map(|v| v["const"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["EASY", "MEDIUM", "DIFFICULT"]);
    }

    #[test]
    fn ref_objects_have_no_sibling_keywords() {
        // A documented field of enum type: schemars puts its doc beside `$ref`; strict mode forbids siblings.
        let raw = raw_schema::<RegenerationResponse>();
        let difficulty = &raw["$defs"]["RegeneratedComment"]["properties"]["difficulty"];
        assert!(difficulty.get("$ref").is_some() && difficulty.get("description").is_some());
        for (name, schema) in wire_schemas() {
            for object in all_objects(&schema) {
                if object.contains_key("$ref") {
                    assert_eq!(object.len(), 1, "{name}: {object:?}");
                }
            }
        }
        let strict = strict_schema::<RegenerationResponse>();
        assert_eq!(
            strict["$defs"]["RegeneratedComment"]["properties"]["difficulty"],
            json!({"$ref": "#/$defs/Difficulty"})
        );
    }

    #[test]
    fn the_walk_is_keyword_aware() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "format": {"type": "string", "format": "uri"},
                "$ref": {"type": "number"}
            }
        });
        make_strict(&mut schema);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "format": {"type": "string"},
                    "$ref": {"type": "number"}
                },
                "required": ["$ref", "format"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn one_of_beside_any_of_requires_both() {
        let mut schema = json!({"anyOf": [{"type": "string"}], "oneOf": [{"type": "null"}]});
        make_strict(&mut schema);
        assert_eq!(
            schema,
            json!({"allOf": [
                {"anyOf": [{"type": "string"}]},
                {"anyOf": [{"type": "null"}]}
            ]})
        );
    }

    fn regenerated(id: &str, would: bool, comment: &str) -> RegeneratedComment {
        RegeneratedComment {
            id: id.to_string(),
            would_you_comment_here: would,
            comment: comment.to_string(),
            difficulty: Difficulty::Easy,
            rationale: "r".to_string(),
            missing_context: None,
        }
    }

    fn judged(id: &str, confidence: f64) -> JudgeItem {
        JudgeItem {
            id: id.to_string(),
            decision: JudgeDecision::Remove,
            final_comment: None,
            confidence,
            rationale: "r".to_string(),
        }
    }

    #[test]
    fn regeneration_validation() {
        let ok = RegenerationResponse {
            comments: vec![regenerated("C1", true, "why"), regenerated("C2", false, "")],
        };
        assert_eq!(ok.validate(), Ok(()));
        let bad = RegenerationResponse {
            comments: vec![
                regenerated("C1", true, "  "),
                regenerated("", false, ""),
                regenerated("C1", false, "x"),
            ],
        };
        let errors = bad.validate().unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("wouldYouCommentHere")));
        assert!(errors.iter().any(|e| e.contains("non-empty string")));
        assert!(errors.iter().any(|e| e.contains("duplicate")));
    }

    #[test]
    fn remove_mode_validation_requires_comment_text() {
        let comment = |text: &str| RemoveModeComment {
            anchor: Anchor {
                node_kind: "function_declaration".to_string(),
                name: Some("f".to_string()),
                snippet: "function f() {}".to_string(),
            },
            position: CitedPosition::Leading,
            comment: text.to_string(),
            difficulty: Difficulty::Medium,
            rationale: "r".to_string(),
            missing_context: None,
        };
        let ok = RemoveModeRegenerationResponse {
            comments: vec![comment("why")],
        };
        assert_eq!(ok.validate(), Ok(()));
        let bad = RemoveModeRegenerationResponse {
            comments: vec![comment("")],
        };
        assert_eq!(bad.validate().unwrap_err().len(), 1);
    }

    #[test]
    fn judge_validation() {
        let ok = JudgeResponse {
            decisions: vec![judged("C1", 0.0), judged("C2", 1.0)],
        };
        assert_eq!(ok.validate(), Ok(()));
        let bad = JudgeResponse {
            decisions: vec![
                judged("C1", 1.5),
                judged("C2", -0.1),
                judged("C3", f64::NAN),
                judged("C3", 0.5),
                judged(" ", 0.5),
            ],
        };
        assert_eq!(bad.validate().unwrap_err().len(), 5);
    }

    #[test]
    fn wire_structs_are_strict_on_read() {
        let extra = r#"{"decisions": [], "note": 1}"#;
        assert!(serde_json::from_str::<JudgeResponse>(extra).is_err());
        let missing = r#"{"decisions": [{"id": "C1", "decision": "REMOVE", "confidence": 1}]}"#;
        assert!(serde_json::from_str::<JudgeResponse>(missing).is_err());
        let wrong_name = r#"{"decisions": [{"id": "C1", "decision": "NEW",
            "finalComment": null, "confidence": 1, "rationale": "r"}]}"#;
        assert!(serde_json::from_str::<JudgeResponse>(wrong_name).is_err());
        // A missing nullable key is accepted as `None` (json_object leniency): an omitted `Option` field deserializes to `None` without `#[serde(default)]`.
        let lenient = r#"{"decisions": [{"id": "C1", "decision": "REMOVE",
            "confidence": 1, "rationale": "r"}]}"#;
        let parsed: JudgeResponse = serde_json::from_str(lenient).unwrap();
        assert_eq!(parsed.decisions[0].final_comment, None);
        let dangling = r#"{"comments": [{"anchor": {"nodeKind": "x", "name": null,
            "snippet": ""}, "position": "dangling", "comment": "c", "difficulty": "EASY",
            "rationale": "r", "missingContext": null}]}"#;
        assert!(serde_json::from_str::<RemoveModeRegenerationResponse>(dangling).is_err());
    }

    #[test]
    fn cited_position_maps_onto_position() {
        assert_eq!(Position::from(CitedPosition::Leading), Position::Leading);
        assert_eq!(Position::from(CitedPosition::Trailing), Position::Trailing);
    }
}
