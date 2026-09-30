use crate::*;
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientBinding {
    pub endpoint: String,
    pub ca_file: PathBuf,
    pub credential: SecretRef,
    pub timeout_ms: u64,
}
pub struct RemoteClient {
    client: reqwest::blocking::Client,
    binding: ClientBinding,
}
impl RemoteClient {
    pub fn new(binding: ClientBinding) -> Result<Self> {
        let url = reqwest::Url::parse(&binding.endpoint).map_err(|_| invalid())?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/v1/operations"
            || !(1..=120000).contains(&binding.timeout_ms)
        {
            return Err(invalid());
        }
        let tls = rustls::ClientConfig::builder_with_provider(binding::provider())
            .with_safe_default_protocol_versions()
            .map_err(|_| invalid())?
            .with_root_certificates(binding::roots(&binding.ca_file)?)
            .with_no_client_auth();
        let client = reqwest::blocking::Client::builder()
            .use_preconfigured_tls(tls)
            .https_only(true)
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_millis(binding.timeout_ms))
            .build()
            .map_err(|_| invalid())?;
        Ok(Self { client, binding })
    }
    /// Read the credential reference for each call so an atomic file replacement
    /// can rotate credentials without changing the workflow or recreating client.
    /// No automatic replay: an unavailable reply may follow a committed mutation.
    pub fn call(&self, request: &Request) -> Result<Response> {
        let bytes = transport::encode_request(request)?;
        let secret = self.binding.credential.resolve()?;
        transport::validate_credential(&secret)?;
        let reply = self
            .client
            .post(&self.binding.endpoint)
            .bearer_auth(secret.expose())
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .map_err(|_| unavailable())?;
        let status = reply.status();
        if status.is_redirection()
            || reply
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(invalid());
        }
        let mut bytes = vec![];
        reply
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(invalid());
        }
        let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        transport::accept_reply(request, reply, status.is_success())
    }
}
