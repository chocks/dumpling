use dumpling::config::TransformSpec;
use dumpling::transform::{TransformContext, TransformRegistry, TransformedValue};

#[test]
fn unique_email_never_contains_source() {
    let reg = TransformRegistry::with_builtins();
    let mut ctx = TransformContext::new(true, b"unit-test-secret").with_scope("users.email");
    let spec = TransformSpec::Named("unique_email".into());
    let original = "alice.anderson@corp-example.com";
    let out = reg.apply(&spec, Some(original), &mut ctx).unwrap();
    match out {
        TransformedValue::Text(email) => {
            assert!(email.ends_with(".invalid"));
            assert!(!email.to_ascii_lowercase().contains("alice"));
            assert!(!email.contains("corp-example"));
        }
        _ => panic!("expected text email"),
    }
}

#[test]
fn identity_mapping_is_stable_across_calls() {
    let reg = TransformRegistry::with_builtins();
    let mut ctx = TransformContext::new(true, b"unit-test-secret").with_scope("users.phone");
    let spec = TransformSpec::Named("unique_phone".into());
    let a = reg.apply(&spec, Some("+1-202-555-0101"), &mut ctx).unwrap();
    let b = reg.apply(&spec, Some("+1-202-555-0101"), &mut ctx).unwrap();
    assert_eq!(a, b);
}

#[test]
fn null_transform() {
    let reg = TransformRegistry::with_builtins();
    let mut ctx = TransformContext::new(true, b"s");
    let out = reg
        .apply(&TransformSpec::Named("null".into()), Some("x"), &mut ctx)
        .unwrap();
    assert_eq!(out, TransformedValue::Null);
}
