use crate::config::TransformSpec;
use crate::db::introspect::ColumnInfo;
use crate::error::{DumplingError, Result};
use crate::transform::registry::{TransformContext, TransformedValue};
use chrono::NaiveDate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Constant,
    Preserve,
    FakeEmail,
    UniqueEmail,
    FakeFirstName,
    FakeLastName,
    FakePhone,
    UniquePhone,
    FakeAddress,
    RandomDate,
    Hash,
}

pub fn builtins() -> Vec<(&'static str, Kind)> {
    vec![
        ("null", Kind::Null),
        ("constant", Kind::Constant),
        ("preserve", Kind::Preserve),
        ("fake_email", Kind::FakeEmail),
        ("unique_email", Kind::UniqueEmail),
        ("fake_first_name", Kind::FakeFirstName),
        ("fake_last_name", Kind::FakeLastName),
        ("fake_phone", Kind::FakePhone),
        ("unique_phone", Kind::UniquePhone),
        ("fake_address", Kind::FakeAddress),
        ("random_date", Kind::RandomDate),
        ("hash", Kind::Hash),
    ]
}

const FIRST_NAMES: &[&str] = &[
    "Alex", "Jordan", "Taylor", "Casey", "Riley", "Morgan", "Quinn", "Avery", "Jamie", "Reese",
    "Rowan", "Skyler", "Emery", "Finley", "Harper", "Kai", "Logan", "Noel", "Parker", "Sage",
];
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Lee", "Garcia", "Brown", "Davis", "Miller", "Wilson", "Moore", "Taylor",
    "Anderson", "Thomas", "Clark", "Lewis", "Walker", "Hall", "Young", "King", "Wright", "Scott",
];
const STREETS: &[&str] = &[
    "Maple",
    "Oak",
    "Cedar",
    "Pine",
    "Elm",
    "Birch",
    "Willow",
    "Lakeview",
    "Hillcrest",
    "Sunset",
];
const CITIES: &[&str] = &[
    "Springfield",
    "Riverton",
    "Fairview",
    "Georgetown",
    "Clinton",
    "Madison",
    "Salem",
    "Franklin",
];

/// Longest string the transform can produce, if it is a fixed-shape text
/// transform. Used to reject columns that are too small before any write.
pub fn max_output_len(kind: Kind) -> Option<usize> {
    match kind {
        Kind::FakeEmail | Kind::UniqueEmail => Some(36),
        Kind::FakeFirstName | Kind::FakeLastName => Some(10),
        Kind::FakePhone | Kind::UniquePhone => Some(14),
        Kind::FakeAddress => Some(60),
        Kind::RandomDate => Some(10),
        Kind::Hash => Some(64),
        Kind::Null | Kind::Constant | Kind::Preserve => None,
    }
}

/// Reject a transform that cannot work on `col` (wrong type, or output longer
/// than the column), so the problem shows up at planning time.
pub fn check_column(kind: Kind, table: &str, col: &ColumnInfo) -> Result<()> {
    let Some(max) = max_output_len(kind) else {
        return Ok(());
    };
    let ty = col.data_type.to_ascii_lowercase();
    let text = matches!(
        ty.as_str(),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext"
    );
    let date_ok =
        kind == Kind::RandomDate && matches!(ty.as_str(), "date" | "datetime" | "timestamp");
    if !text && !date_ok {
        return Err(DumplingError::Validation(format!(
            "transform on `{table}.{}` produces text but the column type is `{}`",
            col.name, col.column_type
        )));
    }
    let declared = col
        .column_type
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(n, _)| n.parse::<usize>().ok());
    let limit = match ty.as_str() {
        "char" | "varchar" => declared,
        "tinytext" => Some(255),
        _ => None,
    };
    if let Some(limit) = limit
        && limit < max
    {
        return Err(DumplingError::Validation(format!(
            "transform on `{table}.{}` can produce {max} characters but the column holds {limit}",
            col.name
        )));
    }
    Ok(())
}

fn date_bounds(spec: &TransformSpec) -> Result<(NaiveDate, NaiveDate)> {
    let parse = |label: &str, v: Option<&String>, default: (i32, u32, u32)| match v {
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
            DumplingError::Validation(format!("random_date {label} `{s}` is not YYYY-MM-DD"))
        }),
        None => Ok(NaiveDate::from_ymd_opt(default.0, default.1, default.2).unwrap()),
    };
    let min = parse("min", spec.min.as_ref(), (1950, 1, 1))?;
    let max = parse("max", spec.max.as_ref(), (2005, 12, 31))?;
    if min > max {
        return Err(DumplingError::Validation(format!(
            "random_date min {min} is after max {max}"
        )));
    }
    Ok((min, max))
}

