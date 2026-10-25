//! Unit-level behaviour of the built-in transformations (no database).

use dumpling::config::TransformSpec;
use dumpling::transform::{TransformContext, TransformRegistry, TransformedValue};
use std::collections::HashSet;

fn apply(name: &str, original: Option<&str>, ctx: &mut TransformContext) -> TransformedValue {
    TransformRegistry::with_builtins()
        .apply(&TransformSpec::named(name), original, ctx)
        .unwrap()
}

fn text(v: TransformedValue) -> String {
    match v {
        TransformedValue::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn same_secret_gives_same_output_across_runs_and_other_secret_differs() {
    for name in [
        "unique_email",
        "fake_first_name",
        "unique_phone",
        "fake_address",
        "hash",
    ] {
        let a = apply(
            name,
            Some("alice@corp.com"),
            &mut TransformContext::new(b"secret-1"),
        );
        let b = apply(
            name,
            Some("alice@corp.com"),
            &mut TransformContext::new(b"secret-1"),
        );
        let c = apply(
            name,
            Some("alice@corp.com"),
            &mut TransformContext::new(b"secret-2"),
        );
        assert_eq!(a, b, "{name} must be stable for one secret");
        assert_ne!(a, c, "{name} must depend on the secret");
    }
}

#[test]
fn random_secret_differs_per_run() {
    let a = text(apply("hash", Some("x"), &mut TransformContext::random()));
    let b = text(apply("hash", Some("x"), &mut TransformContext::random()));
    assert_ne!(a, b);
}

#[test]
fn output_never_contains_the_source_value() {
    let mut ctx = TransformContext::new(b"s");
    for (name, original) in [
        ("unique_email", "alice.anderson@corp-example.com"),
        ("fake_email", "alice.anderson@corp-example.com"),
        ("unique_phone", "+1-202-555-0101"),
        ("fake_phone", "+1-202-555-0101"),
        ("fake_address", "1600 Pennsylvania Avenue"),
        ("hash", "alice.anderson@corp-example.com"),
    ] {
        let out = text(apply(name, Some(original), &mut ctx)).to_lowercase();
        assert!(!out.contains(&original.to_lowercase()), "{name}: {out}");
        assert!(!out.contains("alice"), "{name}: {out}");
        assert!(!out.contains("corp-example"), "{name}: {out}");
    }
}

#[test]
fn emails_use_a_reserved_domain() {
    let mut ctx = TransformContext::new(b"s");
    for name in ["fake_email", "unique_email"] {
        assert!(text(apply(name, Some("a@b.com"), &mut ctx)).ends_with("@example.invalid"));
    }
}

#[test]
fn unique_transforms_never_collide() {
    // The phone space has only 10M values: 50k inputs would collide ~120 times
    // without explicit collision handling.
    for name in ["unique_phone", "unique_email"] {
        let mut ctx = TransformContext::new(b"s");
        let outputs: HashSet<String> = (0..50_000)
            .map(|i| text(apply(name, Some(&format!("person-{i}")), &mut ctx)))
            .collect();
        assert_eq!(outputs.len(), 50_000, "{name} produced duplicates");
    }
}

#[test]
fn same_input_maps_to_same_output_within_a_run() {
    let mut ctx = TransformContext::new(b"s");
    let a = apply("unique_phone", Some("+1-202-555-0101"), &mut ctx);
    for i in 0..1000 {
        apply("unique_phone", Some(&format!("other-{i}")), &mut ctx);
    }
    assert_eq!(a, apply("unique_phone", Some("+1-202-555-0101"), &mut ctx));
}

#[test]
fn sql_null_stays_null() {
    let mut ctx = TransformContext::new(b"s");
    for name in [
        "unique_email",
        "fake_first_name",
        "unique_phone",
        "hash",
        "random_date",
    ] {
        assert_eq!(
            apply(name, None, &mut ctx),
            TransformedValue::Null,
            "{name}"
        );
    }
}

fn date_spec(min: Option<&str>, max: Option<&str>) -> TransformSpec {
    TransformSpec {
        name: "random_date".into(),
        value: None,
        min: min.map(Into::into),
        max: max.map(Into::into),
    }
}

#[test]
fn random_date_stays_within_configured_bounds() {
    let reg = TransformRegistry::with_builtins();
    let spec = date_spec(Some("1990-01-01"), Some("1990-01-31"));
    reg.validate_spec(&spec).unwrap();
    let mut ctx = TransformContext::new(b"s");
    for i in 0..500 {
        let d = text(reg.apply(&spec, Some(&format!("p{i}")), &mut ctx).unwrap());
        assert!(("1990-01-01"..="1990-01-31").contains(&d.as_str()), "{d}");
    }
}

#[test]
fn random_date_rejects_bad_bounds() {
    let reg = TransformRegistry::with_builtins();
    assert!(
        reg.validate_spec(&date_spec(Some("2000-01-01"), Some("1999-01-01")))
            .is_err()
    );
    assert!(
        reg.validate_spec(&date_spec(Some("not-a-date"), None))
            .is_err()
    );
}

#[test]
fn null_and_constant_transforms() {
    let reg = TransformRegistry::with_builtins();
    let mut ctx = TransformContext::new(b"s");
    assert_eq!(
        reg.apply(&TransformSpec::named("null"), Some("x"), &mut ctx)
            .unwrap(),
        TransformedValue::Null
    );
    let spec = TransformSpec {
        name: "constant".into(),
        value: Some(serde_yaml::Value::String("fixed".into())),
        min: None,
        max: None,
    };
    assert_eq!(
        reg.apply(&spec, Some("x"), &mut ctx).unwrap(),
        TransformedValue::Text("fixed".into())
    );
    assert!(
        reg.validate_spec(&TransformSpec::named("constant"))
            .is_err()
    );
}

#[test]
fn unknown_transform_is_rejected() {
    let err = TransformRegistry::with_builtins()
        .validate_spec(&TransformSpec::named("make_pretty"))
        .unwrap_err();
    assert!(err.to_string().contains("unknown transform"));
}
