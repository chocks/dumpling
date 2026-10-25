use crate::config::TransformSpec;
use crate::error::{DumplingError, Result};
use crate::transform::builtin::{self, Kind};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};

type HmacSha256 = Hmac<Sha256>;
type Digest = [u8; 32];

/// Value after transformation. Never log the inner content.
#[derive(Debug, Clone, PartialEq)]
pub enum TransformedValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

/// Per-run state for keyed pseudonymization.
///
/// Every replacement is `HMAC(secret, kind | original)` rendered into a
/// plausible value, so the same source value always maps to the same output
/// (across tables too, since the key is the transform kind, not the column).
/// For `unique` kinds an output already issued to a *different* input is
/// rejected and re-derived, so uniqueness constraints hold.
///
/// Memory: unique kinds remember one 32-byte digest and one output per distinct
/// value for the length of the run. Raw source values are never retained.
pub struct TransformContext {
    secret: Vec<u8>,
    issued: HashSet<(&'static str, String)>,
    by_input: HashMap<(&'static str, Digest), String>,
}

impl std::fmt::Debug for TransformContext {
    // Never print the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformContext").finish_non_exhaustive()
    }
}

impl TransformContext {
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self {
            secret: secret.as_ref().to_vec(),
            issued: HashSet::new(),
            by_input: HashMap::new(),
        }
    }

    /// A fresh random secret. Outputs are consistent within the run only.
    pub fn random() -> Self {
        Self::new(rand::random::<[u8; 32]>())
    }

    fn digest(&self, kind: &str, attempt: u32, original: &str) -> Digest {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(kind.as_bytes());
        mac.update(&attempt.to_le_bytes());
        mac.update(original.as_bytes());
        mac.finalize().into_bytes().into()
    }

    /// Deterministic replacement; collisions with other inputs are allowed.
    pub fn pseudonym(
        &self,
        kind: &str,
        original: &str,
        render: impl Fn(&Digest) -> String,
    ) -> String {
        render(&self.digest(kind, 0, original))
    }

    /// Deterministic replacement that is distinct for distinct inputs.
    pub fn unique_pseudonym(
        &mut self,
        kind: &'static str,
        original: &str,
        render: impl Fn(&Digest) -> String,
    ) -> String {
        let first = self.digest(kind, 0, original);
        if let Some(out) = self.by_input.get(&(kind, first)) {
            return out.clone();
        }
        let mut digest = first;
        let mut attempt = 0;
        loop {
            let out = render(&digest);
            if self.issued.insert((kind, out.clone())) {
                self.by_input.insert((kind, first), out.clone());
                return out;
            }
            attempt += 1;
            digest = self.digest(kind, attempt, original);
        }
    }
}

#[derive(Debug, Default)]
pub struct TransformRegistry {
    kinds: HashMap<String, Kind>,
}

impl TransformRegistry {
    pub fn with_builtins() -> Self {
        Self {
            kinds: builtin::builtins()
                .into_iter()
                .map(|(n, k)| (n.to_string(), k))
                .collect(),
        }
    }

    pub fn kind(&self, spec: &TransformSpec) -> Result<Kind> {
        let name = if spec.name() == "NULL" {
            "null"
        } else {
            spec.name()
        };
        self.kinds.get(name).copied().ok_or_else(|| {
            DumplingError::Validation(format!(
                "unknown transform `{name}`; known: {}",
                self.known_names().join(", ")
            ))
        })
    }

    pub fn validate_spec(&self, spec: &TransformSpec) -> Result<()> {
        let kind = self.kind(spec)?;
        builtin::validate(kind, spec)
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
        builtin::apply(self.kind(spec)?, spec, original, ctx)
    }
}
