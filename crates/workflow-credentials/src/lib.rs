//! Host-only credential delivery. A trusted local broker writes private,
//! short-lived provider credentials; definitions and task data contain no keys.
//! The provider/gateway must enforce the issued credential's expiry and scope.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::Read, path::PathBuf};

pub const MAX_LEASE_MS: u64 = 300_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub tenant: String,
    pub project: String,
    pub actor: String,
}
impl Principal {
    pub fn validate(&self) -> Result<(), Rejected> {
        for id in [&self.tenant, &self.project, &self.actor] {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                return Err(Rejected);
            }
        }
        Ok(())
    }
}

/// A reference to a broker-owned file, never a credential literal. Each call
/// rereads the file, so atomic replacement rotates credentials without changing
/// the workflow, policy, host binding or running worker.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseRef {
    pub path: PathBuf,
    pub principal: Principal,
}

// Deserialize only: no general-purpose serialization or Debug path for secrets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema_version: u32,
    principal: Principal,
    audience: String,
    not_before_unix_ms: u64,
    expires_at_unix_ms: u64,
    secret: String,
}
pub struct Credential {
    secret: String,
    expires_at_unix_ms: u64,
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rejected;
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("credential reference rejected")
    }
}
impl std::error::Error for Rejected {}
impl Credential {
    pub fn expose(&self) -> &str {
        &self.secret
    }
    pub fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
    fn new(secret: String, expires_at_unix_ms: u64) -> Result<Self, Rejected> {
        if secret.is_empty()
            || secret.len() > 8192
            || secret.bytes().any(|b| !(0x21..=0x7e).contains(&b))
        {
            return Err(Rejected);
        }
        Ok(Self {
            secret,
            expires_at_unix_ms,
        })
    }
}
impl LeaseRef {
    pub fn validate(&self) -> Result<(), Rejected> {
        self.principal.validate()?;
        if !self.path.is_absolute() || self.path.as_os_str().len() > 4096 {
            return Err(Rejected);
        }
        Ok(())
    }
    pub fn resolve(&self, audience: &str, now: u64) -> Result<Credential, Rejected> {
        self.validate()?;
        let bytes = read_private(&self.path, 32_768)?;
        let e: Envelope = serde_json::from_slice(&bytes).map_err(|_| Rejected)?;
        if e.schema_version != 1
            || e.principal != self.principal
            || e.audience != audience
            || e.not_before_unix_ms == 0
            || now < e.not_before_unix_ms
            || now >= e.expires_at_unix_ms
            || e.expires_at_unix_ms
                .checked_sub(e.not_before_unix_ms)
                .is_none_or(|n| n == 0 || n > MAX_LEASE_MS)
        {
            return Err(Rejected);
        }
        Credential::new(e.secret, e.expires_at_unix_ms)
    }
}
/// Exactly one source is configured. Environment references remain available
/// for trusted local execution and explicitly enabled loopback fixtures.
pub fn validate_sources(environment: &str, lease: Option<&LeaseRef>) -> Result<(), Rejected> {
    if let Some(lease) = lease {
        if !environment.is_empty() {
            return Err(Rejected);
        }
        lease.validate()
    } else if !environment.is_empty()
        && environment.len() <= 128
        && environment
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        Ok(())
    } else {
        Err(Rejected)
    }
}
pub fn resolve(
    environment: &str,
    lease: Option<&LeaseRef>,
    audience: &str,
    now: u64,
) -> Result<Credential, Rejected> {
    validate_sources(environment, lease)?;
    match lease {
        Some(lease) => lease.resolve(audience, now),
        None => Credential::new(std::env::var(environment).map_err(|_| Rejected)?, u64::MAX),
    }
}
pub fn read_private(path: &std::path::Path, limit: usize) -> Result<Vec<u8>, Rejected> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| Rejected)?;
        let m = f.metadata().map_err(|_| Rejected)?;
        // SAFETY: geteuid has no pointer arguments or side effects.
        if !m.is_file()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
            || m.nlink() != 1
        {
            return Err(Rejected);
        }
        let mut bytes = vec![];
        f.take(limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Rejected)?;
        if bytes.len() > limit {
            return Err(Rejected);
        }
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, limit);
        Err(Rejected)
    }
}

/// Reject known invocation credentials in retained structured data, including
/// decoded JSON escapes, object keys and byte arrays. This is not general DLP.
pub fn reflects<T: Serialize>(value: &T, secret: &str) -> bool {
    fn visit(value: &Value, secret: &str) -> bool {
        match value {
            Value::String(s) => s.contains(secret),
            Value::Array(a) => {
                a.iter().any(|v| visit(v, secret)) || {
                    let bytes: Option<Vec<u8>> = a
                        .iter()
                        .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                        .collect();
                    bytes.is_some_and(|b| b.windows(secret.len()).any(|w| w == secret.as_bytes()))
                }
            }
            Value::Object(o) => o
                .iter()
                .any(|(k, v)| k.contains(secret) || visit(v, secret)),
            _ => false,
        }
    }
    if secret.is_empty() {
        return true;
    }
    serde_json::to_value(value).map_or(true, |v| visit(&v, secret))
}

#[cfg(test)]
mod tests;
