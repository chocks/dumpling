//! Phase 3: anonymization end to end against real MySQL.

mod common;
use common::*;
use sqlx::{MySqlPool, Row};

const SCHEMA: &str = r#"
CREATE TABLE users (
  id INT PRIMARY KEY,
  email VARCHAR(255) NOT NULL,
  first_name VARCHAR(50), last_name VARCHAR(50),
  phone VARCHAR(32), password_hash VARCHAR(255), dob DATE, bio TEXT,
  UNIQUE KEY uq_email (email), UNIQUE KEY uq_phone (phone)
) ENGINE=InnoDB;
CREATE TABLE orders (
  id INT PRIMARY KEY, user_id INT NOT NULL, contact_email VARCHAR(255) NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users (id)
) ENGINE=InnoDB;
"#;

const SEED: &str = r#"
INSERT INTO users WITH RECURSIVE n AS (SELECT 1 i UNION ALL SELECT i+1 FROM n WHERE i < 300)
SELECT i, CONCAT('person', i, '@real-customer.com'), CONCAT('Zoltan', i), CONCAT('Quillfeather', i),
       CONCAT('+44-20-7946-', LPAD(i, 4, '0')), CONCAT('$2b$topsecret', i),
       DATE_ADD('1971-03-04', INTERVAL i DAY), CONCAT('bio ', i)
FROM n;
INSERT INTO orders WITH RECURSIVE n AS (SELECT 1 i UNION ALL SELECT i+1 FROM n WHERE i < 300)
SELECT i, i, CONCAT('person', i, '@real-customer.com') FROM n;
"#;

const RULES: &str = r#"
mode: merge
tables:
  users:
    columns:
      email: { transform: unique_email }
      first_name: { transform: fake_first_name }
      last_name: { transform: fake_last_name }
      phone: { transform: unique_phone }
      password_hash: { transform: "null" }
      dob: { transform: random_date, min: "1990-01-01", max: "1995-12-31" }
  orders:
    columns:
      contact_email: { transform: unique_email }
"#;

async fn fixture(name: &str) -> Option<Fixture> {
    Fixture::with_schema(name, SCHEMA, SEED).await
}

/// Every cell of every table as a string.
async fn all_cells(pool: &MySqlPool) -> Vec<String> {
    let mut cells = Vec::new();
    for t in ["users", "orders"] {
        for row in dump(pool, t, "id").await {
            cells.extend(row.into_iter().flatten());
        }
    }
    cells
}

#[tokio::test]
async fn no_original_sensitive_value_reaches_the_destination() {
    let Some(fx) = fixture("noleak").await else {
        return;
    };
    let yaml = fx.yaml("  transform_secret: test-secret", RULES);
    fx.sync(&yaml, false, false).await.unwrap();

    // Original sensitive values, read straight from the source.
    let mut sensitive = Vec::new();
    for row in sqlx::query(
        "SELECT email, first_name, last_name, phone, password_hash, CAST(dob AS CHAR) d FROM users",
    )
    .fetch_all(&fx.src)
    .await
    .unwrap()
    {
        for col in [
            "email",
            "first_name",
            "last_name",
            "phone",
            "password_hash",
            "d",
        ] {
            sensitive.push(row.get::<String, _>(col));
        }
    }
    assert_eq!(sensitive.len(), 300 * 6);

    let cells = all_cells(&fx.dst).await;
    assert!(cells.len() > 1000);
    for value in &sensitive {
        assert!(
            !cells.iter().any(|c| c.contains(value.as_str())),
            "an original sensitive value was found in the destination"
        );
    }
    // Untouched columns are copied as-is.
    assert_eq!(
        count(&fx.dst, "SELECT COUNT(*) FROM users WHERE bio LIKE 'bio %'").await,
        300
    );
}

