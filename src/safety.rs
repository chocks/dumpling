use crate::config::{Connection, ResolvedConfig};
use crate::error::{DumplingError, Result};
use url::Url;

/// Classification of a database endpoint for write-safety decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationClass {
    /// Clearly local / development (loopback, documented local hosts, or explicit allow).
    LocalDev,
    /// Ambiguous host — refuse writes under safe_destination unless explicitly allowed.
    Ambiguous,
    /// Looks like production — writes always refused.
    Production,
}

/// Evaluate whether Dumpling may write to the resolved destination.
///
/// `--force` never overrides `production: true` or a Production classification
/// when `safe_destination` is enabled. Force only unlocks destructive modes
/// (e.g. replace) on destinations that are already write-allowed.
pub fn assert_destination_writable(resolved: &ResolvedConfig) -> Result<()> {
    let dest = resolved
        .config
        .connections
        .get(&resolved.dest_name)
        .ok_or_else(|| {
            DumplingError::Safety(format!(
                "destination connection `{}` not found",
                resolved.dest_name
            ))
        })?;

    if dest.production {
        return Err(DumplingError::Safety(format!(
            "refusing to write to `{}`: connection is marked production: true \
             (--force cannot override this)",
            resolved.dest_name
        )));
    }

    let class = classify_destination(&dest.url, dest)?;

    if class == DestinationClass::Production {
        return Err(DumplingError::Safety(format!(
            "refusing to write to `{}`: host looks like production ({})",
            resolved.dest_name,
            redacted_host(&dest.url)
        )));
    }

    if resolved.config.defaults.safe_destination {
        if !dest.allow_destination {
            return Err(DumplingError::Safety(format!(
                "refusing to write to `{}`: set allow_destination: true on this \
                 connection after confirming it is a development database \
                 (safe_destination is enabled)",
                resolved.dest_name
            )));
        }
        if class == DestinationClass::Ambiguous && !dest.allow_destination {
            return Err(DumplingError::Safety(format!(
                "refusing ambiguous destination `{}` ({})",
                resolved.dest_name,
                redacted_host(&dest.url)
            )));
        }
    }

    // Destructive replace requires --force even on allowed destinations.
    if matches!(resolved.mode, crate::config::DestinationMode::Replace)
        && !resolved.force
        && !resolved.dry_run
    {
        return Err(DumplingError::Safety(
            "mode: replace is destructive; re-run with --force after confirming \
             the destination is disposable (CI: set DUMPLING_ALLOW_REPLACE=1 \
             together with --force)"
                .into(),
        ));
    }

    if matches!(resolved.mode, crate::config::DestinationMode::Replace)
        && resolved.force
        && !resolved.dry_run
    {
        // Noninteractive second confirmation via env for CI.
        let allow = std::env::var("DUMPLING_ALLOW_REPLACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !allow && !is_interactive() {
            return Err(DumplingError::Safety(
                "replace with --force in noninteractive mode also requires \
                 DUMPLING_ALLOW_REPLACE=1"
                    .into(),
            ));
        }
    }

    Ok(())
}

fn is_interactive() -> bool {
    console::user_attended()
}

/// Host/path classification used by destination safety checks.
///
/// Heuristics alone are never enough: connections must also set
/// `allow_destination: true`, and `production: true` always wins.
pub fn classify_destination(url_str: &str, conn: &Connection) -> Result<DestinationClass> {
    if conn.production {
        return Ok(DestinationClass::Production);
    }

    let url = Url::parse(url_str)
        .map_err(|e| DumplingError::Safety(format!("cannot parse destination URL: {e}")))?;

    let host = url.host_str().unwrap_or("").to_ascii_lowercase();

    // Unix socket paths are treated as local.
    if url_str.contains("socket=") || host.is_empty() {
        return Ok(DestinationClass::LocalDev);
    }

    if is_loopback_host(&host) {
        return Ok(DestinationClass::LocalDev);
    }

    // Well-known local / compose service names (still require allow_destination).
    const LOCAL_NAMES: &[&str] = &[
        "localhost",
        "mysql",
        "mariadb",
        "db",
        "database",
        "dumpling-mysql-source",
        "dumpling-mysql-dest",
        "host.docker.internal",
    ];
    if LOCAL_NAMES.contains(&host.as_str()) {
        return Ok(DestinationClass::LocalDev);
    }

    // Conservative production-like signals.
    let production_markers = [
        "prod",
        "production",
        "prd",
        "live",
        "primary",
        "aurora",
        "rds.amazonaws.com",
        "cloudsql",
        "database.azure.com",
        "googleapis.com",
    ];
    if production_markers.iter().any(|m| host.contains(m)) {
        return Ok(DestinationClass::Production);
    }

    Ok(DestinationClass::Ambiguous)
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "0.0.0.0"
        || host.starts_with("127.")
}

