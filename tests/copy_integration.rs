//! Phase 2: the copy engine against real MySQL (docker compose).

mod common;
use common::*;

const ALL_TYPES: &str = r#"
CREATE TABLE all_types (
  id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  c_tiny TINYINT, c_utiny TINYINT UNSIGNED, c_small SMALLINT, c_med MEDIUMINT UNSIGNED,
  c_int INT, c_big BIGINT, c_ubig BIGINT UNSIGNED,
  c_dec DECIMAL(30,10), c_float FLOAT, c_double DOUBLE,
  c_char CHAR(8), c_varchar VARCHAR(255), c_text TEXT, c_longtext LONGTEXT,
  c_bin BINARY(4), c_varbin VARBINARY(64), c_blob BLOB, c_longblob LONGBLOB,
  c_json JSON, c_date DATE, c_dt DATETIME(6), c_ts TIMESTAMP(3) NULL, c_time TIME(3),
  c_year YEAR, c_bit BIT(12), c_enum ENUM('a','b','c'), c_set SET('x','y','z'), c_bool BOOLEAN
) ENGINE=InnoDB;
"#;

const ALL_TYPES_SEED: &str = r#"
INSERT INTO all_types VALUES
 (1, -128, 255, -32768, 16777215, -2147483648, -9223372036854775808, 18446744073709551615,
  12345678901234567890.1234567891, 1.5, 3.141592653589793,
  'abc', 'héllo 🥟 wörld', 'some text', REPEAT('x', 100000),
  X'00FF10AB', X'000102030405FF', X'0000FFFF', X'DEADBEEF00BEEF',
  '{"a": [1, 2, {"b": null}], "unicode": "🥟"}', '2024-02-29', '2024-02-29 23:59:59.123456',
  '2024-02-29 23:59:59.123', '-838:59:59.000', 2155, b'101010101010', 'c', 'x,z', TRUE),
 (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
  NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
 (3, 0, 0, 0, 0, 0, 0, 0, 0.0000000001, 0, 0, '', '', '', '', X'00000000', X'', X'', X'',
  'null', '1000-01-01', '1000-01-01 00:00:00', '1970-01-01 00:00:01', '00:00:00', 1901,
  b'0', 'a', '', FALSE);
"#;

#[tokio::test]
async fn copies_every_mysql_type_without_loss() {
    let Some(fx) = Fixture::with_schema("types", ALL_TYPES, ALL_TYPES_SEED).await else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  all_types: {}\n");
    let summary = fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(summary.tables, vec![("all_types".to_string(), 3)]);
    assert_eq!(
        dump(&fx.src, "all_types", "id").await,
        dump(&fx.dst, "all_types", "id").await
    );
}

const FK_SCHEMA: &str = r#"
CREATE TABLE users (id BIGINT UNSIGNED PRIMARY KEY, status VARCHAR(16) NOT NULL) ENGINE=InnoDB;
CREATE TABLE orders (id BIGINT UNSIGNED PRIMARY KEY, user_id BIGINT UNSIGNED NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users (id)) ENGINE=InnoDB;
CREATE TABLE order_items (id BIGINT UNSIGNED PRIMARY KEY, order_id BIGINT UNSIGNED NOT NULL,
  FOREIGN KEY (order_id) REFERENCES orders (id)) ENGINE=InnoDB;
CREATE TABLE notes (id BIGINT UNSIGNED PRIMARY KEY, order_id BIGINT UNSIGNED NULL,
  FOREIGN KEY (order_id) REFERENCES orders (id)) ENGINE=InnoDB;
CREATE TABLE regions (country CHAR(2) NOT NULL, code CHAR(3) NOT NULL, active TINYINT NOT NULL,
  PRIMARY KEY (country, code)) ENGINE=InnoDB;
CREATE TABLE stores (id INT PRIMARY KEY, country CHAR(2) NOT NULL, code CHAR(3) NOT NULL,
  FOREIGN KEY (country, code) REFERENCES regions (country, code)) ENGINE=InnoDB;
"#;

const FK_SEED: &str = r#"
INSERT INTO users VALUES (1, 'active'), (2, 'deleted');
INSERT INTO orders VALUES (10, 1), (11, 2);
INSERT INTO order_items VALUES (100, 10), (101, 11);
INSERT INTO notes VALUES (1, 10), (2, 11), (3, NULL);
INSERT INTO regions VALUES ('US', 'CA', 1), ('US', 'NY', 0), ('DE', 'BE', 1);
INSERT INTO stores VALUES (1, 'US', 'CA'), (2, 'US', 'NY'), (3, 'DE', 'BE');
"#;

#[tokio::test]
async fn parent_filters_propagate_so_children_never_dangle() {
    let Some(fx) = Fixture::with_schema("fkfilter", FK_SCHEMA, FK_SEED).await else {
        return;
    };
    let yaml = fx.yaml(
        "  batch_size: 1",
        r#"
mode: merge
tables:
  users: { where: "status = 'active'" }
  orders: {}
  order_items: {}
  notes: {}
  regions: { where: "active = 1" }
  stores: {}
"#,
    );
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(ids(&fx.dst, "users").await, vec![1]);
    assert_eq!(ids(&fx.dst, "orders").await, vec![10]);
    assert_eq!(ids(&fx.dst, "order_items").await, vec![100]);
    // Nullable FK: NULL references are always valid and must be kept.
    assert_eq!(ids(&fx.dst, "notes").await, vec![1, 3]);
    // Composite FK.
    assert_eq!(ids(&fx.dst, "stores").await, vec![1, 3]);
}

const ITEMS: &str =
    "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT) ENGINE=InnoDB;";

#[tokio::test]
async fn replace_failure_rolls_back_destination() {
    let src = format!(
        "{ITEMS} INSERT INTO items VALUES (1,'a',1),(2,'b',2),(3,'c',3),(4,'d',4),(5,'e',500);"
    );
    let dst = "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT, CHECK (qty < 100)) \
               ENGINE=InnoDB; INSERT INTO items VALUES (100,'old',1),(101,'old',2);";
    let Some(fx) = Fixture::new("rollback", &src, dst).await else {
        return;
    };
    let yaml = fx.yaml("  batch_size: 2", "mode: replace\ntables:\n  items: {}\n");
    let err = fx.sync(&yaml, true, false).await.unwrap_err();
    assert!(err.to_string().contains("items"), "{err}");
    assert_eq!(
        ids(&fx.dst, "items").await,
        vec![100, 101],
        "must roll back"
    );
}

#[tokio::test]
async fn replace_requires_force() {
    let Some(fx) =
        Fixture::with_schema("noforce", ITEMS, "INSERT INTO items VALUES (1,'a',1);").await
    else {
        return;
    };
    sqlx::query("INSERT INTO items VALUES (9,'keep',9)")
        .execute(&fx.dst)
        .await
        .unwrap();
    let yaml = fx.yaml("", "mode: replace\ntables:\n  items: {}\n");
    assert!(fx.sync(&yaml, false, false).await.is_err());
    assert_eq!(ids(&fx.dst, "items").await, vec![9]);
}

#[tokio::test]
async fn merge_upserts_and_keeps_other_rows() {
    let Some(fx) = Fixture::with_schema(
        "merge",
        ITEMS,
        "INSERT INTO items VALUES (1,'new1',1),(2,'new2',2);",
    )
    .await
    else {
        return;
    };
    sqlx::query("INSERT INTO items VALUES (1,'old1',0),(99,'keep',9)")
        .execute(&fx.dst)
        .await
        .unwrap();
    let yaml = fx.yaml("", "mode: merge\ntables:\n  items: {}\n");
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(ids(&fx.dst, "items").await, vec![1, 2, 99]);
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM items WHERE id = 1 AND name = 'new1'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn auto_increment_continues_after_copy() {
    let schema =
        "CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(10)) ENGINE=InnoDB;";
    let Some(fx) = Fixture::with_schema(
        "autoinc",
        schema,
        "INSERT INTO t VALUES (1,'a'),(2,'b'),(500,'c');",
    )
    .await
    else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  t: {}\n");
    fx.sync(&yaml, false, false).await.unwrap();
    sqlx::query("INSERT INTO t (name) VALUES ('new')")
        .execute(&fx.dst)
        .await
        .unwrap();
    assert_eq!(ids(&fx.dst, "t").await, vec![1, 2, 500, 501]);
}

#[tokio::test]
async fn pages_large_tables_with_composite_keys() {
    let schema = "CREATE TABLE big (a INT NOT NULL, b INT NOT NULL, v VARCHAR(20), \
                  PRIMARY KEY (a, b)) ENGINE=InnoDB;";
    let seed = "SET SESSION cte_max_recursion_depth = 10000; \
                INSERT INTO big WITH RECURSIVE n AS (SELECT 1 i UNION ALL SELECT i+1 FROM n WHERE i < 5000) \
                SELECT i % 7, i, CONCAT('v', i) FROM n;";
    let Some(fx) = Fixture::with_schema("big", schema, seed).await else {
        return;
    };
    let yaml = fx.yaml("  batch_size: 300", "mode: merge\ntables:\n  big: {}\n");
    let s = fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(s.tables, vec![("big".to_string(), 5000)]);
    assert_eq!(
        dump(&fx.src, "big", "a, b").await,
        dump(&fx.dst, "big", "a, b").await
    );
}

#[tokio::test]
async fn wide_tables_stay_under_the_placeholder_limit() {
    let cols: Vec<String> = (0..150).map(|i| format!("c{i} INT")).collect();
    let schema = format!(
        "CREATE TABLE wide (id INT PRIMARY KEY, {}) ENGINE=InnoDB;",
        cols.join(", ")
    );
    let vals: Vec<String> = (0..150).map(|_| "i".to_string()).collect();
    let seed = format!(
        "INSERT INTO wide WITH RECURSIVE n AS (SELECT 1 i UNION ALL SELECT i+1 FROM n WHERE i < 700) \
         SELECT i, {} FROM n;",
        vals.join(", ")
    );
    let Some(fx) = Fixture::with_schema("wide", &schema, &seed).await else {
        return;
    };
    // 700 rows x 151 columns = 105,700 placeholders in one statement (> 65,535).
    let yaml = fx.yaml("  batch_size: 700", "mode: merge\ntables:\n  wide: {}\n");
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(count(&fx.dst, "SELECT COUNT(*) FROM wide").await, 700);
}

#[tokio::test]
async fn copies_only_shared_columns_and_warns() {
    let src = "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10), extra VARCHAR(10)) ENGINE=InnoDB; \
               INSERT INTO t VALUES (1,'a','secret');";
    let dst =
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10), only_dst INT NULL) ENGINE=InnoDB;";
    let Some(fx) = Fixture::new("shared", src, dst).await else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  t: {}\n");
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(ids(&fx.dst, "t").await, vec![1]);
}