#[tokio::test]
async fn constraints_and_identities_survive_anonymization() {
    let Some(fx) = fixture("identity").await else {
        return;
    };
    let yaml = fx.yaml("  transform_secret: test-secret", RULES);
    fx.sync(&yaml, false, false).await.unwrap();

    assert_eq!(
        count(&fx.dst, "SELECT COUNT(DISTINCT email) FROM users").await,
        300
    );
    assert_eq!(
        count(&fx.dst, "SELECT COUNT(DISTINCT phone) FROM users").await,
        300
    );
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM users WHERE password_hash IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM users WHERE dob NOT BETWEEN '1990-01-01' AND '1995-12-31'"
        )
        .await,
        0
    );
    // Same person, two tables, one replacement email.
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM orders o JOIN users u ON u.id = o.user_id \
             WHERE o.contact_email <> u.email"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM users WHERE email NOT LIKE '%@example.invalid'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn same_secret_is_stable_and_a_different_secret_is_not() {
    let Some(fx) = fixture("secret").await else {
        return;
    };
    let run = |secret: &'static str| {
        let yaml = fx.yaml(&format!("  transform_secret: {secret}"), RULES);
        let fx = &fx;
        async move {
            fx.sync(&yaml, false, false).await.unwrap();
            dump(&fx.dst, "users", "id").await
        }
    };
    let a1 = run("secret-a").await;
    let a2 = run("secret-a").await;
    let b = run("secret-b").await;
    assert_eq!(a1, a2);
    assert_ne!(a1, b);
}

#[tokio::test]
async fn without_a_secret_output_is_random_per_run_but_consistent_within_one() {
    let Some(fx) = fixture("nosecret").await else {
        return;
    };
    let yaml = fx.yaml("", RULES);
    fx.sync(&yaml, false, false).await.unwrap();
    let first = dump(&fx.dst, "users", "id").await;
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM orders o JOIN users u ON u.id = o.user_id \
             WHERE o.contact_email <> u.email"
        )
        .await,
        0
    );
    fx.sync(&yaml, false, false).await.unwrap();
    assert_ne!(first, dump(&fx.dst, "users", "id").await);
}

#[tokio::test]
async fn anonymize_false_copies_raw_values() {
    let Some(fx) = fixture("raw").await else {
        return;
    };
    let yaml = fx.yaml("  anonymize: false", RULES);
    fx.sync(&yaml, false, false).await.unwrap();
    assert_eq!(
        dump(&fx.src, "users", "id").await,
        dump(&fx.dst, "users", "id").await
    );
}

async fn rejected(name: &str, schema: &str, rules: &str, expect: &str) {
    let Some(fx) = Fixture::with_schema(name, schema, "INSERT INTO t (id) VALUES (1);").await
    else {
        return;
    };
    sqlx::query("INSERT INTO t (id) VALUES (7)")
        .execute(&fx.dst)
        .await
        .unwrap();
    let yaml = fx.yaml(
        "",
        &format!("mode: replace\ntables:\n  t:\n    columns:\n{rules}"),
    );
    let err = fx.sync(&yaml, true, false).await.unwrap_err().to_string();
    assert!(err.contains(expect), "expected `{expect}` in: {err}");
    assert_eq!(ids(&fx.dst, "t").await, vec![7], "nothing may be written");
}

#[tokio::test]
async fn transform_that_cannot_fit_the_column_is_rejected_up_front() {
    rejected(
        "toosmall",
        "CREATE TABLE t (id INT PRIMARY KEY, e VARCHAR(10));",
        "      e: { transform: unique_email }",
        "t.e",
    )
    .await;
}

#[tokio::test]
async fn text_transform_on_a_numeric_column_is_rejected_up_front() {
    rejected(
        "numeric",
        "CREATE TABLE t (id INT PRIMARY KEY, e INT);",
        "      e: { transform: hash }",
        "t.e",
    )
    .await;
}

#[tokio::test]
async fn transform_on_a_paging_key_is_rejected() {
    rejected(
        "keycol",
        "CREATE TABLE t (id INT PRIMARY KEY, e VARCHAR(20));",
        "      id: { transform: hash }",
        "t.id",
    )
    .await;
}

#[tokio::test]
async fn database_errors_do_not_echo_values() {
    let Some(fx) = Fixture::with_schema(
        "errleak",
        "CREATE TABLE t (id INT PRIMARY KEY, email VARCHAR(50), UNIQUE KEY (email)) ENGINE=InnoDB;",
        "INSERT INTO t VALUES (1, 'a@x.com'), (2, 'b@x.com');",
    )
    .await
    else {
        return;
    };
    // Both rows get the same constant -> duplicate-entry error from MySQL,
    // whose message normally quotes the value.
    let yaml = fx.yaml(
        "",
        "mode: replace\ntables:\n  t:\n    columns:\n      email: { transform: constant, value: LEAKCHECK-VALUE }\n",
    );
    let err = fx.sync(&yaml, true, false).await.unwrap_err().to_string();
    assert!(!err.contains("LEAKCHECK-VALUE"), "{err}");
    assert!(err.contains("1062"), "expected the MySQL error code: {err}");
}
