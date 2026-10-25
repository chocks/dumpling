use crate::db::MySqlPool;
use crate::error::{DumplingError, Result};
use indexmap::IndexMap;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct SchemaSnapshot {
    pub database: String,
    pub tables: IndexMap<String, TableInfo>,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub engine: Option<String>,
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Vec<String>,
    pub unique_indexes: Vec<Vec<String>>,
    pub foreign_keys: Vec<ForeignKey>,
    pub auto_increment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub column_type: String,
    pub is_nullable: bool,
    pub column_default: Option<String>,
    pub extra: String,
    pub ordinal: u32,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub constraint_name: String,
    pub column_name: String,
    pub referenced_table: String,
    pub referenced_column: String,
}

pub async fn introspect(pool: &MySqlPool) -> Result<SchemaSnapshot> {
    let database = crate::db::mysql::current_database(pool).await?;
    let mut tables = IndexMap::new();

    let table_rows = sqlx::query(
        r#"
        SELECT TABLE_NAME, ENGINE
        FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE'
        ORDER BY TABLE_NAME
        "#,
    )
    .bind(&database)
    .fetch_all(pool)
    .await
    .map_err(|e| DumplingError::Database(format!("list tables failed: {e}")))?;

    for row in table_rows {
        let name: String = row.try_get("TABLE_NAME").map_err(db_err)?;
        let engine: Option<String> = row.try_get("ENGINE").ok();
        tables.insert(
            name.clone(),
            TableInfo {
                name,
                engine,
                columns: Vec::new(),
                primary_key: Vec::new(),
                unique_indexes: Vec::new(),
                foreign_keys: Vec::new(),
                auto_increment: None,
            },
        );
    }

    let col_rows = sqlx::query(
        r#"
        SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE,
               COLUMN_DEFAULT, EXTRA, ORDINAL_POSITION
        FROM information_schema.COLUMNS
        WHERE TABLE_SCHEMA = ?
        ORDER BY TABLE_NAME, ORDINAL_POSITION
        "#,
    )
    .bind(&database)
    .fetch_all(pool)
    .await
    .map_err(|e| DumplingError::Database(format!("list columns failed: {e}")))?;

    for row in col_rows {
        let table: String = row.try_get("TABLE_NAME").map_err(db_err)?;
        let Some(info) = tables.get_mut(&table) else {
            continue;
        };
        let col = ColumnInfo {
            name: row.try_get("COLUMN_NAME").map_err(db_err)?,
            data_type: row.try_get("DATA_TYPE").map_err(db_err)?,
            column_type: row.try_get("COLUMN_TYPE").map_err(db_err)?,
            is_nullable: {
                let s: String = row.try_get("IS_NULLABLE").map_err(db_err)?;
                s.eq_ignore_ascii_case("YES")
            },
            column_default: row.try_get("COLUMN_DEFAULT").ok(),
            extra: row
                .try_get::<String, _>("EXTRA")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            ordinal: row.try_get::<u32, _>("ORDINAL_POSITION").unwrap_or(0),
        };
        if col.extra.contains("auto_increment") {
            info.auto_increment = Some(col.name.clone());
        }
        info.columns.push(col);
    }

    let pk_rows = sqlx::query(
        r#"
        SELECT TABLE_NAME, COLUMN_NAME, SEQ_IN_INDEX
        FROM information_schema.STATISTICS
        WHERE TABLE_SCHEMA = ? AND INDEX_NAME = 'PRIMARY'
        ORDER BY TABLE_NAME, SEQ_IN_INDEX
        "#,
    )
    .bind(&database)
    .fetch_all(pool)
    .await
    .map_err(|e| DumplingError::Database(format!("list primary keys failed: {e}")))?;

    for row in pk_rows {
        let table: String = row.try_get("TABLE_NAME").map_err(db_err)?;
        let col: String = row.try_get("COLUMN_NAME").map_err(db_err)?;
        if let Some(info) = tables.get_mut(&table) {
            info.primary_key.push(col);
        }
    }

    let uniq_rows = sqlx::query(
        r#"
        SELECT TABLE_NAME, INDEX_NAME, COLUMN_NAME, SEQ_IN_INDEX
        FROM information_schema.STATISTICS
        WHERE TABLE_SCHEMA = ? AND NON_UNIQUE = 0 AND INDEX_NAME != 'PRIMARY'
        ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX
        "#,
    )
    .bind(&database)
    .fetch_all(pool)
    .await
    .map_err(|e| DumplingError::Database(format!("list unique indexes failed: {e}")))?;

    let mut current: Option<(String, String, Vec<String>)> = None;
    for row in uniq_rows {
        let table: String = row.try_get("TABLE_NAME").map_err(db_err)?;
        let index: String = row.try_get("INDEX_NAME").map_err(db_err)?;
        let col: String = row.try_get("COLUMN_NAME").map_err(db_err)?;
        match &mut current {
            Some((t, i, cols)) if *t == table && *i == index => cols.push(col),
            _ => {
                if let Some((t, _, cols)) = current.take()
                    && let Some(info) = tables.get_mut(&t)
                {
                    info.unique_indexes.push(cols);
                }
                current = Some((table, index, vec![col]));
            }
        }
    }
    if let Some((t, _, cols)) = current
        && let Some(info) = tables.get_mut(&t)
    {
        info.unique_indexes.push(cols);
    }

    let fk_rows = sqlx::query(
        r#"
        SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME,
               REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
        FROM information_schema.KEY_COLUMN_USAGE
        WHERE TABLE_SCHEMA = ?
          AND REFERENCED_TABLE_NAME IS NOT NULL
        ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION
        "#,
    )
    .bind(&database)
    .fetch_all(pool)
    .await
    .map_err(|e| DumplingError::Database(format!("list foreign keys failed: {e}")))?;

    for row in fk_rows {
        let table: String = row.try_get("TABLE_NAME").map_err(db_err)?;
        let fk = ForeignKey {
            constraint_name: row.try_get("CONSTRAINT_NAME").map_err(db_err)?,
            column_name: row.try_get("COLUMN_NAME").map_err(db_err)?,
            referenced_table: row.try_get("REFERENCED_TABLE_NAME").map_err(db_err)?,
            referenced_column: row.try_get("REFERENCED_COLUMN_NAME").map_err(db_err)?,
        };
        if let Some(info) = tables.get_mut(&table) {
            info.foreign_keys.push(fk);
        }
    }

    Ok(SchemaSnapshot { database, tables })
}

fn db_err(e: sqlx::Error) -> DumplingError {
    DumplingError::Database(e.to_string())
}