#[tokio::test]
async fn rejects_missing_destination_table_before_writing() {
    let Some(fx) = Fixture::new(
        "nodsttable",
        &format!("{ITEMS} INSERT INTO items VALUES (1,'a',1);"),
        "CREATE TABLE other (id INT PRIMARY KEY);",
    )
    .await
    else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  items: {}\n");
    let err = fx.sync(&yaml, false, false).await.unwrap_err();
    assert!(err.to_string().contains("items"));
}

#[tokio::test]
async fn refuses_when_source_and_destination_are_the_same_database() {
    let Some(fx) = Fixture::with_schema("same", ITEMS, "INSERT INTO items VALUES (1,'a',1);").await
    else {
        return;
    };
    // Two differently named connections that point at the same database.
    let yaml = format!(
        r#"
version: 1
connections:
  a: {{ url: "{u}" }}
  b: {{ url: "{u}", allow_destination: true }}
source: a
destination: b
mode: replace
tables:
  items: {{}}
"#,
        u = fx.src_url
    );
    let err = fx.sync(&yaml, true, false).await.unwrap_err();
    assert!(err.to_string().to_lowercase().contains("same"), "{err}");
    assert_eq!(
        ids(&fx.src, "items").await,
        vec![1],
        "source must be untouched"
    );
}

