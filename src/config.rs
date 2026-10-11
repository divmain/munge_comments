//! Resolved configuration for `plan`, and the config snapshot stored in the
//! plan artifact.
//!
//! [`PlanConfig`] is part of the artifact schema and is therefore always
//! compiled. `resolve_plan` and `ResolvedPlan` take clap-derived argument
//! types and live behind the `cli` feature, so the `--no-default-features`
//! library build never sees them (hence plain code spans, not intra-doc
//! links, which would dangle in that build).
//!
//! Resolution precedence is flags over environment over defaults, evaluated
//! against an explicitly passed environment map (tests never touch process
//! state). Resolution performs no filesystem access: positional paths are
//! never stat'ed here — path existence and glob handling are owned by
//! discovery, which receives the injected cwd.

// Only the `cli`-gated resolution layer (and its tests) needs these; the
// artifact snapshot itself is path- and map-free.
#[cfg(feature = "cli")]
use std::collections::HashMap;
#[cfg(feature = "cli")]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::errors::Error;

/// Default model when neither flag nor environment names one.
pub const DEFAULT_MODEL: &str = "gpt-4.1";

/// Default OpenAI-compatible endpoint, applied when the client is built
/// (Phase 5). The artifact's `baseUrl` stays `None` unless flag or
/// environment set it, so the snapshot records only what the user chose.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Hiding strategy for prose comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Replace each prose comment with a stable-id placeholder in the
    /// language's own delimiter style.
    Redact,
    /// Delete target prose comments outright; the model re-cites where
    /// comments belong.
    Remove,
}

/// The config snapshot recorded in the plan artifact (camelCase keys,
/// unknown fields rejected so hand-edit typos fail loudly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanConfig {
    /// Hiding strategy (`--mode`).
    pub mode: Mode,
    /// LLM model name.
    pub model: String,
    /// OpenAI-compatible endpoint; `None` means the default endpoint.
    pub base_url: Option<String>,
    /// Whether NEW comments may be applied (`--allow-add`); honored by
    /// apply, hand-editable.
    pub allow_add: bool,
    /// Heuristic redundancy cutoff, in `[0, 1]`.
    pub similarity_threshold: f64,
    /// Chunking threshold, in Unicode scalar values.
    pub max_file_chars: u64,
    /// Parallel LLM files.
    pub concurrency: u64,
    /// No LLM calls; heuristic-only decisions.
    pub offline: bool,
    /// Discovery inputs as given on the request (cwd-relative or absolute;
    /// globs verbatim).
    pub paths: Vec<String>,
    /// Include globs; `None` means the built-in extension set derived from
    /// the language layer's extension table.
    pub include: Option<Vec<String>>,
    /// User-supplied extra exclude globs; the built-in defaults always apply
    /// on top of these.
    pub exclude: Vec<String>,
}

impl PlanConfig {
    /// Validates the fields the CLI cannot check at parse time and library
    /// consumers can get wrong: `concurrency >= 1`, `max_file_chars >= 1`,
    /// and `similarity_threshold` in `[0, 1]`.
    pub fn validate(&self) -> Result<(), Error> {
        if self.concurrency < 1 {
            return Err(Error::Usage(format!(
                "concurrency must be at least 1, got {}",
                self.concurrency
            )));
        }
        if self.max_file_chars < 1 {
            return Err(Error::Usage(format!(
                "max-file-chars must be at least 1, got {}",
                self.max_file_chars
            )));
        }
        if !(0.0..=1.0).contains(&self.similarity_threshold) {
            return Err(Error::Usage(format!(
                "similarity-threshold must be within 0..=1, got {}",
                self.similarity_threshold
            )));
        }
        Ok(())
    }
}

/// Everything `plan` needs after resolution: the artifact-bound config
/// snapshot, the resolved API key, and the CLI-owned output paths and flags.
///
/// Behind the `cli` feature because it carries clap-derived argument types.
#[cfg(feature = "cli")]
#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    /// The config snapshot, exactly as it will be recorded in the artifact.
    pub config: PlanConfig,
    /// API key from flag or environment; `None` when neither is set.
    pub api_key: Option<String>,
    /// Artifact output path (`--out`).
    pub out: PathBuf,
    /// Post-plan markdown report path (`--report`).
    pub report_path: PathBuf,
    /// Whether to resume from the existing `--out` artifact.
    pub resume: bool,
}

