//! Lossless column transport between MySQL and Rust.
//!
//! Each source column is read in one of a few forms chosen from its declared
//! type: native integers/floats, text, bytes, or `CAST(.. AS CHAR)` text for
//! types where a Rust numeric/date type would be lossy (DECIMAL, temporal
//! types, JSON). Text is bound back as a string parameter and MySQL parses it
//! into the destination column type, so exactness is preserved.

use crate::db::introspect::ColumnInfo;
use crate::error::{DumplingError, Result};
use crate::transform::TransformedValue;
use sqlx::mysql::MySqlRow;
use sqlx::{Row, ValueRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    UInt,
    F32,
    F64,
    Text,
    Bytes,
}

pub fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// SELECT expression and decode kind for a column, or an error for types
/// Dumpling does not copy yet (spatial types).
pub fn read_plan(col: &ColumnInfo) -> Result<(String, Kind)> {
    let q = quote_ident(&col.name);
    let cast_text = format!("CAST({q} AS CHAR)");
    let unsigned = col.column_type.to_ascii_lowercase().contains("unsigned");
    Ok(match col.data_type.to_ascii_lowercase().as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" => {
            (q, if unsigned { Kind::UInt } else { Kind::Int })
        }
        "year" | "bit" => (format!("CAST({q} AS UNSIGNED)"), Kind::UInt),
        "float" => (q, Kind::F32),
        "double" => (q, Kind::F64),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" => (q, Kind::Text),
        "decimal" | "numeric" | "date" | "datetime" | "timestamp" | "time" | "json" | "enum"
        | "set" => (cast_text, Kind::Text),
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
            (q, Kind::Bytes)
        }
        other => {
            return Err(DumplingError::Validation(format!(
                "column `{}` has unsupported type `{other}`",
                col.name
            )));
        }
    })
}

/// How a column takes part in keyset paging: the SQL expression to order and
/// compare by, and the placeholder the previous page's last value is bound to.
///
/// The comparison must use the same ordering as `ORDER BY` and must not lose
/// precision, or pages can skip or repeat rows:
/// - ENUM/SET sort by index/bitmask, not by label, so they are compared as
///   numbers (`CAST(col+0 AS UNSIGNED)`; see [`key_reads_as_number`]).
/// - DECIMAL and temporal values are read as text; the placeholder casts it
///   back to the column's own type instead of letting MySQL compare it as a
///   double or a loosely parsed string.
pub fn key_compare(col: &ColumnInfo) -> (String, String) {
    let q = quote_ident(&col.name);
    if key_reads_as_number(col) {
        return (format!("CAST({q}+0 AS UNSIGNED)"), "?".into());
    }
    let ty = col.column_type.to_ascii_lowercase();
    let ty = ty.split_whitespace().next().unwrap_or("").to_string();
    let cast = match col.data_type.to_ascii_lowercase().as_str() {
        "decimal" | "numeric" => Some(ty.to_ascii_uppercase()),
        "date" => Some("DATE".to_string()),
        "datetime" | "timestamp" => {
            Some(ty.replacen("timestamp", "datetime", 1).to_ascii_uppercase())
        }
        "time" => Some(ty.to_ascii_uppercase()),
        _ => None,
    };
    match cast {
        Some(t) => (q, format!("CAST(? AS {t})")),
        None => (q, "?".into()),
    }
}

/// ENUM and SET keys are paged by their numeric value, not their label.
pub fn key_reads_as_number(col: &ColumnInfo) -> bool {
    matches!(col.data_type.to_ascii_lowercase().as_str(), "enum" | "set")
}

/// Expression that yields the column as text, for client-side transforms.
pub fn text_expr(col: &ColumnInfo) -> Result<String> {
    let (expr, kind) = read_plan(col)?;
    match kind {
        Kind::Bytes => Err(DumplingError::Validation(format!(
            "transforms are not supported on binary column `{}`",
            col.name
        ))),
        Kind::Text => Ok(expr),
        _ => Ok(format!("CAST({expr} AS CHAR)")),
    }
}

fn err(e: sqlx::Error) -> DumplingError {
    // sqlx decode errors describe types, not values.
    DumplingError::Database(format!("read column failed: {e}"))
}

pub fn read_cell(row: &MySqlRow, idx: usize, kind: Kind) -> Result<TransformedValue> {
    if row.try_get_raw(idx).map_err(err)?.is_null() {
        return Ok(TransformedValue::Null);
    }
    Ok(match kind {
        Kind::Int => TransformedValue::Int(row.try_get(idx).map_err(err)?),
        Kind::UInt => TransformedValue::UInt(row.try_get(idx).map_err(err)?),
        Kind::F32 => TransformedValue::Float(row.try_get::<f32, _>(idx).map_err(err)? as f64),
        Kind::F64 => TransformedValue::Float(row.try_get(idx).map_err(err)?),
        Kind::Text => TransformedValue::Text(row.try_get(idx).map_err(err)?),
        Kind::Bytes => TransformedValue::Bytes(row.try_get(idx).map_err(err)?),
    })
}
