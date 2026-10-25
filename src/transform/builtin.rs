use crate::config::TransformSpec;
use crate::error::{DumplingError, Result};
use crate::transform::registry::{TransformContext, TransformedValue};
use fake::Fake;
use fake::faker::address::en::{CityName, StreetName, ZipCode};
use fake::faker::name::en::{FirstName, LastName};

#[derive(Debug, Clone)]
pub enum TransformKind {
    Null,
    Constant,
    FakeEmail,
    UniqueEmail,
    FakeFirstName,
    FakeLastName,
    FakePhone,
    UniquePhone,
    RandomDate,
    FakeAddress,
    Hash,
    Preserve,
}

pub fn builtins() -> Vec<(&'static str, TransformKind)> {
    vec![
        ("null", TransformKind::Null),
        ("constant", TransformKind::Constant),
        ("fake_email", TransformKind::FakeEmail),
        ("unique_email", TransformKind::UniqueEmail),
        ("fake_first_name", TransformKind::FakeFirstName),
        ("fake_last_name", TransformKind::FakeLastName),
        ("fake_phone", TransformKind::FakePhone),
        ("unique_phone", TransformKind::UniquePhone),
        ("random_date", TransformKind::RandomDate),
        ("fake_address", TransformKind::FakeAddress),
        ("hash", TransformKind::Hash),
        ("preserve", TransformKind::Preserve),
    ]
}

pub fn known_transform_names() -> Vec<&'static str> {
    builtins().into_iter().map(|(n, _)| n).collect()
}

pub fn apply(
    kind: &TransformKind,
    spec: &TransformSpec,
    original: Option<&str>,
    ctx: &mut TransformContext,
) -> Result<TransformedValue> {
    match kind {
        TransformKind::Null => Ok(TransformedValue::Null),
        TransformKind::Constant => {
            let v = spec.constant_value().ok_or_else(|| {
                DumplingError::Validation("constant transform missing value".into())
            })?;
            Ok(yaml_to_value(v)?)
        }
        TransformKind::Preserve => Ok(TransformedValue::Preserve),
        TransformKind::Hash => {
            let material = original.unwrap_or("");
            let digest = ctx.hex_digest(material);
            Ok(TransformedValue::Text(digest))
        }
        TransformKind::FakeEmail => Ok(TransformedValue::Text(fake_email(ctx, original))),
        TransformKind::UniqueEmail => Ok(TransformedValue::Text(unique_email(ctx, original))),
        TransformKind::FakeFirstName => Ok(TransformedValue::Text(fake_first_name(ctx, original))),
        TransformKind::FakeLastName => Ok(TransformedValue::Text(fake_last_name(ctx, original))),
        TransformKind::FakePhone => Ok(TransformedValue::Text(fake_phone(ctx, original, false))),
        TransformKind::UniquePhone => Ok(TransformedValue::Text(fake_phone(ctx, original, true))),
        TransformKind::RandomDate => Ok(TransformedValue::Text(random_date(ctx, original))),
        TransformKind::FakeAddress => Ok(TransformedValue::Text(fake_address(ctx, original))),
    }
}

fn yaml_to_value(v: &serde_yaml::Value) -> Result<TransformedValue> {
    match v {
        serde_yaml::Value::Null => Ok(TransformedValue::Null),
        serde_yaml::Value::Bool(b) => Ok(TransformedValue::Bool(*b)),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(TransformedValue::Int(i))
            } else if let Some(u) = n.as_u64() {
                Ok(TransformedValue::UInt(u))
            } else if let Some(f) = n.as_f64() {
                Ok(TransformedValue::Float(f))
            } else {
                Err(DumplingError::Validation(
                    "unsupported numeric constant".into(),
                ))
            }
        }
        serde_yaml::Value::String(s) => Ok(TransformedValue::Text(s.clone())),
        _ => Err(DumplingError::Validation(
            "constant transform only supports null/bool/number/string".into(),
        )),
    }
}

fn seed_from(ctx: &TransformContext, original: Option<&str>) -> u64 {
    let material = original.unwrap_or("");
    let d = ctx.keyed_digest(material);
    u64::from_le_bytes(d[0..8].try_into().unwrap())
}

fn fake_email(ctx: &mut TransformContext, original: Option<&str>) -> String {
    if ctx.deterministic {
        if let Some(orig) = original
            && let Some(cached) = ctx.lookup_identity("email", orig)
        {
            return cached;
        }
        let digest = ctx.hex_digest(original.unwrap_or(""));
        let local = &digest[..12];
        let email = format!("{local}@example.invalid");
        if let Some(orig) = original {
            return ctx.remember_identity("email", orig, email);
        }
        email
    } else {
        let n = ctx.next_unique("email");
        format!("user{n}@example.invalid")
    }
}

