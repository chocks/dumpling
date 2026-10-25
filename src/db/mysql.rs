use crate::error::{DumplingError, Result};
use crate::safety::redact_url;
use sqlx::MySqlPool;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use std::str::FromStr;
use std::time::Duration;

pub async fn connect(url: &str, label: &str) -> Result<MySqlPool> {
    let opts = MySqlConnectOptions::from_str(url).map_err(|e| {
        DumplingError::Database(format!(
            "invalid {label} connection URL ({}): {e}",
            redact_url(url)
        ))
    })?;

    MySqlPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(15))
        .connect_with(opts)
        .await
        .map_err(|e| {
            DumplingError::Database(format!(
                "failed to connect to {label} ({}): {e}",
                redact_url(url)
            ))
        })
}

pub async fn ping(pool: &MySqlPool) -> Result<()> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map_err(|e| DumplingError::Database(format!("ping failed: {e}")))?;
    Ok(())
}

pub async fn current_database(pool: &MySqlPool) -> Result<String> {
    let (name,): (Option<String>,) = sqlx::query_as("SELECT DATABASE()")
        .fetch_one(pool)
        .await
        .map_err(|e| DumplingError::Database(format!("DATABASE() failed: {e}")))?;
    name.ok_or_else(|| {
        DumplingError::Database("connection has no default database selected".into())
    })
}
