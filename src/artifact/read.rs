//! The artifact validation boundary: read, deserialize, refine.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::error::Category;

use super::types::{PlanArtifact, ValidatedArtifact};
use crate::errors::Error;

/// Reads and validates the plan artifact at absolute `path`.
///
/// A relative `path` is `Error::Usage` (callers join it onto their cwd; the
/// process cwd is never consulted). A missing file, invalid or truncated
/// JSON, a schema mismatch (unknown key, unknown enum value, wrong type)
/// and any [`PlanArtifact::validate`] refinement failure are all
/// `Error::Artifact` (exit 2); parse messages carry the artifact path and
/// the line/column. Any other failure to read the file is `Error::Fs`.
pub fn read_artifact(path: &Path) -> Result<ValidatedArtifact, Error> {
    if !path.is_absolute() {
        return Err(Error::Usage(format!(
            "the artifact path must be absolute, got {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|source| match source.kind() {
        ErrorKind::NotFound => {
            Error::Artifact(format!("{}: no such artifact file", path.display()))
        }
        _ => Error::Fs {
            path: path.to_path_buf(),
            source,
        },
    })?;
    let artifact: PlanArtifact = serde_json::from_slice(&bytes).map_err(|err| {
        let what = match err.classify() {
            Category::Eof => "truncated artifact JSON (unexpected end of input)",
            Category::Syntax => "malformed artifact JSON",
            Category::Data => "artifact JSON does not match the schema",
            Category::Io => "unreadable artifact JSON",
        };
        Error::Artifact(format!("{}: {what}: {err}", path.display()))
    })?;
    artifact.validate().map_err(|err| match err {
        Error::Artifact(detail) => {
            Error::Artifact(format!("{}: invalid artifact: {detail}", path.display()))
        }
        other => other,
    })
}
