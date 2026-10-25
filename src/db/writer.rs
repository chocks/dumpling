//! Destination write helpers (replace / merge).
//!
//! MySQL limitations:
//! - DDL and mixed storage engines can break transaction atomicity.
//! - `replace` truncates selected tables inside a best-effort transaction;
//!   InnoDB foreign keys may still require parent-before-child ordering.

use crate::db::MySqlPool;
use crate::db::introspect::TableInfo;
use crate::error::{DumplingError, Result};
use crate::transform::TransformedValue;
use sqlx::{AssertSqlSafe, MySql};

pub async fn truncate_table(pool: &MySqlPool, table: &str) -> Result<()> {
    validate_ident(table)?;
    // Identifier validated above; not user-controlled free-form SQL.
    let sql = format!("TRUNCATE TABLE `{}`", table.replace('`', "``"));
    sqlx::query(AssertSqlSafe(sql))
        .execute(pool)
        .await
        .map_err(|e| DumplingError::Database(format!("truncate `{table}` failed: {e}")))?;
    Ok(())
}

pub async fn delete_all(pool: &MySqlPool, table: &str) -> Result<()> {
    validate_ident(table)?;
    let sql = format!("DELETE FROM `{}`", table.replace('`', "``"));
    sqlx::query(AssertSqlSafe(sql))
        .execute(pool)
        .await
        .map_err(|e| DumplingError::Database(format!("delete `{table}` failed: {e}")))?;
    Ok(())
}

/// Build a batched INSERT ... ON DUPLICATE KEY UPDATE for merge mode.
pub fn build_upsert_sql(table: &TableInfo, columns: &[&str], batch_len: usize) -> Result<String> {
    if columns.is_empty() || batch_len == 0 {
        return Err(DumplingError::Database(
            "upsert requires columns and at least one row".into(),
        ));
    }
    validate_ident(&table.name)?;
    for c in columns {
        validate_ident(c)?;
    }

    let cols = columns
        .iter()
        .map(|c| format!("`{}`", c.replace('`', "``")))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders_one = format!(
        "({})",
        columns.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
    );
    let values = (0..batch_len)
        .map(|_| placeholders_one.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let updates = columns
        .iter()
        .filter(|c| !table.primary_key.iter().any(|pk| pk == *c))
        .map(|c| {
            let q = c.replace('`', "``");
            format!("`{q}`=VALUES(`{q}`)")
        })
        .collect::<Vec<_>>();

    let sql = if updates.is_empty() {
        format!(
            "INSERT IGNORE INTO `{}` ({cols}) VALUES {values}",
            table.name.replace('`', "``")
        )
    } else {
        format!(
            "INSERT INTO `{}` ({cols}) VALUES {values} ON DUPLICATE KEY UPDATE {}",
            table.name.replace('`', "``"),
            updates.join(", ")
        )
    };
    Ok(sql)
}

pub fn bind_value<'q>(
    query: sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments>,
    value: &'q TransformedValue,
) -> sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments> {
    match value {
        TransformedValue::Null => query.bind(Option::<String>::None),
        TransformedValue::Bool(b) => query.bind(*b),
        TransformedValue::Int(i) => query.bind(*i),
        TransformedValue::UInt(u) => query.bind(*u),
        TransformedValue::Float(f) => query.bind(*f),
        TransformedValue::Text(s) => query.bind(s.as_str()),
        TransformedValue::Bytes(b) => query.bind(b.as_slice()),
        TransformedValue::Preserve => query.bind(Option::<String>::None),
    }
}

pub fn validate_ident(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(DumplingError::Validation(format!(
            "invalid SQL identifier `{name}`"
        )));
    }
    Ok(())
}
