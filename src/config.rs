use crate::error::{DumplingError, Result};
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_CONFIG_FILE: &str = ".dumpling.yml";
pub const EXAMPLE_CONFIG_FILE: &str = ".dumpling.example.yml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub defaults: Defaults,
    pub connections: IndexMap<String, Connection>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub mode: DestinationMode,
    #[serde(default)]
    pub tables: IndexMap<String, TableConfig>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub groups: IndexMap<String, Vec<String>>,
    /// Named environments map CLI `--env` to source/destination connection names.
    #[serde(default)]
    pub environments: IndexMap<String, Environment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_jobs")]
    pub jobs: usize,
    #[serde(default = "default_true")]
    pub anonymize: bool,
    #[serde(default = "default_true")]
    pub safe_destination: bool,
    #[serde(default = "default_true")]
    pub preserve_auto_increment: bool,
    /// Secret used for deterministic keyed transforms (HMAC). Prefer env var.
    #[serde(default)]
    pub transform_secret: Option<String>,
    /// When true, identical source values map to identical replacements.
    #[serde(default = "default_true")]
    pub deterministic: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            batch_size: default_batch_size(),
            jobs: default_jobs(),
            anonymize: true,
            safe_destination: true,
            preserve_auto_increment: true,
            transform_secret: None,
            deterministic: true,
        }
    }
}