#[tokio::test]
async fn dry_run_reports_exact_row_counts_and_modifies_nothing() {
    let schema = "CREATE TABLE items (id BIGINT UNSIGNED PRIMARY KEY, name VARCHAR(20), qty INT) \
                  ENGINE=InnoDB; CREATE TABLE plain (id BIGINT UNSIGNED PRIMARY KEY) ENGINE=InnoDB;";
    let Some(fx) = Fixture::with_schema(
        "dry",
        schema,
        "INSERT INTO items VALUES (1,'a',1),(2,'b',2),(3,'c',3); INSERT INTO plain VALUES (1),(2),(3),(4);",
    )
    .await
    else {
        return;
    };
    let yaml = fx.yaml(
        "",
        "mode: replace\ntables:\n  items: { where: \"qty > 1\" }\n  plain: {}\n",
    );
    let s = fx.sync(&yaml, true, true).await.unwrap();
    assert!(s.dry_run);
    assert_eq!(s.estimated_rows["items"], Some(2));
    assert_eq!(s.estimated_rows["plain"], Some(4));
    assert_eq!(count(&fx.dst, "SELECT COUNT(*) FROM items").await, 0);
}

const SELECTION_YAML: &str = r#"
mode: merge
tables:
  users: {}
  orders: {}
  order_items: {}
  notes: {}
