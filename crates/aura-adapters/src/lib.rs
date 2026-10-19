//! Provider adapters: how to start each agent CLI headlessly, and how to
//! recognise a subscription-limit refusal in its output.
//!
//! Adapters are pure. They build argv and parse text. Process supervision
//! belongs to the daemon.

pub mod limit;

use aura_core::config::{parse_duration, ProviderConfig};
use aura_core::LimitSignal;
use chrono::{DateTime, Utc};
use regex::Regex;
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;

pub use limit::{detect_limit, DEFAULT_LIMIT_PATTERNS};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdapterError {
    #[error("provider {0} is disabled")]
    Disabled(String),
    #[error("invalid limit pattern {pattern:?} for {provider}: {reason}")]
    BadPattern {
        provider: String,
        pattern: String,
        reason: String,
    },
}

/// A fully resolved command, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// One provider CLI, configured from `aura.toml`.
#[derive(Debug, Clone)]
pub struct CliAdapter {
    name: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    limit_patterns: Vec<Regex>,
    window: Option<Duration>,
}

impl CliAdapter {
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<CliAdapter, AdapterError> {
        if !cfg.enabled {
            return Err(AdapterError::Disabled(name.to_string()));
        }
        let mut patterns = Vec::new();
        for text in cfg
            .limit_patterns
            .iter()
            .map(String::as_str)
            .chain(DEFAULT_LIMIT_PATTERNS.iter().copied())
        {
            let regex =
                Regex::new(&format!("(?i){text}")).map_err(|e| AdapterError::BadPattern {
                    provider: name.to_string(),
                    pattern: text.to_string(),
                    reason: e.to_string(),
                })?;
            patterns.push(regex);
        }
        Ok(CliAdapter {
            name: name.to_string(),
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            limit_patterns: patterns,
            window: cfg.quota_window.as_deref().and_then(parse_duration),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Length of one subscription window, when configured.
    pub fn window(&self) -> Option<Duration> {
        self.window
    }

    /// The command for one run. `{prompt}` in an argument is replaced with
    /// `prompt`; no shell is involved.
    pub fn invocation(&self, prompt: &str) -> Invocation {
        Invocation {
            program: self.command.clone(),
            args: self
                .args
                .iter()
                .map(|a| a.replace("{prompt}", prompt))
                .collect(),
            env: self.env.clone(),
        }
    }

    /// Looks for a refusal in a run's output.
    pub fn detect_limit(&self, output: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
        detect_limit(&self.limit_patterns, output, now)
    }
}

/// Builds an adapter for every enabled provider in the config.
pub fn build_all(
    providers: &BTreeMap<String, ProviderConfig>,
) -> Result<BTreeMap<String, CliAdapter>, AdapterError> {
    let mut out = BTreeMap::new();
    for (name, cfg) in providers {
        if cfg.enabled {
            out.insert(name.clone(), CliAdapter::new(name, cfg)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::config::builtin_providers;
    use chrono::TimeZone;

    fn claude() -> CliAdapter {
        CliAdapter::new("claude-code", &builtin_providers()["claude-code"]).unwrap()
    }

    #[test]
    fn invocation_substitutes_prompt_without_a_shell() {
        let inv = claude().invocation("do the thing; rm -rf /");
        assert_eq!(inv.program, "claude");
        assert!(inv.args.contains(&"do the thing; rm -rf /".to_string()));
        assert!(inv.args.contains(&"acceptEdits".to_string()));
    }

    #[test]
    fn disabled_provider_has_no_adapter() {
        let cfg = builtin_providers()["hermes"].clone();
        assert_eq!(
            CliAdapter::new("hermes", &cfg).unwrap_err(),
            AdapterError::Disabled("hermes".into())
        );
    }

    #[test]
    fn custom_pattern_is_compiled() {
        let mut cfg = builtin_providers()["codex"].clone();
        cfg.limit_patterns = vec!["maxed out".into()];
        let adapter = CliAdapter::new("codex", &cfg).unwrap();
        assert!(adapter
            .detect_limit("you are maxed out", Utc::now())
            .is_some());
    }

    #[test]
    fn bad_custom_pattern_is_reported() {
        let mut cfg = builtin_providers()["codex"].clone();
        cfg.limit_patterns = vec!["(unclosed".into()];
        assert!(matches!(
            CliAdapter::new("codex", &cfg),
            Err(AdapterError::BadPattern { .. })
        ));
    }

    #[test]
    fn fixture_without_limit_is_not_flagged() {
        let text = include_str!("../tests/fixtures/no-limit.txt");
        assert!(claude().detect_limit(text, Utc::now()).is_none());
    }

    #[test]
    fn build_all_skips_disabled_providers() {
        let adapters = build_all(&builtin_providers()).unwrap();
        assert!(adapters.contains_key("claude-code"));
        assert!(!adapters.contains_key("antigravity"));
        let _ = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0);
    }
}