fn default_batch_size() -> usize {
    2000
}
fn default_jobs() -> usize {
    4
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub url: String,
    /// Explicitly mark this connection as a development/local destination.
    #[serde(default)]
    pub allow_destination: bool,
    /// Explicitly mark this connection as production (writes always refused).
    #[serde(default)]
    pub production: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DestinationMode {
    /// Insert or update selected rows by primary key.
    #[default]
    Merge,
    /// Truncate (or delete) selected destination tables, then insert.
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct TableConfig {
    #[serde(default)]
    pub where_clause: Option<String>,
    /// Alias accepted in YAML as `where` (SQL keyword).
    #[serde(default, rename = "where")]
    pub r#where: Option<String>,
    #[serde(default)]
    pub columns: IndexMap<String, ColumnConfig>,
    #[serde(default)]
    pub defer: bool,
}

impl TableConfig {
    pub fn filter(&self) -> Option<&str> {
        self.r#where
            .as_deref()
            .or(self.where_clause.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ColumnConfig {
    pub transform: TransformSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TransformSpec {
    Named(String),
    Detailed {
        #[serde(rename = "transform")]
        name: String,
        #[serde(default)]
        value: Option<serde_yaml::Value>,
        #[serde(default)]
        min: Option<String>,
        #[serde(default)]
        max: Option<String>,
    },
}

impl TransformSpec {
    pub fn name(&self) -> &str {
        match self {
            Self::Named(n) => n,
            Self::Detailed { name, .. } => name,
        }
    }

    pub fn constant_value(&self) -> Option<&serde_yaml::Value> {
        match self {
            Self::Detailed { value, .. } => value.as_ref(),
            Self::Named(_) => None,
        }
    }
}

/// Resolved runtime settings after applying CLI overrides.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config: Config,
    pub source_name: String,
    pub dest_name: String,
    pub source_url: String,
    pub dest_url: String,
    pub mode: DestinationMode,
    pub table_filter: Option<Vec<String>>,
    pub group: Option<String>,
    pub force: bool,
    pub dry_run: bool,
    pub config_path: PathBuf,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|e| {
            DumplingError::Config(format!("failed to read config {}: {e}", path.display()))
        })?;
        Self::parse_yaml(&raw)
    }

    pub fn parse_yaml(raw: &str) -> Result<Self> {
        let interpolated = interpolate_env(raw)?;
        let config: Config = serde_yaml::from_str(&interpolated)
            .map_err(|e| DumplingError::Config(format!("invalid YAML configuration: {e}")))?;
        config.validate_structure()?;
        Ok(config)
    }

    pub fn validate_structure(&self) -> Result<()> {
        if self.version != 1 {
            return Err(DumplingError::Config(format!(
                "unsupported config version {}; expected 1",
                self.version
            )));
        }
        if self.connections.is_empty() {
            return Err(DumplingError::Config(
                "at least one connection is required".into(),
            ));
        }
        for (name, conn) in &self.connections {
            validate_mysql_url(&conn.url)
                .map_err(|e| DumplingError::Config(format!("connection `{name}`: {e}")))?;
        }
        if let Some(src) = &self.source
            && !self.connections.contains_key(src)
        {
            return Err(DumplingError::Config(format!(
                "source connection `{src}` is not defined"
            )));
        }
        if let Some(dst) = &self.destination
            && !self.connections.contains_key(dst)
        {
            return Err(DumplingError::Config(format!(
                "destination connection `{dst}` is not defined"
            )));
        }
        for (env_name, env) in &self.environments {
            if !self.connections.contains_key(&env.source) {
                return Err(DumplingError::Config(format!(
                    "environment `{env_name}` references unknown source `{}`",
                    env.source
                )));
            }
            if !self.connections.contains_key(&env.destination) {
                return Err(DumplingError::Config(format!(
                    "environment `{env_name}` references unknown destination `{}`",
                    env.destination
                )));
            }
        }
        for (group, tables) in &self.groups {
            for table in tables {
                if !self.tables.contains_key(table) && !self.exclude.iter().any(|e| e == table) {
                    // Groups may reference tables that use defaults (no explicit entry).
                    // Only warn via validation if they appear in exclude.
                    let _ = group;
                    let _ = table;
                }
            }
        }
        if self.defaults.batch_size == 0 {
            return Err(DumplingError::Config(
                "defaults.batch_size must be >= 1".into(),
            ));
        }
        if self.defaults.jobs == 0 {
            return Err(DumplingError::Config("defaults.jobs must be >= 1".into()));
        }
        Ok(())
    }

    pub fn resolve(
        mut self,
        config_path: PathBuf,
        env: Option<&str>,
        tables: Option<Vec<String>>,
        group: Option<String>,
        force: bool,
        dry_run: bool,
    ) -> Result<ResolvedConfig> {
        let (source_name, dest_name) = if let Some(env_name) = env {
            let environment = self.environments.get(env_name).ok_or_else(|| {
                DumplingError::Config(format!(
                    "environment `{env_name}` not found; defined: {}",
                    self.environments
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            (environment.source.clone(), environment.destination.clone())
        } else {
            let source = self
                .source
                .clone()
                .or_else(|| self.connections.keys().next().cloned())
                .ok_or_else(|| DumplingError::Config("no source connection configured".into()))?;
            let dest = self.destination.clone().ok_or_else(|| {
                DumplingError::Config(
                    "destination connection is required (set `destination:` or use --env)".into(),
                )
            })?;
            (source, dest)
        };

        if source_name == dest_name {
            return Err(DumplingError::Config(
                "source and destination connections must be different".into(),
            ));
        }

        let source_url = self
            .connections
            .get(&source_name)
            .ok_or_else(|| {
                DumplingError::Config(format!("source connection `{source_name}` not found"))
            })?
            .url
            .clone();
        let dest_url = self
            .connections
            .get(&dest_name)
            .ok_or_else(|| {
                DumplingError::Config(format!("destination connection `{dest_name}` not found"))
            })?
            .url
            .clone();

        if let Some(ref g) = group
            && !self.groups.contains_key(g)
        {
            return Err(DumplingError::Config(format!(
                "group `{g}` not found; defined: {}",
                self.groups.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }

        // Resolve transform secret from env if not in config.
        if self.defaults.transform_secret.is_none()
            && let Ok(secret) = env::var("DUMPLING_TRANSFORM_SECRET")
        {
            self.defaults.transform_secret = Some(secret);
        }

        let mode = self.mode;
        Ok(ResolvedConfig {
            config: self,
            source_name,
            dest_name,
            source_url,
            dest_url,
            mode,
            table_filter: tables,
            group,
            force,
            dry_run,
            config_path,
        })
    }
}

fn env_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("env interpolation regex")
    })
}

/// Replace `${VAR}` placeholders. Missing variables are errors (never silently empty).
pub fn interpolate_env(input: &str) -> Result<String> {
    let mut missing = Vec::new();
    let mut cache: HashMap<String, String> = HashMap::new();
    let out = env_pattern()
        .replace_all(input, |caps: &regex::Captures| {
            let key = &caps[1];
            if let Some(v) = cache.get(key) {
                return v.clone();
            }
            match env::var(key) {
                Ok(v) => {
                    cache.insert(key.to_string(), v.clone());
                    v
                }
                Err(_) => {
                    missing.push(key.to_string());
                    caps[0].to_string()
                }
            }
        })
        .into_owned();
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(DumplingError::Config(format!(
            "missing environment variable(s): {}",
            missing.join(", ")
        )));
    }
    Ok(out)
}

pub fn validate_mysql_url(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        return Err(DumplingError::Config("database URL is empty".into()));
    }
    let parsed = url::Url::parse(url)
        .map_err(|e| DumplingError::Config(format!("malformed database URL: {e}")))?;
    match parsed.scheme() {
        "mysql" | "mariadb" => {}
        other => {
            return Err(DumplingError::Config(format!(
                "unsupported URL scheme `{other}`; expected mysql:// or mariadb://"
            )));
        }
    }
    if parsed.host_str().is_none() && parsed.path() != "localhost" {
        // mysql://user:pass@localhost/db and socket URLs with host are fine;
        // empty host without socket is not.
        if !url.contains("socket=") && parsed.host().is_none() {
            return Err(DumplingError::Config(
                "database URL must include a host or socket".into(),
            ));
        }
    }
    Ok(())
}

pub fn find_config(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(DumplingError::Config(format!(
            "config file not found: {}",
            p.display()
        )));
    }
    let default = PathBuf::from(DEFAULT_CONFIG_FILE);
    if default.exists() {
        return Ok(default);
    }
    Err(DumplingError::Config(format!(
        "no `{DEFAULT_CONFIG_FILE}` found; run `dumpling init` first"
    )))
}

pub fn example_config_yaml() -> &'static str {
    include_str!("../.dumpling.example.yml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn interpolate_requires_env() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { env::remove_var("DUMPLING_TEST_MISSING") };
        let err = interpolate_env("url: ${DUMPLING_TEST_MISSING}").unwrap_err();
        assert!(err.to_string().contains("DUMPLING_TEST_MISSING"));
    }

    #[test]
    fn interpolate_substitutes() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { env::set_var("DUMPLING_TEST_URL", "mysql://u:p@localhost/db") };
        let out = interpolate_env("url: ${DUMPLING_TEST_URL}").unwrap();
        assert!(out.contains("mysql://u:p@localhost/db"));
        unsafe { env::remove_var("DUMPLING_TEST_URL") };
    }

    #[test]
    fn rejects_unknown_keys() {
        let yaml = r#"
version: 1
connections:
  local:
    url: mysql://root@localhost/db
    mystery: true
"#;
        let err = Config::parse_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("invalid YAML") || err.to_string().contains("mystery"));
    }

    #[test]
    fn parses_table_where_alias() {
        let yaml = r#"
version: 1
connections:
  production:
    url: mysql://root@127.0.0.1:3306/src
  local:
    url: mysql://root@127.0.0.1:3307/dst
    allow_destination: true
source: production
destination: local
tables:
  users:
    where: "status != 'deleted'"
    columns:
      email:
        transform: unique_email
"#;
        let cfg = Config::parse_yaml(yaml).unwrap();
        assert_eq!(cfg.tables["users"].filter(), Some("status != 'deleted'"));
        assert_eq!(
            cfg.tables["users"].columns["email"].transform.name(),
            "unique_email"
        );
    }
}
