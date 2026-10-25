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
