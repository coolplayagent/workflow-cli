//! JSON effect gateway adapter. The gateway owns provider-specific idempotency,
//! lookup and credentials. No automatic HTTP retries or redirects are permitted.
use reqwest::{Url, blocking::Client};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};
use workflow_effects::*;
use workflow_ir::VersionRef;
use workflow_worker::{Capability, CapabilityDescriptor, Clock};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpEffectBinding {
    pub schema_version: u32,
    pub target: VersionRef,
    pub call_identity: VersionRef,
    pub capability: CapabilityDescriptor,
    pub endpoint: String,
    /// Environment variable name only; the value is never serialized or logged.
    pub api_key_env: String,
    #[serde(default)]
    pub allow_loopback_http: bool,
}
pub struct HttpEffect {
    binding: HttpEffectBinding,
    client: Client,
}
fn invalid() -> Error {
    Error::new(ErrorCode::InvalidBinding, "invalid effect gateway binding")
}
fn unknown() -> Observation {
    Observation::Unknown {
        reason: "gateway returned no verified effect observation".into(),
    }
}
impl HttpEffect {
    pub fn new(binding: HttpEffectBinding) -> Result<Self> {
        let url = Url::parse(&binding.endpoint).map_err(|_| invalid())?;
        let loopback = url
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|a| a.is_loopback());
        Capability::new(binding.capability.clone())?;
        if binding.schema_version != 1
            || binding.endpoint.len() > 2048
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (binding.allow_loopback_http && loopback && url.scheme() == "http"))
            || binding.api_key_env.is_empty()
            || binding.api_key_env.len() > 128
            || !binding
                .api_key_env
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(invalid());
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| invalid())?;
        Ok(Self { binding, client })
    }
    /// Resolve credentials outside run state. The secret is used only in a
    /// sensitive HTTP header and is excluded from error messages and records.
    pub fn execute_with_secret(
        &self,
        attempt: &EffectAttempt,
        clock: &dyn Clock,
        key: &str,
    ) -> Result<Observation> {
        attempt.validate()?;
        let now = clock.now_unix_ms()?;
        if attempt.intent.policy.target != self.binding.target
            || attempt.intent.policy.call_identity != self.binding.call_identity
            || attempt.intent.capability != self.binding.capability
            || now < attempt.issued_at_unix_ms
            || now >= attempt.deadline_unix_ms
            || (attempt.kind == CallKind::Write && now >= attempt.intent.write_deadline())
            || (attempt.kind == CallKind::Query && !attempt.intent.has_query())
        {
            return Err(invalid());
        }
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| invalid())?;
        authorization.set_sensitive(true);
        if key.is_empty() || key.len() > 8192 {
            return Err(invalid());
        }
        let path = if attempt.kind == CallKind::Write {
            "write"
        } else {
            "query"
        };
        let response = self
            .client
            .post(format!(
                "{}/{path}",
                self.binding.endpoint.trim_end_matches('/')
            ))
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header("Idempotency-Key", &attempt.intent.operation_key)
            .header("X-Effect-Intent", digest(&attempt.intent)?)
            .header("X-Effect-Request", digest(attempt)?)
            .timeout(Duration::from_millis(
                (attempt.deadline_unix_ms - now).min(60_000),
            ))
            .json(attempt)
            .send();
        let Ok(response) = response else {
            return Ok(unknown());
        };
        // Status codes alone cannot establish whether a write happened.
        if !response.status().is_success() {
            return Ok(unknown());
        }
        let mut bytes = vec![];
        if response
            .take(workflow_worker::MAX_MESSAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Ok(unknown());
        }
        let reply: EffectReply = match workflow_worker::parse_message(&bytes) {
            Ok(value) => value,
            Err(_) => return Ok(unknown()),
        };
        if reply.request_digest != digest(attempt)? {
            return Ok(unknown());
        }
        let observed = reply.observation;
        if let Observation::Applied { receipt } = &observed {
            receipt.validate(&attempt.intent)?;
        }
        // Receipt validation and request kind/error-code checks are repeated by
        // the durable authority. Late, truthful receipts are retained under a live lease.
        Ok(observed)
    }
}
impl EffectAdapter for HttpEffect {
    fn execute(&self, attempt: &EffectAttempt, clock: &dyn Clock) -> Result<Observation> {
        let key = std::env::var(&self.binding.api_key_env).map_err(|_| invalid())?;
        self.execute_with_secret(attempt, clock, &key)
    }
}

/// Host routing is explicit and refuses absent or ambiguous bindings.
pub struct HttpEffects(Vec<HttpEffect>);
impl HttpEffects {
    pub fn new(bindings: Vec<HttpEffectBinding>) -> Result<Self> {
        if bindings.is_empty() || bindings.len() > 256 {
            return Err(invalid());
        }
        let mut identities = std::collections::BTreeSet::new();
        let mut adapters = vec![];
        for b in bindings {
            if !identities.insert(digest(&(&b.target, &b.call_identity, &b.capability))?) {
                return Err(invalid());
            }
            adapters.push(HttpEffect::new(b)?);
        }
        Ok(Self(adapters))
    }
}
impl EffectAdapter for HttpEffects {
    fn execute(&self, attempt: &EffectAttempt, clock: &dyn Clock) -> Result<Observation> {
        let i = &attempt.intent;
        self.0
            .iter()
            .find(|a| {
                a.binding.target == i.policy.target
                    && a.binding.call_identity == i.policy.call_identity
                    && a.binding.capability == i.capability
            })
            .ok_or_else(invalid)?
            .execute(attempt, clock)
    }
}

#[cfg(test)]
mod tests;