/// Environment variables consulted during resolution.
#[cfg(feature = "cli")]
mod env_names {
    pub const API_KEY: &str = "OPENAI_API_KEY";
    pub const BASE_URL: &str = "OPENAI_BASE_URL";
    pub const MODEL: &str = "MUNGE_COMMENTS_MODEL";
}

/// Resolves `plan` arguments against an explicit environment map.
///
/// Precedence: flags beat environment (`OPENAI_API_KEY`, `OPENAI_BASE_URL`,
/// `MUNGE_COMMENTS_MODEL`), environment beats defaults. `base_url` in the
/// snapshot is the flag value if set, else the environment value, else
/// `None`. Empty positional paths default to `["."]`; empty `--include`
/// defaults to the built-in extension set (`include: None`). Never touches
/// the filesystem.
#[cfg(feature = "cli")]
pub fn resolve_plan(
    args: &crate::cli::PlanArgs,
    env: &HashMap<String, String>,
) -> Result<ResolvedPlan, Error> {
    use crate::cli::ModeArg;

    let mode = match args.mode {
        ModeArg::Redact => Mode::Redact,
        ModeArg::Remove => Mode::Remove,
    };
    let model = args
        .model
        .clone()
        .or_else(|| env.get(env_names::MODEL).cloned())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let base_url = args
        .base_url
        .clone()
        .or_else(|| env.get(env_names::BASE_URL).cloned());
    let api_key = args
        .api_key
        .clone()
        .or_else(|| env.get(env_names::API_KEY).cloned());
    let paths = if args.paths.is_empty() {
        vec![".".to_string()]
    } else {
        args.paths.clone()
    };
    let include = if args.include.is_empty() {
        None
    } else {
        Some(args.include.clone())
    };
    let config = PlanConfig {
        mode,
        model,
        base_url,
        allow_add: args.allow_add,
        similarity_threshold: args.similarity_threshold,
        max_file_chars: args.max_file_chars,
        concurrency: args.concurrency,
        offline: args.offline,
        paths,
        include,
        exclude: args.exclude.clone(),
    };
    config.validate()?;
    Ok(ResolvedPlan {
        config,
        api_key,
        out: args.out.clone(),
        report_path: args.report.clone(),
        resume: args.resume,
    })
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use crate::cli::ModeArg;

    fn plan_args() -> crate::cli::PlanArgs {
        crate::cli::PlanArgs {
            paths: Vec::new(),
            mode: ModeArg::Redact,
            model: None,
            base_url: None,
            api_key: None,
            include: Vec::new(),
            exclude: Vec::new(),
            concurrency: 4,
            max_file_chars: 120_000,
            similarity_threshold: 0.6,
            allow_add: false,
            offline: false,
            resume: false,
            out: PathBuf::from("munge-comments-plan.json"),
            report: PathBuf::from("munge-comments-plan-report.md"),
        }
    }

    fn env_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn precedence_is_flag_over_env_over_default() {
        let env = env_with(&[
            ("OPENAI_API_KEY", "env-key"),
            ("OPENAI_BASE_URL", "http://env.example/v1"),
            ("MUNGE_COMMENTS_MODEL", "env-model"),
        ]);

        // Flags win over the environment.
        let mut flagged = plan_args();
        flagged.model = Some("flag-model".to_string());
        flagged.base_url = Some("http://flag.example/v1".to_string());
        flagged.api_key = Some("flag-key".to_string());
        let resolved = resolve_plan(&flagged, &env).expect("valid flags resolve");
        assert_eq!(resolved.config.model, "flag-model");
        assert_eq!(
            resolved.config.base_url.as_deref(),
            Some("http://flag.example/v1")
        );
        assert_eq!(resolved.api_key.as_deref(), Some("flag-key"));

        // The environment wins over defaults.
        let resolved = resolve_plan(&plan_args(), &env).expect("env-backed args resolve");
        assert_eq!(resolved.config.model, "env-model");
        assert_eq!(
            resolved.config.base_url.as_deref(),
            Some("http://env.example/v1"),
        );
        assert_eq!(resolved.api_key.as_deref(), Some("env-key"));

        // Defaults when neither flag nor environment is set.
        let resolved = resolve_plan(&plan_args(), &HashMap::new()).expect("bare args resolve");
        assert_eq!(resolved.config.model, DEFAULT_MODEL);
        assert_eq!(resolved.config.base_url, None);
        assert_eq!(resolved.api_key, None);
    }

    #[test]
    fn bare_arguments_resolve_to_documented_defaults() {
        let resolved = resolve_plan(&plan_args(), &HashMap::new()).expect("bare args resolve");
        let config = resolved.config;
        assert_eq!(config.mode, Mode::Redact);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.base_url, None);
        assert!(!config.allow_add);
        assert_eq!(config.similarity_threshold, 0.6);
        assert_eq!(config.max_file_chars, 120_000);
        assert_eq!(config.concurrency, 4);
        assert!(!config.offline);
        assert_eq!(config.paths, ["."]);
        assert_eq!(config.include, None);
        assert!(config.exclude.is_empty());
        assert!(!resolved.resume);
        assert_eq!(resolved.out, PathBuf::from("munge-comments-plan.json"));
        assert_eq!(
            resolved.report_path,
            PathBuf::from("munge-comments-plan-report.md"),
        );
    }

    #[test]
    fn user_arguments_pass_through_unchanged() {
        let mut args = plan_args();
        args.paths = vec!["src/".to_string(), "docs/".to_string()];
        args.mode = ModeArg::Remove;
        args.include = vec!["**/*.rs".to_string()];
        args.exclude = vec!["target/**".to_string()];
        args.allow_add = true;
        args.offline = true;
        args.resume = true;
        let resolved = resolve_plan(&args, &HashMap::new()).expect("explicit args resolve");
        assert_eq!(resolved.config.paths, ["src/", "docs/"]);
        assert_eq!(resolved.config.mode, Mode::Remove);
        assert_eq!(resolved.config.include, Some(vec!["**/*.rs".to_string()]),);
        assert_eq!(resolved.config.exclude, ["target/**"]);
        assert!(resolved.config.allow_add);
        assert!(resolved.config.offline);
        assert!(resolved.resume);
    }

    #[test]
    fn out_of_range_values_are_usage_errors() {
        let mut args = plan_args();
        args.concurrency = 0;
        let err = resolve_plan(&args, &HashMap::new()).expect_err("concurrency 0 rejected");
        assert!(matches!(err, Error::Usage(ref message) if message.contains("concurrency")));

        let mut args = plan_args();
        args.max_file_chars = 0;
        let err = resolve_plan(&args, &HashMap::new()).expect_err("max-file-chars 0 rejected");
        assert!(matches!(err, Error::Usage(ref message) if message.contains("max-file-chars")),);

        let mut args = plan_args();
        args.similarity_threshold = 1.5;
        let err =
            resolve_plan(&args, &HashMap::new()).expect_err("similarity-threshold 1.5 rejected");
        assert!(
            matches!(err, Error::Usage(ref message) if message.contains("similarity-threshold")),
        );

        let mut args = plan_args();
        args.similarity_threshold = -0.1;
        resolve_plan(&args, &HashMap::new()).expect_err("negative similarity-threshold rejected");
    }

    #[test]
    fn plan_config_snapshot_uses_camel_case_and_rejects_unknown_keys() {
        let config = plan_args_config();
        let json = serde_json::to_string(&config).expect("config serializes");
        assert!(json.contains("\"similarityThreshold\""));
        assert!(json.contains("\"maxFileChars\""));
        assert!(json.contains("\"allowAdd\""));
        assert!(json.contains("\"baseUrl\":null"));
        assert!(json.contains("\"mode\":\"redact\""));

        let round_tripped: PlanConfig = serde_json::from_str(&json).expect("config round-trips");
        assert_eq!(round_tripped, config);

        let mut value = serde_json::to_value(&config).expect("config to value");
        value["unexpectedKey"] = serde_json::json!(1);
        let tampered = serde_json::to_string(&value).expect("value serializes");
        assert!(
            serde_json::from_str::<PlanConfig>(&tampered).is_err(),
            "unknown keys must be rejected",
        );
    }

    fn plan_args_config() -> PlanConfig {
        resolve_plan(&plan_args(), &HashMap::new())
            .expect("bare args resolve")
            .config
    }
}
