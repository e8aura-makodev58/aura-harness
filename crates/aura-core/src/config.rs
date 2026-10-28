//! Configuration file (`aura.toml`) and the state directory layout.

use crate::model::Autonomy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub repo: RepoConfig,
    pub defaults: Defaults,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub roles: Roles,
    pub budgets: Budgets,
    pub acceptance: Acceptance,
    pub web: WebConfig,
    /// Where the database, worktrees, logs and socket live.
    /// Defaults to `$HOME/.local/share/aura`.
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct RepoConfig {
    pub path: PathBuf,
    pub main_branch: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Defaults {
    pub autonomy: Autonomy,
    pub max_parallel_runs: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct ProviderConfig {
    pub command: String,
    /// Argument template. `{prompt}` is replaced by the fixed prompt, and the
    /// run's working directory is the task worktree.
    pub args: Vec<String>,
    pub max_parallel: usize,
    /// Length of the subscription window, such as `5h`. Used only when the
    /// provider refuses a run without saying when it resets.
    pub quota_window: Option<String>,
    pub enabled: bool,
    /// Extra regular expressions that mark a refusal, in addition to the
    /// built-in ones in `aura_adapters`.
    pub limit_patterns: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Roles {
    pub planner: Vec<String>,
    pub worker: Vec<String>,
    pub reviewer: Vec<String>,
    /// Let a reviewer use the worker's provider when no other provider is
    /// available. Off by default so the same model does not review its own code.
    pub allow_self_review: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Budgets {
    pub per_task_runs: u32,
    pub run_timeout_minutes: u64,
    pub idle_timeout_minutes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Acceptance {
    /// Programs that acceptance commands may start. Commands run without a shell.
    pub allowed_programs: Vec<String>,
    /// Commands added to every task's acceptance list.
    pub default_checks: Vec<String>,
}

/// The local web interface served by the daemon.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    /// Must be a loopback address. The UI is not meant for other machines.
    pub listen: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            enabled: true,
            listen: "127.0.0.1:7878".into(),
        }
    }
}

impl Default for RepoConfig {
    fn default() -> Self {
        RepoConfig {
            path: PathBuf::from("."),
            main_branch: "main".into(),
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            autonomy: Autonomy::AskPlan,
            max_parallel_runs: 3,
        }
    }
}

impl Default for Roles {
    fn default() -> Self {
        Roles {
            planner: vec!["claude-code".into()],
            worker: vec!["claude-code".into(), "codex".into()],
            reviewer: vec!["codex".into(), "claude-code".into()],
            allow_self_review: false,
        }
    }
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            per_task_runs: 3,
            run_timeout_minutes: 60,
            idle_timeout_minutes: 10,
        }
    }
}

impl Default for Acceptance {
    fn default() -> Self {
        Acceptance {
            allowed_programs: vec!["cargo".into(), "true".into()],
            default_checks: vec![],
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            command: String::new(),
            args: vec![],
            max_parallel: 1,
            quota_window: None,
            enabled: true,
            limit_patterns: vec![],
            env: BTreeMap::new(),
        }
    }
}

/// Providers that ship with working defaults. The argument lists track the
/// documented headless modes; they are the first thing to check against a
/// real install (see docs/implementation.md).
pub fn builtin_providers() -> BTreeMap<String, ProviderConfig> {
    let mut map = BTreeMap::new();
    map.insert(
        "claude-code".to_string(),
        ProviderConfig {
            command: "claude".into(),
            args: vec![
                "-p".into(),
                "{prompt}".into(),
                "--output-format".into(),
                "json".into(),
                "--permission-mode".into(),
                "acceptEdits".into(),
            ],
            max_parallel: 2,
            quota_window: Some("5h".into()),
            ..ProviderConfig::default()
        },
    );
    map.insert(
        "codex".to_string(),
        ProviderConfig {
            command: "codex".into(),
            args: vec![
                "exec".into(),
                "--sandbox".into(),
                "workspace-write".into(),
                "{prompt}".into(),
            ],
            max_parallel: 1,
            quota_window: Some("5h".into()),
            ..ProviderConfig::default()
        },
    );
    // Headless modes not yet confirmed. Disabled until a command is configured.
    for name in ["antigravity", "hermes"] {
        map.insert(
            name.to_string(),
            ProviderConfig {
                command: name.into(),
                enabled: false,
                ..ProviderConfig::default()
            },
        );
    }
    map
}

