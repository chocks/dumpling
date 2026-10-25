use crate::config::TransformSpec;
use crate::error::{DumplingError, Result};
use crate::transform::builtin::{self, TransformKind};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

/// Value after transformation — never log the inner content in diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub enum TransformedValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    /// Pass-through of an already-safe SQL expression fragment for source-side transforms.
    /// Only used for documented source-side cases; never built from untrusted input.
    Preserve,
}

#[derive(Debug, Clone)]
pub struct TransformContext {
    pub deterministic: bool,
    pub secret: Vec<u8>,
    /// Scope key for uniqueness (e.g. table.column).
    pub scope: String,
    identity_cache: HashMap<(String, String), String>,
    unique_counters: HashMap<String, u64>,
}

impl TransformContext {
    pub fn new(deterministic: bool, secret: impl AsRef<[u8]>) -> Self {
        Self {
            deterministic,
            secret: secret.as_ref().to_vec(),
            scope: String::new(),
            identity_cache: HashMap::new(),
            unique_counters: HashMap::new(),
        }
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    pub fn keyed_digest(&self, material: &str) -> [u8; 32] {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(self.scope.as_bytes());
        mac.update(b"|");
        mac.update(material.as_bytes());
        mac.finalize().into_bytes().into()
    }

    pub fn hex_digest(&self, material: &str) -> String {
        hex::encode(self.keyed_digest(material))
    }

    pub fn remember_identity(&mut self, kind: &str, original: &str, replacement: String) -> String {
        let key = (kind.to_string(), original.to_string());
        self.identity_cache
            .entry(key)
            .or_insert(replacement)
            .clone()
    }

    pub fn lookup_identity(&self, kind: &str, original: &str) -> Option<String> {
        self.identity_cache
            .get(&(kind.to_string(), original.to_string()))
            .cloned()
    }

    pub fn next_unique(&mut self, kind: &str) -> u64 {
        let counter = self.unique_counters.entry(kind.to_string()).or_insert(0);
        *counter += 1;
        *counter
    }
}

#[derive(Debug, Default)]
pub struct TransformRegistry {
    kinds: HashMap<String, TransformKind>,
}

impl TransformRegistry {
    pub fn with_builtins() -> Self {
        let mut kinds = HashMap::new();
        for (name, kind) in builtin::builtins() {
            kinds.insert(name.to_string(), kind);
        }
        Self { kinds }
    }

    pub fn validate_spec(&self, spec: &TransformSpec) -> Result<()> {
        let name = spec.name();
        if name == "null" || name == "NULL" {
            return Ok(());
        }
        if !self.kinds.contains_key(name) {
            return Err(DumplingError::Validation(format!(
                "unknown transform `{name}`; known: {}",
                self.known_names().join(", ")
            )));
        }
        if name == "constant" && spec.constant_value().is_none() {
            return Err(DumplingError::Validation(
                "transform `constant` requires a `value` field".into(),
            ));
        }
        if name == "preserve" {
            // preserve on potentially sensitive columns is allowed but callers
            // should warn; validation accepts it as an explicit choice.
        }
        Ok(())
    }

    pub fn known_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.kinds.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    pub fn apply(
        &self,
        spec: &TransformSpec,
        original: Option<&str>,
        ctx: &mut TransformContext,
    ) -> Result<TransformedValue> {
        let name = if spec.name() == "NULL" {
            "null"
        } else {
            spec.name()
        };
        let kind = if name == "null" {
            &TransformKind::Null
        } else {
            self.kinds
                .get(name)
                .ok_or_else(|| DumplingError::Validation(format!("unknown transform `{name}`")))?
        };
        builtin::apply(kind, spec, original, ctx)
    }

    /// Whether this transform can be evaluated as a MySQL expression in the
    /// SELECT list so raw values never leave the database process.
    pub fn supports_source_side(&self, name: &str) -> bool {
        matches!(name, "null" | "NULL" | "constant" | "preserve")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_email_is_stable_for_same_input() {
        let reg = TransformRegistry::with_builtins();
        let mut ctx = TransformContext::new(true, b"test-secret").with_scope("users.email");
        let spec = TransformSpec::Named("unique_email".into());
        let a = reg.apply(&spec, Some("alice@corp.com"), &mut ctx).unwrap();
        let b = reg.apply(&spec, Some("alice@corp.com"), &mut ctx).unwrap();
        assert_eq!(a, b);
        if let TransformedValue::Text(email) = a {
            assert!(
                email.ends_with(".invalid"),
                "expected reserved TLD, got {email}"
            );
            assert!(!email.contains("alice"));
            assert!(!email.contains("corp.com"));
        } else {
            panic!("expected text");
        }
    }

    #[test]
    fn rejects_unknown_transform() {
        let reg = TransformRegistry::with_builtins();
        let err = reg
            .validate_spec(&TransformSpec::Named("make_pretty".into()))
            .unwrap_err();
        assert!(err.to_string().contains("unknown transform"));
    }
}