fn unique_email(ctx: &mut TransformContext, original: Option<&str>) -> String {
    if let Some(orig) = original
        && let Some(cached) = ctx.lookup_identity("unique_email", orig)
    {
        return cached;
    }
    let fallback = ctx.next_unique("unique_email").to_string();
    let material = original.unwrap_or(&fallback);
    let digest = ctx.hex_digest(material);
    // Use reserved .invalid TLD (RFC 2606) — never a real mailbox domain.
    let scope_slug = ctx
        .scope
        .replace('.', "-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>();
    let email = format!(
        "u{}@{}{}",
        &digest[..16],
        if scope_slug.is_empty() {
            "dumpling"
        } else {
            &scope_slug
        },
        ".invalid"
    );
    if let Some(orig) = original {
        ctx.remember_identity("unique_email", orig, email)
    } else {
        email
    }
}

fn fake_first_name(ctx: &mut TransformContext, original: Option<&str>) -> String {
    deterministic_or_fake(ctx, "first_name", original, || FirstName().fake())
}

fn fake_last_name(ctx: &mut TransformContext, original: Option<&str>) -> String {
    deterministic_or_fake(ctx, "last_name", original, || LastName().fake())
}

fn deterministic_or_fake(
    ctx: &mut TransformContext,
    kind: &str,
    original: Option<&str>,
    f: impl FnOnce() -> String,
) -> String {
    if let Some(orig) = original
        && let Some(cached) = ctx.lookup_identity(kind, orig)
    {
        return cached;
    }
    let value = if ctx.deterministic {
        // Pick from a small built-in list using keyed digest — avoids depending
        // on faker RNG seeding APIs across versions.
        let names: &[&str] = match kind {
            "first_name" => &[
                "Alex", "Jordan", "Taylor", "Casey", "Riley", "Morgan", "Quinn", "Avery", "Jamie",
                "Reese", "Rowan", "Skyler",
            ],
            "last_name" => &[
                "Smith", "Johnson", "Lee", "Garcia", "Brown", "Davis", "Miller", "Wilson", "Moore",
                "Taylor", "Anderson", "Thomas",
            ],
            _ => &["Anon"],
        };
        let idx = (seed_from(ctx, original) as usize) % names.len();
        names[idx].to_string()
    } else {
        f()
    };
    if let Some(orig) = original {
        ctx.remember_identity(kind, orig, value)
    } else {
        value
    }
}

fn fake_phone(ctx: &mut TransformContext, original: Option<&str>, unique: bool) -> String {
    let kind = if unique { "unique_phone" } else { "phone" };
    if let Some(orig) = original
        && let Some(cached) = ctx.lookup_identity(kind, orig)
    {
        return cached;
    }
    let seed = seed_from(ctx, original);
    // Synthetic NANP-looking number in reserved 555 exchange where possible.
    let line = seed % 10_000;
    let phone = format!("+1-555-{:04}", line);
    if let Some(orig) = original {
        ctx.remember_identity(kind, orig, phone)
    } else {
        let n = ctx.next_unique(kind);
        format!("+1-555-{:04}", n % 10_000)
    }
}

fn random_date(ctx: &mut TransformContext, original: Option<&str>) -> String {
    let seed = seed_from(ctx, original);
    // Deterministic date in 2000-01-01 .. 2024-12-31
    let day = seed % (365 * 25);
    let base = chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    let date = base + chrono::Duration::days(day as i64);
    date.format("%Y-%m-%d").to_string()
}

fn fake_address(ctx: &mut TransformContext, original: Option<&str>) -> String {
    if let Some(orig) = original
        && let Some(cached) = ctx.lookup_identity("address", orig)
    {
        return cached;
    }
    let value = if ctx.deterministic {
        let seed = seed_from(ctx, original);
        format!(
            "{} Test Street, Suite {}, City{}, ST {:05}",
            (seed % 9999) + 1,
            (seed / 10) % 500,
            seed % 100,
            seed % 100_000
        )
    } else {
        format!(
            "{} {}, {}, {}",
            (100..9999).fake::<u16>(),
            StreetName().fake::<String>(),
            CityName().fake::<String>(),
            ZipCode().fake::<String>()
        )
    };
    if let Some(orig) = original {
        ctx.remember_identity("address", orig, value)
    } else {
        value
    }
}

/// Conservative name-based heuristics for `dumpling scan`.
#[derive(Debug, Clone)]
pub struct SensitiveHeuristic {
    pub column: String,
    pub reason: String,
}

pub fn scan_column_name(table: &str, column: &str) -> Option<SensitiveHeuristic> {
    let c = column.to_ascii_lowercase();
    let patterns: &[(&str, &str)] = &[
        ("password", "credential"),
        ("passwd", "credential"),
        ("secret", "secret"),
        ("token", "auth token"),
        ("api_key", "api key"),
        ("apikey", "api key"),
        ("private_key", "private key"),
        ("ssn", "national identifier"),
        ("social_security", "national identifier"),
        ("email", "email address"),
        ("phone", "phone number"),
        ("mobile", "phone number"),
        ("address", "address"),
        ("credit_card", "payment"),
        ("card_number", "payment"),
        ("cvv", "payment"),
        ("iban", "payment"),
        ("auth", "authentication"),
        ("dob", "date of birth"),
        ("birth", "date of birth"),
        ("salary", "compensation"),
        ("income", "compensation"),
    ];
    for (pat, reason) in patterns {
        if c.contains(pat) {
            return Some(SensitiveHeuristic {
                column: format!("{table}.{column}"),
                reason: (*reason).into(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_does_not_echo_source() {
        let mut ctx = TransformContext::new(true, b"secret");
        let out = apply(
            &TransformKind::Hash,
            &TransformSpec::Named("hash".into()),
            Some("sensitive-value"),
            &mut ctx,
        )
        .unwrap();
        match out {
            TransformedValue::Text(t) => {
                assert!(!t.contains("sensitive"));
                assert_eq!(t.len(), 64);
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn scan_finds_email() {
        let h = scan_column_name("users", "email_address").unwrap();
        assert!(h.reason.contains("email"));
    }
}