impl Default for Config {
    fn default() -> Self {
        Config {
            repo: RepoConfig::default(),
            defaults: Defaults::default(),
            providers: builtin_providers(),
            roles: Roles::default(),
            budgets: Budgets::default(),
            acceptance: Acceptance::default(),
            web: WebConfig::default(),
            state_dir: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    /// Location used when no path is given: `$AURA_CONFIG`, then
    /// `$XDG_CONFIG_HOME/aura/aura.toml`, then `~/.config/aura/aura.toml`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("AURA_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"));
        base.join("aura").join("aura.toml")
    }

    /// Reads and validates the file. A missing file at the default location
    /// means "use defaults"; a missing file at an explicit path is an error.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let (path, explicit) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => (Config::default_path(), false),
        };
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<Config>(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(source) if !explicit && source.kind() == std::io::ErrorKind::NotFound => {
                Config::default()
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        // Built-in providers stay available when the file only overrides some.
        for (name, provider) in builtin_providers() {
            config.providers.entry(name).or_insert(provider);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.repo.main_branch.trim().is_empty() {
            return Err(ConfigError::Invalid("repo.main_branch is empty".into()));
        }
        if self.web.enabled {
            let addr: std::net::SocketAddr = self.web.listen.parse().map_err(|_| {
                ConfigError::Invalid(format!(
                    "web.listen {:?} is not an address",
                    self.web.listen
                ))
            })?;
            if !addr.ip().is_loopback() {
                return Err(ConfigError::Invalid(format!(
                    "web.listen {} must be a loopback address",
                    self.web.listen
                )));
            }
        }
        if self.defaults.max_parallel_runs == 0 {
            return Err(ConfigError::Invalid(
                "defaults.max_parallel_runs must be at least 1".into(),
            ));
        }
        for (name, provider) in &self.providers {
            if provider.enabled && provider.command.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "provider {name} has no command"
                )));
            }
            if provider.enabled && provider.args.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "provider {name} is enabled but has no args; set args or enabled = false"
                )));
            }
            if let Some(window) = &provider.quota_window {
                if parse_duration(window).is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "provider {name}: quota_window {window:?} is not a duration like 5h"
                    )));
                }
            }
        }
        for (role, names) in [
            ("planner", &self.roles.planner),
            ("worker", &self.roles.worker),
            ("reviewer", &self.roles.reviewer),
        ] {
            if names.is_empty() {
                return Err(ConfigError::Invalid(format!("roles.{role} is empty")));
            }
            for name in names {
                match self.providers.get(name) {
                    None => {
                        return Err(ConfigError::Invalid(format!(
                            "roles.{role} names unknown provider {name}"
                        )))
                    }
                    Some(p) if !p.enabled => {
                        return Err(ConfigError::Invalid(format!(
                            "roles.{role} names disabled provider {name}"
                        )))
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    pub fn state_dir(&self) -> PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(|| home().join(".local").join("share").join("aura"))
    }

    pub fn paths(&self) -> Paths {
        Paths::new(self.state_dir())
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Parses durations such as `90s`, `30m`, `5h` or `2d`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits.parse().ok()?;
    let seconds = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

/// Every path the daemon and the CLI agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub db: PathBuf,
    pub socket: PathBuf,
    pub worktrees: PathBuf,
    pub scratch: PathBuf,
    pub merge: PathBuf,
    pub logs: PathBuf,
    /// Worktree of the `harness` branch that mirrors task and event state.
    pub mirror_dir: PathBuf,
}

impl Paths {
    pub fn new(state_dir: PathBuf) -> Paths {
        Paths {
            db: state_dir.join("aura.db"),
            socket: state_dir.join("aurad.sock"),
            worktrees: state_dir.join("worktrees"),
            scratch: state_dir.join("scratch"),
            merge: state_dir.join("merge"),
            logs: state_dir.join("runs"),
            mirror_dir: state_dir.join("mirror"),
            state_dir,
        }
    }

    pub fn mirror(&self) -> PathBuf {
        self.mirror_dir.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("5h"), Some(Duration::from_secs(18_000)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("5w"), None);
    }

    #[test]
    fn defaults_include_builtin_providers() {
        let c = Config::default();
        assert!(c.providers["claude-code"].enabled);
        assert!(!c.providers["antigravity"].enabled);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn partial_file_keeps_builtins_and_overrides_roles() {
        let text = r#"
            [repo]
            main_branch = "trunk"

            [roles]
            worker = ["codex"]
        "#;
        let mut c: Config = toml::from_str(text).unwrap();
        for (name, provider) in builtin_providers() {
            c.providers.entry(name).or_insert(provider);
        }
        assert_eq!(c.repo.main_branch, "trunk");
        assert_eq!(c.roles.worker, vec!["codex".to_string()]);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn enabled_provider_without_args_is_rejected() {
        let mut c = Config::default();
        c.providers.get_mut("codex").unwrap().args.clear();
        assert!(c.validate().is_err());
    }

    #[test]
    fn role_naming_disabled_provider_is_rejected() {
        let mut c = Config::default();
        c.roles.worker = vec!["hermes".into()];
        assert!(c.validate().unwrap_err().to_string().contains("disabled"));
    }
}