groups:
  recent: [orders]
"#;

#[tokio::test]
async fn group_copies_members_and_their_parents_only() {
    let Some(fx) = Fixture::with_schema("group", FK_SCHEMA, FK_SEED).await else {
        return;
    };
    let yaml = fx.yaml("", SELECTION_YAML);
    let resolved = fx.resolve_with(&yaml, None, Some("recent".into()), false, false);
    dumpling::sync::run_sync(&resolved, &dumpling::progress::Reporter::new(true))
        .await
        .unwrap();
    assert_eq!(ids(&fx.dst, "users").await, vec![1, 2], "parent pulled in");
    assert_eq!(ids(&fx.dst, "orders").await, vec![10, 11]);
    assert!(
        ids(&fx.dst, "order_items").await.is_empty(),
        "child not selected"
    );
}

#[tokio::test]
async fn tables_flag_restricts_the_run() {
    let Some(fx) = Fixture::with_schema("tablesflag", FK_SCHEMA, FK_SEED).await else {
        return;
    };
    let yaml = fx.yaml("", SELECTION_YAML);
    let resolved = fx.resolve_with(&yaml, Some(vec!["users".into()]), None, false, false);
    dumpling::sync::run_sync(&resolved, &dumpling::progress::Reporter::new(true))
        .await
        .unwrap();
    assert_eq!(ids(&fx.dst, "users").await, vec![1, 2]);
    assert!(ids(&fx.dst, "orders").await.is_empty());
}

#[tokio::test]
async fn excluded_parent_is_reported_not_silently_skipped() {
    let Some(fx) = Fixture::with_schema("missingdep", FK_SCHEMA, FK_SEED).await else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  orders: {}\nexclude: [users]\n");
    let err = fx.sync(&yaml, false, false).await.unwrap_err().to_string();
    assert!(err.contains("users"), "{err}");
    assert!(ids(&fx.dst, "orders").await.is_empty());
}