pub fn validate(kind: Kind, spec: &TransformSpec) -> Result<()> {
    match kind {
        Kind::Constant if spec.constant_value().is_none() => Err(DumplingError::Validation(
            "transform `constant` requires a `value`".into(),
        )),
        Kind::RandomDate => date_bounds(spec).map(|_| ()),
        _ => Ok(()),
    }
}

pub fn apply(
    kind: Kind,
    spec: &TransformSpec,
    original: Option<&str>,
    ctx: &mut TransformContext,
) -> Result<TransformedValue> {
    match kind {
        Kind::Null => return Ok(TransformedValue::Null),
        Kind::Constant => {
            return yaml_to_value(spec.constant_value().ok_or_else(|| {
                DumplingError::Validation("transform `constant` requires a `value`".into())
            })?);
        }
        _ => {}
    }
    // SQL NULL carries no personal data; keep it NULL.
    let Some(orig) = original else {
        return Ok(TransformedValue::Null);
    };
    let text = match kind {
        Kind::Preserve => orig.to_string(),
        Kind::Hash => ctx.pseudonym("hash", orig, |d| hex::encode(d)),
        Kind::FakeEmail => ctx.pseudonym("email", orig, email),
        Kind::UniqueEmail => ctx.unique_pseudonym("email", orig, email),
        Kind::FakeFirstName => ctx.pseudonym("first_name", orig, |d| pick(FIRST_NAMES, d, 0)),
        Kind::FakeLastName => ctx.pseudonym("last_name", orig, |d| pick(LAST_NAMES, d, 0)),
        Kind::FakePhone => ctx.pseudonym("phone", orig, phone),
        Kind::UniquePhone => ctx.unique_pseudonym("phone", orig, phone),
        Kind::FakeAddress => ctx.pseudonym("address", orig, address),
        Kind::RandomDate => {
            let (min, max) = date_bounds(spec)?;
            let days = (max - min).num_days() as u64 + 1;
            ctx.pseudonym("date", orig, |d| {
                (min + chrono::Duration::days((word(d, 0) % days) as i64))
                    .format("%Y-%m-%d")
                    .to_string()
            })
        }
        Kind::Null | Kind::Constant => unreachable!(),
    };
    Ok(TransformedValue::Text(text))
}

/// 8 bytes of the digest as a number; `slot` selects which 8.
fn word(d: &[u8; 32], slot: usize) -> u64 {
    u64::from_le_bytes(d[slot * 8..slot * 8 + 8].try_into().unwrap())
}

fn pick(list: &[&str], d: &[u8; 32], slot: usize) -> String {
    list[(word(d, slot) % list.len() as u64) as usize].to_string()
}

// Reserved `.invalid` TLD (RFC 2606): can never be a real mailbox.
fn email(d: &[u8; 32]) -> String {
    format!("user{}@example.invalid", hex::encode(&d[..8]))
}

// 555 is the NANP exchange reserved for fictional numbers.
fn phone(d: &[u8; 32]) -> String {
    format!("+1-555-{:07}", word(d, 0) % 10_000_000)
}

fn address(d: &[u8; 32]) -> String {
    format!(
        "{} {} St, {}, ST {:05}",
        word(d, 0) % 9999 + 1,
        pick(STREETS, d, 1),
        pick(CITIES, d, 2),
        word(d, 3) % 100_000
    )
}

pub fn yaml_to_value(v: &serde_yaml::Value) -> Result<TransformedValue> {
    match v {
        serde_yaml::Value::Null => Ok(TransformedValue::Null),
        serde_yaml::Value::Bool(b) => Ok(TransformedValue::Bool(*b)),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(TransformedValue::Int(i))
            } else if let Some(u) = n.as_u64() {
                Ok(TransformedValue::UInt(u))
            } else {
                Ok(TransformedValue::Float(n.as_f64().unwrap_or_default()))
            }
        }
        serde_yaml::Value::String(s) => Ok(TransformedValue::Text(s.clone())),
        _ => Err(DumplingError::Validation(
            "constant only supports null/bool/number/string".into(),
        )),
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
    const PATTERNS: &[(&str, &str)] = &[
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
    PATTERNS
        .iter()
        .find(|(p, _)| c.contains(p))
        .map(|(_, reason)| SensitiveHeuristic {
            column: format!("{table}.{column}"),
            reason: (*reason).into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_email() {
        assert!(
            scan_column_name("users", "email_address")
                .unwrap()
                .reason
                .contains("email")
        );
    }
}