/// Return host only — never credentials — for error messages.
pub fn redacted_host(url_str: &str) -> String {
    Url::parse(url_str)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            Some(format!("{host}{port}"))
        })
        .unwrap_or_else(|| "<unparseable>".into())
}

/// Redact password from a URL for any diagnostic that must mention a URL shape.
pub fn redact_url(url_str: &str) -> String {
    match Url::parse(url_str) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("***"));
            }
            u.to_string()
        }
        Err(_) => "<unparseable-url>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DestinationMode};
    use indexmap::IndexMap;
    use std::path::PathBuf;

    fn resolved(
        dest_url: &str,
        allow: bool,
        production: bool,
        mode: DestinationMode,
        force: bool,
    ) -> ResolvedConfig {
        let mut connections = IndexMap::new();
        connections.insert(
            "src".into(),
            Connection {
                url: "mysql://u:p@prod.example.com/db".into(),
                allow_destination: false,
                production: true,
            },
        );
        connections.insert(
            "dst".into(),
            Connection {
                url: dest_url.into(),
                allow_destination: allow,
                production,
            },
        );
        let config = Config {
            version: 1,
            defaults: Default::default(),
            connections,
            source: Some("src".into()),
            destination: Some("dst".into()),
            mode,
            tables: IndexMap::new(),
            exclude: vec![],
            groups: IndexMap::new(),
            environments: IndexMap::new(),
        };
        ResolvedConfig {
            config,
            source_name: "src".into(),
            dest_name: "dst".into(),
            source_url: "mysql://u:p@prod.example.com/db".into(),
            dest_url: dest_url.into(),
            mode,
            table_filter: None,
            group: None,
            force,
            dry_run: false,
            config_path: PathBuf::from(".dumpling.yml"),
        }
    }

    #[test]
    fn refuses_production_flag_even_with_force() {
        let r = resolved(
            "mysql://u:p@127.0.0.1:3306/db",
            true,
            true,
            DestinationMode::Merge,
            true,
        );
        let err = assert_destination_writable(&r).unwrap_err();
        assert!(err.to_string().contains("production"));
    }

    #[test]
    fn refuses_without_allow_destination() {
        let r = resolved(
            "mysql://u:p@127.0.0.1:3306/db",
            false,
            false,
            DestinationMode::Merge,
            false,
        );
        let err = assert_destination_writable(&r).unwrap_err();
        assert!(err.to_string().contains("allow_destination"));
    }

    #[test]
    fn allows_local_with_opt_in() {
        let r = resolved(
            "mysql://u:p@127.0.0.1:3307/db",
            true,
            false,
            DestinationMode::Merge,
            false,
        );
        assert_destination_writable(&r).unwrap();
    }

    #[test]
    fn force_does_not_bypass_missing_allow() {
        let r = resolved(
            "mysql://u:p@127.0.0.1:3307/db",
            false,
            false,
            DestinationMode::Merge,
            true,
        );
        assert!(assert_destination_writable(&r).is_err());
    }

    #[test]
    fn replace_requires_force() {
        let r = resolved(
            "mysql://u:p@127.0.0.1:3307/db",
            true,
            false,
            DestinationMode::Replace,
            false,
        );
        let err = assert_destination_writable(&r).unwrap_err();
        assert!(err.to_string().contains("replace"));
    }

    #[test]
    fn classifies_rds_as_production() {
        let conn = Connection {
            url: "mysql://u:p@mydb.abcd.us-east-1.rds.amazonaws.com/app".into(),
            allow_destination: true,
            production: false,
        };
        assert_eq!(
            classify_destination(&conn.url, &conn).unwrap(),
            DestinationClass::Production
        );
    }

    #[test]
    fn redacts_password() {
        let s = redact_url("mysql://user:s3cret@localhost:3306/db");
        assert!(!s.contains("s3cret"));
        assert!(s.contains("***"));
    }
}