/// The source read fails after earlier batches were already inserted. The
/// destination must not commit those batches.
#[tokio::test]
async fn read_failure_midway_commits_nothing() {
    let schema = "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(20)) ENGINE=InnoDB;";
    let seed = "SET SESSION cte_max_recursion_depth = 2000; \
                INSERT INTO t WITH RECURSIVE n AS (SELECT 1 i UNION ALL SELECT i+1 FROM n WHERE i < 1500) \
                SELECT i, 'x' FROM n;";
    let Some(fx) = Fixture::with_schema("readfail", schema, seed).await else {
        return;
    };
    // The filter is fine for ids <= 1000 and raises "Invalid JSON text" after that.
    let yaml = fx.yaml(
        "  batch_size: 250",
        "mode: merge\ntables:\n  t:\n    where: \"id <= 1000 OR JSON_EXTRACT(CONCAT('{bad', id), '$') IS NOT NULL\"\n",
    );
    let err = fx.sync(&yaml, false, false).await.unwrap_err().to_string();
    assert!(err.contains("reading `t` failed"), "{err}");
    assert_eq!(
        count(&fx.dst, "SELECT COUNT(*) FROM t").await,
        0,
        "batches written before the failure must roll back"
    );
}

#[tokio::test]
async fn generated_columns_are_not_written() {
    let ddl = "CREATE TABLE t (id INT PRIMARY KEY, a INT NOT NULL, b INT NOT NULL, \
               total INT GENERATED ALWAYS AS (a + b) STORED, \
               v INT GENERATED ALWAYS AS (a * 2) VIRTUAL) ENGINE=InnoDB;";
    let Some(fx) = Fixture::new(
        "gencols",
        &format!("{ddl} INSERT INTO t (id, a, b) VALUES (1, 1, 2), (2, 3, 4);"),
        ddl,
    )
    .await
    else {
        return;
    };
    for mode in ["merge", "replace"] {
        let yaml = fx.yaml("", &format!("mode: {mode}\ntables:\n  t: {{}}\n"));
        fx.sync(&yaml, mode == "replace", false).await.unwrap();
        assert_eq!(
            count(&fx.dst, "SELECT CAST(SUM(total) AS SIGNED) FROM t").await,
            3 + 7,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn merge_into_pk_only_table_surfaces_fk_violations() {
    let src = "CREATE TABLE parents (id INT PRIMARY KEY) ENGINE=InnoDB; \
               CREATE TABLE links (parent_id INT, role_id INT, PRIMARY KEY (parent_id, role_id)) ENGINE=InnoDB; \
               INSERT INTO links VALUES (1, 1), (2, 2);";
    let dst = "CREATE TABLE parents (id INT PRIMARY KEY) ENGINE=InnoDB; \
               CREATE TABLE links (parent_id INT, role_id INT, PRIMARY KEY (parent_id, role_id), \
               FOREIGN KEY (parent_id) REFERENCES parents (id)) ENGINE=InnoDB;";
    let Some(fx) = Fixture::new("pkonlyfk", src, dst).await else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  links: {}\n");
    let res = fx.sync(&yaml, false, false).await;
    assert!(res.is_err(), "FK violation must not be silently ignored");
    assert_eq!(count(&fx.dst, "SELECT COUNT(*) FROM links").await, 0);
}

#[tokio::test]
async fn merge_into_pk_only_table_ignores_duplicates() {
    let ddl = "CREATE TABLE links (a INT, b INT, PRIMARY KEY (a, b)) ENGINE=InnoDB;";
    let Some(fx) = Fixture::new(
        "pkonlydup",
        &format!("{ddl} INSERT INTO links VALUES (1,1),(2,2);"),
        &format!("{ddl} INSERT INTO links VALUES (1,1);"),
    )
    .await
    else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  links: {}\n");
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(count(&fx.dst, "SELECT COUNT(*) FROM links").await, 2);
}

#[tokio::test]
async fn functional_unique_index_does_not_break_introspection() {
    let ddl = "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20), \
               UNIQUE KEY uq_lower ((LOWER(name)))) ENGINE=InnoDB;";
    let Some(fx) = Fixture::new(
        "funcidx",
        &format!("{ddl} INSERT INTO t VALUES (1,'a'),(2,'b');"),
        ddl,
    )
    .await
    else {
        return;
    };
    let yaml = fx.yaml("", "mode: merge\ntables:\n  t: {}\n");
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(ids(&fx.dst, "t").await, vec![1, 2]);
}

#[tokio::test]
async fn cross_schema_foreign_key_is_not_matched_to_local_table() {
    // `users` exists locally, but the FK points at another schema's `users`.
    let sql = "DROP DATABASE IF EXISTS dt_xs_other; CREATE DATABASE dt_xs_other; \
               CREATE TABLE dt_xs_other.users (id INT PRIMARY KEY) ENGINE=InnoDB; \
               CREATE TABLE users (id INT PRIMARY KEY) ENGINE=InnoDB; \
               CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, \
               FOREIGN KEY (user_id) REFERENCES dt_xs_other.users (id)) ENGINE=InnoDB;";
    let Some(fx) = Fixture::new("xschema", sql, "SELECT 1").await else {
        return;
    };
    let schema = dumpling::db::introspect::introspect(&fx.src).await.unwrap();
    assert!(schema.tables["orders"].foreign_keys.is_empty());
}

/// Page with batch_size 1 so every row boundary exercises the keyset compare.
async fn assert_pages_completely(name: &str, ddl: &str, insert: &str, count_expected: i64) {
    let Some(fx) = Fixture::new(name, &format!("{ddl} {insert}"), ddl).await else {
        return;
    };
    for mode in ["merge", "replace"] {
        let yaml = fx.yaml(
            "  batch_size: 1",
            &format!("mode: {mode}\ntables:\n  t: {{}}\n"),
        );
        fx.sync(&yaml, mode == "replace", false).await.unwrap();
        assert_eq!(
            count(&fx.dst, "SELECT COUNT(*) FROM t").await,
            count_expected,
            "{name} {mode}"
        );
        assert_eq!(
            dump(&fx.src, "t", "k").await,
            dump(&fx.dst, "t", "k").await,
            "{name} {mode}"
        );
    }
}

#[tokio::test]
async fn pages_enum_key_in_index_order() {
    // Index order (z, a, m) differs from alphabetical order.
    assert_pages_completely(
        "pageenum",
        "CREATE TABLE t (k ENUM('z','a','m') NOT NULL PRIMARY KEY, v INT) ENGINE=InnoDB;",
        "INSERT INTO t VALUES ('z',1),('a',2),('m',3);",
        3,
    )
    .await;
}

#[tokio::test]
async fn pages_set_key_in_bitmask_order() {
    assert_pages_completely(
        "pageset",
        "CREATE TABLE t (k SET('z','a','m') NOT NULL PRIMARY KEY, v INT) ENGINE=InnoDB;",
        "INSERT INTO t VALUES ('z',1),('a',2),('m',3),('z,a',4),('a,m',5);",
        5,
    )
    .await;
}

#[tokio::test]
async fn pages_decimal_key_without_precision_loss() {
    // Neighbouring values that are equal once converted to a double.
    assert_pages_completely(
        "pagedec",
        "CREATE TABLE t (k DECIMAL(30,10) NOT NULL PRIMARY KEY, v INT) ENGINE=InnoDB;",
        "INSERT INTO t VALUES (1234567890123456789.0000000001,1),\
         (1234567890123456789.0000000002,2),(1234567890123456789.0000000003,3);",
        3,
    )
    .await;
}

#[tokio::test]
async fn pages_temporal_keys_with_fractional_seconds() {
    assert_pages_completely(
        "pagedt",
        "CREATE TABLE t (k DATETIME(6) NOT NULL PRIMARY KEY, v INT) ENGINE=InnoDB;",
        "INSERT INTO t VALUES ('2024-01-01 00:00:00.000001',1),\
         ('2024-01-01 00:00:00.000002',2),('2024-01-01 00:00:00.000003',3);",
        3,
    )
    .await;
}
