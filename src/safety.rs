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

    if resolved.config.defaults.safe_destination && !dest.allow_destination {
        return Err(DumplingError::Safety(format!(
            "refusing to write to `{}`: set allow_destination: true on this \
             connection after confirming it is a development database \
             (safe_destination is enabled)",
            resolved.dest_name
        )));
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
        if !allow {
            if !is_interactive() {
                return Err(DumplingError::Safety(
                    "replace with --force in noninteractive mode also requires \
                     DUMPLING_ALLOW_REPLACE=1"
                        .into(),
                ));
            }
            confirm_replace(&redacted_host(&dest.url))?;
        }
    }

    Ok(())
}

/// Second confirmation for `replace --force` in a terminal: the user must
/// type the destination host.
fn confirm_replace(host: &str) -> Result<()> {
    let term = console::Term::stderr();
    let _ = term.write_line(&format!(
        "mode: replace will DELETE existing rows in the destination ({host})."
    ));
    let _ = term.write_str("Type the destination host to continue: ");
    let answer = term.read_line().unwrap_or_default();
    if answer.trim() == host {
        Ok(())
    } else {
        Err(DumplingError::Safety(
            "replace not confirmed; nothing was changed".into(),
        ))
    }
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

    if conn.not_production {
        return Ok(DestinationClass::Ambiguous);
    }

    if looks_like_production(&host) {
        return Ok(DestinationClass::Production);
    }

    Ok(DestinationClass::Ambiguous)
}

/// Hosts are split into words (`product-dev.example.com` is `product`, `dev`,
/// `example`, `com`) so `prod` does not match `product`. Unambiguous words
/// (`prod`, `prd`, `production`) always win; weaker ones (`live`, `primary`)
/// are overridden by an explicit dev-like word. Managed-cloud domains are
/// always production-like. Anything else is rejected as ambiguous unless the
/// connection sets `allow_destination`; a wrongly flagged host can be cleared
/// with `not_production: true`.
fn looks_like_production(host: &str) -> bool {
    const STRONG: &[&str] = &["prod", "production", "prd"];
    const WEAK: &[&str] = &["live", "primary", "aurora"];
    const DEV: &[&str] = &["dev", "development", "test", "qa", "local", "sandbox", "ci"];
    const CLOUD_DOMAINS: &[&str] = &[
        "rds.amazonaws.com",
        "cloudsql",
        "database.azure.com",
        "googleapis.com",
    ];
    let words: Vec<&str> = host
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |set: &[&str]| words.iter().any(|w| set.contains(w));
    has(STRONG) || CLOUD_DOMAINS.iter().any(|d| host.contains(d)) || (has(WEAK) && !has(DEV))
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
                not_production: false,
            },
        );
        connections.insert(
            "dst".into(),
            Connection {
                url: dest_url.into(),
                allow_destination: allow,
                production,
                not_production: false,
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
            not_production: false,
        };
        assert_eq!(
            classify_destination(&conn.url, &conn).unwrap(),
            DestinationClass::Production
        );
    }

    fn class_of(host: &str) -> DestinationClass {
        let conn = Connection {
            url: format!("mysql://u:p@{host}/app"),
            allow_destination: true,
            production: false,
            not_production: false,
        };
        classify_destination(&conn.url, &conn).unwrap()
    }

    #[test]
    fn matches_whole_words_not_substrings() {
        for h in [
            "delivery-db.dev.internal",
            "product-dev.example.com",
            "primary-replica.dev",
            "olive.example.com",
        ] {
            assert_eq!(class_of(h), DestinationClass::Ambiguous, "{h}");
        }
        for h in [
            "prod-db.example.com",
            "db.prod.internal",
            "app-prd-1.example.com",
            "primary.example.com",
            "live-db.example.com",
        ] {
            assert_eq!(class_of(h), DestinationClass::Production, "{h}");
        }
    }

    #[test]
    fn not_production_clears_a_false_positive() {
        let conn = Connection {
            url: "mysql://u:p@dev-db.abc.us-east-1.rds.amazonaws.com/app".into(),
            allow_destination: true,
            production: false,
            not_production: true,
        };
        assert_eq!(
            classify_destination(&conn.url, &conn).unwrap(),
            DestinationClass::Ambiguous
        );
    }

    #[test]
    fn redacts_password() {
        let s = redact_url("mysql://user:s3cret@localhost:3306/db");
        assert!(!s.contains("s3cret"));
        assert!(s.contains("***"));
    }
}
