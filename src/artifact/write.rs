//! The incremental artifact writer.
//!
//! `plan` writes the artifact as soon as the run starts, after each file
//! completes, and once at the end. Every write is atomic, keeps `files`
//! sorted by path, and recomputes the summary, so an empty discovery, an
//! all-carried `--resume`, or a run cancelled before its first file
//! completes still leaves a valid artifact on disk. The writer is owned by
//! the single driver loop, so writes are serialized by construction.

use std::path::{Path, PathBuf};

use super::types::{ARTIFACT_VERSION, FilePlan, PlanArtifact, Summary};
use crate::config::PlanConfig;
use crate::errors::Error;
use crate::util::fs::atomic_write;

/// Computes the informational summary from the file plans and the config
/// snapshot (the config matters: `char_reduction_pct` counts NEW insertions
/// only when `allowAdd`). The shape of `report::stats::compute_summary`.
pub type SummarizeFn = fn(&[FilePlan], &PlanConfig) -> Result<Summary, Error>;

/// The run-level fields of an artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactHeader {
    /// RFC 3339 UTC creation time, seconds precision.
    pub created_at: String,
    /// The source root, POSIX, relative to the artifact's directory.
    pub root: String,
    /// The glob base, POSIX, relative to `root`.
    pub glob_base: String,
    /// The config snapshot.
    pub config: PlanConfig,
}

/// Serializes an artifact exactly as it is stored: pretty-printed JSON
/// (2-space indent, keys in struct order) with a trailing newline.
pub fn artifact_to_json(artifact: &PlanArtifact) -> Result<String, Error> {
    let mut json = serde_json::to_string_pretty(artifact)
        .map_err(|err| Error::Invariant(format!("serializing the plan artifact: {err}")))?;
    json.push('\n');
    Ok(json)
}

/// Owns the in-progress artifact and rewrites it atomically on every
/// change.
#[derive(Debug)]
pub struct ArtifactWriter {
    path: PathBuf,
    artifact: PlanArtifact,
    summarize: SummarizeFn,
}

impl ArtifactWriter {
    /// Creates the writer and immediately writes the header plus
    /// `initial_files` (files carried by `--resume`; later entries replace
    /// earlier ones with the same path) to absolute `path`.
    ///
    /// Write failures are `Error::Fs`; summarize failures propagate as
    /// returned (`Error::Invariant` from `compute_summary`).
    pub fn new(
        path: &Path,
        header: ArtifactHeader,
        initial_files: Vec<FilePlan>,
        summarize: SummarizeFn,
    ) -> Result<Self, Error> {
        let ArtifactHeader {
            created_at,
            root,
            glob_base,
            config,
        } = header;
        let mut writer = Self {
            path: path.to_path_buf(),
            artifact: PlanArtifact {
                version: ARTIFACT_VERSION,
                created_at,
                root,
                glob_base,
                config,
                files: Vec::new(),
                summary: Summary::default(),
            },
            summarize,
        };
        for file in initial_files {
            writer.upsert(file);
        }
        writer.write()?;
        Ok(writer)
    }

    /// Inserts `file` (or replaces the entry with the same path), keeping
    /// `files` sorted, recomputes the summary, and rewrites the artifact.
    pub fn record(&mut self, file: FilePlan) -> Result<(), Error> {
        self.upsert(file);
        self.write()
    }

    /// [`record`](Self::record) for a batch: every file is upserted, then
    /// the artifact is rewritten once.
    pub fn record_all(&mut self, files: impl IntoIterator<Item = FilePlan>) -> Result<(), Error> {
        for file in files {
            self.upsert(file);
        }
        self.write()
    }

    /// Performs the final write and returns the artifact as written.
    pub fn finish(mut self) -> Result<PlanArtifact, Error> {
        self.write()?;
        Ok(self.artifact)
    }

    /// The artifact as last recorded (its summary as of the last write).
    pub fn artifact(&self) -> &PlanArtifact {
        &self.artifact
    }

    fn upsert(&mut self, file: FilePlan) {
        let files = &mut self.artifact.files;
        match files.binary_search_by(|probe| probe.path.as_str().cmp(&file.path)) {
            Ok(index) => files[index] = file,
            Err(index) => files.insert(index, file),
        }
    }

    fn write(&mut self) -> Result<(), Error> {
        self.artifact.summary = (self.summarize)(&self.artifact.files, &self.artifact.config)?;
        let json = artifact_to_json(&self.artifact)?;
        atomic_write(&self.path, json.as_bytes())
    }
}
