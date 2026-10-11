//! Retention measurement against the noisy-repo ground-truth manifest,
//! shared by the `FakeLlm` e2e (`tests/noisy_repo.rs`) and the env-gated
//! real-endpoint e2e (`tests/real_endpoint.rs`).
//!
//! Scanned records are joined to manifest entries by `(path, match_key)`
//! of their original text — never by line — and the join is asserted to be
//! a bijection before anything is measured. The targets, overall and per
//! adapter id (a label with no entries for a language passes vacuously):
//!
//! - ≥ 90% of `superfluous` comments → `REMOVE`;
//! - 100% of `functional` and of `protected` → `KEEP_PROTECTED`;
//! - ≥ 80% of `valuable` comments retained (anything but `REMOVE`).

use munge_comments::artifact::types::CommentRecord;
use munge_comments::comments::sanitize::match_key;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::{Decision, LanguageId, PlanArtifact};

use super::noisy_scenario::{Label, Manifest};

/// Superfluous comments that must be removed.
pub const SUPERFLUOUS_REMOVED: f64 = 0.9;
/// Valuable comments that must be retained.
pub const VALUABLE_RETAINED: f64 = 0.8;

/// One scanned record joined to its manifest entry.
pub struct Joined<'a> {
    pub path: &'a str,
    pub language: LanguageId,
    pub label: Label,
    /// The entry's index in the manifest.
    pub entry: usize,
    pub record: &'a CommentRecord,
}

/// Joins every scanned (non-NEW) record of `artifact` to its manifest
/// entry, asserting the join is a bijection: every record has exactly one
/// entry and every entry exactly one record.
pub fn join<'a>(manifest: &Manifest, artifact: &'a PlanArtifact) -> Vec<Joined<'a>> {
    let mut joined = Vec::new();
    let mut seen = vec![0_usize; manifest.entries.len()];
    for file in &artifact.files {
        let adapter = adapter_for(file.language);
        for record in file
            .comments
            .iter()
            .filter(|record| !record.is_new_origin())
        {
            let original = record.original.as_deref().unwrap();
            let key = match_key(original, adapter);
            let entry = manifest
                .entries
                .iter()
                .position(|entry| entry.path == file.path && entry.key == key)
                .unwrap_or_else(|| {
                    panic!(
                        "{} {}: {original:?} has no manifest entry",
                        file.path, record.id
                    )
                });
            seen[entry] += 1;
            joined.push(Joined {
                path: &file.path,
                language: file.language,
                label: manifest.entries[entry].label,
                entry,
                record,
            });
        }
    }
    for (entry, count) in manifest.entries.iter().zip(&seen) {
        assert_eq!(
            *count,
            1,
            "{} must join exactly one record",
            entry.scenario_key()
        );
    }
    joined
}

/// Whether `decision` meets `label`'s target.
pub fn meets(label: Label, decision: Decision) -> bool {
    match label {
        Label::Superfluous => decision == Decision::Remove,
        Label::Valuable => decision != Decision::Remove,
        Label::Functional | Label::Protected => decision == Decision::KeepProtected,
        Label::Rewrite => unreachable!("rewrites are checked against the applied text"),
    }
}

/// The minimum fraction of `label` entries that must meet the target.
pub fn target(label: Label) -> f64 {
    match label {
        Label::Superfluous => SUPERFLUOUS_REMOVED,
        Label::Valuable => VALUABLE_RETAINED,
        Label::Functional | Label::Protected => 1.0,
        Label::Rewrite => unreachable!("rewrites have no retention target"),
    }
}

/// Asserts the four retention targets on `artifact`, overall and per
/// adapter id (vacuous for a language with no entries of a label). Every
/// miss is listed in the failure message.
pub fn assert_retention(manifest: &Manifest, artifact: &PlanArtifact, what: &str) {
    let joined = join(manifest, artifact);
    let mut failures = Vec::new();
    for label in [
        Label::Superfluous,
        Label::Valuable,
        Label::Functional,
        Label::Protected,
    ] {
        let scopes = std::iter::once(None).chain(LanguageId::ALL.into_iter().map(Some));
        for scope in scopes {
            let in_scope: Vec<&Joined> = joined
                .iter()
                .filter(|j| j.label == label && scope.is_none_or(|lang| j.language == lang))
                .collect();
            if in_scope.is_empty() {
                assert!(
                    scope.is_some(),
                    "the manifest has {} entries",
                    label.as_str()
                );
                continue;
            }
            let met = in_scope
                .iter()
                .filter(|j| meets(label, j.record.decision))
                .count();
            let fraction = met as f64 / in_scope.len() as f64;
            if fraction < target(label) {
                let misses: Vec<String> = in_scope
                    .iter()
                    .filter(|j| !meets(label, j.record.decision))
                    .map(|j| {
                        format!(
                            "{} {} {:?} → {} ({:?})",
                            j.path,
                            j.record.id,
                            j.record.original.as_deref().unwrap_or_default(),
                            j.record.decision.as_str(),
                            j.record.decision_rationale
                        )
                    })
                    .collect();
                failures.push(format!(
                    "{what}: {} in {}: {met}/{} = {fraction:.3} < {}\n  {}",
                    label.as_str(),
                    scope.map_or("all languages", LanguageId::as_str),
                    in_scope.len(),
                    target(label),
                    misses.join("\n  ")
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
