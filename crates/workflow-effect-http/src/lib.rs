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
    /// Optional host-owned checkout. Checked by bytes immediately before writes;
    /// the release must explicitly acknowledge the non-atomic workspace race.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<ReleaseWorkspace>,
    pub schema_version: u32,
    pub target: VersionRef,
    pub call_identity: VersionRef,
    pub capability: CapabilityDescriptor,
    pub endpoint: String,
    /// Environment variable name only; the value is never serialized or logged.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<workflow_credentials::LeaseRef>,
    #[serde(default)]
    pub allow_loopback_http: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseWorkspace {
    pub repository: String,
    pub path: std::path::PathBuf,
}
impl HttpEffectBinding {
    /// Shared production execution requires a broker-issued lease. The only
    /// environment-key exception is an explicitly enabled literal loopback fixture.
    pub fn shared_principal(&self) -> Result<Option<&workflow_credentials::Principal>> {
        workflow_credentials::validate_sources(&self.api_key_env, self.credential.as_ref())
            .map_err(|_| invalid())?;
        if let Some(lease) = &self.credential {
            return Ok(Some(&lease.principal));
        }
        let url = Url::parse(&self.endpoint).map_err(|_| invalid())?;
        if self.allow_loopback_http
            && url.scheme() == "http"
            && url
                .host_str()
                .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
                .is_some_and(|a| a.is_loopback())
        {
            Ok(None)
        } else {
            Err(invalid())
        }
    }
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
            || binding
                .workspace
                .as_ref()
                .is_some_and(|w| !w.path.is_absolute() || w.repository.trim().is_empty())
            || binding.endpoint.len() > 2048
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (binding.allow_loopback_http && loopback && url.scheme() == "http"))
            || workflow_credentials::validate_sources(
                &binding.api_key_env,
                binding.credential.as_ref(),
            )
            .is_err()
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
        self.execute_with_credential(attempt, clock, key, u64::MAX)
    }
    fn execute_with_credential(
        &self,
        attempt: &EffectAttempt,
        clock: &dyn Clock,
        key: &str,
        credential_expires: u64,
    ) -> Result<Observation> {
        attempt.validate()?;
        if attempt.kind == CallKind::Write
            && let Some(workspace) = &self.binding.workspace
        {
            let release = attempt.intent.release.as_ref().ok_or_else(invalid)?;
            if !matches!(
                release.policy.target_check,
                TargetCheck::ObserveThenReconcile { .. }
            ) || workspace.repository != release.subject.source_revision.repository
            {
                return Err(invalid());
            }
            workflow_workspace_local::GitSource::open(&workspace.repository, &workspace.path)
                .and_then(|source| source.verify_current_worktree(&release.subject.source_revision))
                .map_err(|_| invalid())?;
        }
        let now = clock.now_unix_ms()?;
        if attempt.intent.policy.target != self.binding.target
            || attempt.intent.policy.call_identity != self.binding.call_identity
            || attempt.intent.capability != self.binding.capability
            || now < attempt.issued_at_unix_ms
            || now >= attempt.deadline_unix_ms
            || now >= credential_expires
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
                (attempt.deadline_unix_ms.min(credential_expires) - now).min(60_000),
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
        if workflow_credentials::reflects(&reply, key) || reply.request_digest != digest(attempt)? {
            return Ok(unknown());
        }
        let observed = reply.observation;
        if let Observation::Applied { receipt } = &observed
            && receipt.validate(&attempt.intent).is_err()
        {
            return Ok(unknown());
        }
        // Receipt validation and request kind/error-code checks are repeated by
        // the durable authority. Late, truthful receipts are retained under a live lease.
        Ok(observed)
    }
}
impl EffectAdapter for HttpEffect {
    fn execute(&self, attempt: &EffectAttempt, clock: &dyn Clock) -> Result<Observation> {
        let now = clock.now_unix_ms()?;
        let key = workflow_credentials::resolve(
            &self.binding.api_key_env,
            self.binding.credential.as_ref(),
            &self.binding.endpoint,
            now,
        )
        .map_err(|_| invalid())?;
        self.execute_with_credential(attempt, clock, key.expose(), key.expires_at_unix_ms())
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
