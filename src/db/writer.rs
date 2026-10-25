//! Destination writes.
//!
//! All writes take a connection that is inside a transaction owned by the
//! caller, so a failed run rolls back (InnoDB only — DDL and non-transactional
//! engines cannot be rolled back; see docs/ARCHITECTURE.md).

use crate::db::introspect::TableInfo;
use crate::db::value::quote_ident;
use crate::error::{DumplingError, Result};
use crate::transform::TransformedValue;
use sqlx::{AssertSqlSafe, MySqlConnection};

/// MySQL allows at most 65,535 placeholders per prepared statement.
const MAX_PLACEHOLDERS: usize = 60_000;

pub fn validate_ident(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return Err(DumplingError::Validation(format!(
            "invalid SQL identifier `{name}`"
        )));
    }
    Ok(())
}

/// `DELETE` (not `TRUNCATE`: it is transactional and FK-friendly).
pub async fn delete_all(conn: &mut MySqlConnection, table: &str) -> Result<()> {
    validate_ident(table)?;
    sqlx::query(AssertSqlSafe(format!("DELETE FROM {}", quote_ident(table))))
        .execute(conn)
        .await
        .map_err(|e| {
            DumplingError::Database(format!(
                "clearing `{table}` failed (rows in tables outside this sync may reference it): {e}"
            ))
        })?;
    Ok(())
}

/// `INSERT` for replace mode (collisions must fail loudly), or
/// `INSERT .. ON DUPLICATE KEY UPDATE` for merge mode.
pub fn build_insert_sql(table: &TableInfo, columns: &[&str], rows: usize, upsert: bool) -> String {
    let cols = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let one = format!("({})", vec!["?"; columns.len()].join(", "));
    let values = vec![one; rows].join(", ");
    let name = quote_ident(&table.name);
    if !upsert {
        return format!("INSERT INTO {name} ({cols}) VALUES {values}");
    }
    let updates: Vec<String> = columns
        .iter()
        .filter(|c| !table.primary_key.iter().any(|pk| pk == *c))
        .map(|c| format!("{0}=VALUES({0})", quote_ident(c)))
        .collect();
    if updates.is_empty() {
        format!("INSERT IGNORE INTO {name} ({cols}) VALUES {values}")
    } else {
        format!(
            "INSERT INTO {name} ({cols}) VALUES {values} ON DUPLICATE KEY UPDATE {}",
            updates.join(", ")
        )
    }
}

pub async fn insert_rows(
    conn: &mut MySqlConnection,
    table: &TableInfo,
    columns: &[&str],
    rows: &[Vec<TransformedValue>],
    upsert: bool,
) -> Result<()> {
    validate_ident(&table.name)?;
    for c in columns {
        validate_ident(c)?;
    }
    let per_stmt = (MAX_PLACEHOLDERS / columns.len().max(1)).max(1);
    for chunk in rows.chunks(per_stmt) {
        let sql = build_insert_sql(table, columns, chunk.len(), upsert);
        let mut q = sqlx::query(AssertSqlSafe(sql));
        for row in chunk {
            for v in row {
                q = match v {
                    TransformedValue::Null => q.bind(Option::<String>::None),
                    TransformedValue::Bool(b) => q.bind(*b),
                    TransformedValue::Int(i) => q.bind(*i),
                    TransformedValue::UInt(u) => q.bind(*u),
                    TransformedValue::Float(f) => q.bind(*f),
                    TransformedValue::Text(s) => q.bind(s.as_str()),
                    TransformedValue::Bytes(b) => q.bind(b.as_slice()),
                };
            }
        }
        // MySQL's message may quote the offending value (e.g. duplicate-entry
        // errors), which would leak source data; report the code only.
        q.execute(&mut *conn).await.map_err(|e| {
            DumplingError::Database(format!(
                "insert into `{}` failed: {}",
                table.name,
                redact_db_error(&e)
            ))
        })?;
    }
    Ok(())
}

/// MySQL error text can embed row values ("Duplicate entry 'x' for key").
/// Report only the error code; the full message is logged at debug level.
pub fn redact_db_error(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(d) => {
            tracing::debug!("full database error: {d}");
            format!(
                "MySQL error {} (message withheld, it may contain row data; use -vv to see it)",
                d.code().as_deref().unwrap_or("?")
            )
        }
        other => other.to_string(),
    }
}
