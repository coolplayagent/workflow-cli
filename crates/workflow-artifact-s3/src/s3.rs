use crate::*;
use hmac::{Hmac, Mac};
use reqwest::{Method, Url, blocking::Client};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::BTreeMap, io::Read, time::Duration};

/// Trusted host routing. Endpoint and credentials never become artifact identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Binding {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    #[serde(default)]
    pub allow_http_loopback: bool,
}

pub struct S3Client {
    binding: S3Binding,
    http: Client,
    access: String,
    secret: String,
    token: Option<String>,
}
impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client").finish_non_exhaustive()
    }
}
impl S3Client {
    pub fn new(
        binding: S3Binding,
        access: String,
        secret: String,
        token: Option<String>,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self> {
        let endpoint = Url::parse(&binding.endpoint).map_err(|_| invalid("invalid S3 endpoint"))?;
        let local = endpoint.host_str().is_some_and(|h| {
            h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if !(endpoint.scheme() == "https"
            || endpoint.scheme() == "http" && local && binding.allow_http_loopback)
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/"
            || !(3..=63).contains(&binding.bucket.len())
            || !component(&binding.bucket)
            || binding.bucket.contains('.')
            || !component(&binding.region)
            || binding.region.len() > 64
            || binding.prefix.len() > 256
            || binding.prefix.split('/').any(|s| !component(s))
            || access.is_empty()
            || access.len() > 128
            || !access.bytes().all(|b| b.is_ascii_alphanumeric())
            || secret.is_empty()
            || secret.len() > 4096
            || secret.chars().any(char::is_control)
            || token
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 16384 || s.chars().any(char::is_control))
        {
            return Err(invalid("invalid S3 host binding or credentials"));
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .tls_backend_rustls();
        // Select the same explicit provider as the other HTTP adapters.
        let _ = rustls::crypto::ring::default_provider().install_default();
        if let Some(pem) = ca_pem {
            if pem.len() > 1_048_576 {
                return Err(invalid("S3 CA exceeds budget"));
            }
            builder =
                builder
                    .tls_certs_merge([reqwest::Certificate::from_pem(pem)
                        .map_err(|_| invalid("invalid S3 CA"))?]);
        }
        Ok(Self {
            binding,
            http: builder.build().map_err(|_| unavailable())?,
            access,
            secret,
            token,
        })
    }
    pub(crate) fn identity(&self) -> Result<String> {
        digest(&(
            &self.binding.endpoint,
            &self.binding.region,
            &self.binding.bucket,
            &self.binding.prefix,
        ))
    }
    fn url(&self, key: &str) -> Result<Url> {
        if !key.split('/').all(component) || key.len() > 256 {
            return Err(invalid("invalid object key"));
        }
        Url::parse(&format!(
            "{}/{}/{}/{}",
            self.binding.endpoint.trim_end_matches('/'),
            self.binding.bucket,
            self.binding.prefix,
            key
        ))
        .map_err(|_| invalid("invalid S3 URL"))
    }
    fn request(
        &self,
        method: Method,
        key: &str,
        body: &[u8],
        create: bool,
    ) -> Result<reqwest::blocking::Response> {
        let url = self.url(key)?;
        let at = time::OffsetDateTime::now_utc();
        let stamp = stamp(at);
        let scope = format!("{}/{}/s3/aws4_request", &stamp[..8], self.binding.region);
        let mut headers = BTreeMap::from([
            ("host", host(&url)),
            ("x-amz-content-sha256", content_digest(body)[7..].to_owned()),
            ("x-amz-date", stamp.clone()),
        ]);
        if create {
            headers.insert("if-none-match", "*".into());
        }
        if let Some(token) = &self.token {
            headers.insert("x-amz-security-token", token.clone());
        }
        let names = headers.keys().copied().collect::<Vec<_>>().join(";");
        let canonical = headers
            .iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect::<String>();
        let request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method.as_str(),
            url.path(),
            canonical,
            names,
            &content_digest(body)[7..]
        );
        let signature = self.signature(&stamp, &scope, &request);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
            self.access
        );
        let mut req = self
            .http
            .request(method, url)
            .header("authorization", authorization);
        for (key, value) in headers {
            req = req.header(key, value);
        }
        if create {
            req.body(reqwest::blocking::Body::sized(
                UploadBody(std::io::Cursor::new(body.to_vec())),
                body.len() as u64,
            ))
            .send()
            .map_err(|_| unavailable())
        } else {
            req.body(body.to_vec()).send().map_err(|_| unavailable())
        }
    }
    fn signature(&self, stamp: &str, scope: &str, canonical: &str) -> String {
        let date = mac(
            format!("AWS4{}", self.secret).as_bytes(),
            &stamp.as_bytes()[..8],
        );
        let region = mac(&date, self.binding.region.as_bytes());
        let service = mac(&region, b"s3");
        let key = mac(&service, b"aws4_request");
        hex(&mac(
            &key,
            format!(
                "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
                &content_digest(canonical.as_bytes())[7..]
            )
            .as_bytes(),
        ))
    }
    pub(crate) fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let response = self.request(Method::PUT, key, bytes, true)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(status(response.status()))
        }
    }
    pub(crate) fn get(&self, key: &str) -> Result<Vec<u8>> {
        let response = self.request(Method::GET, key, &[], false)?;
        if !response.status().is_success() {
            return Err(status(response.status()));
        }
        bounded(response)
    }
    pub(crate) fn delete(&self, key: &str) -> Result<()> {
        let response = self.request(Method::DELETE, key, &[], false)?;
        if response.status().is_success() || response.status().as_u16() == 404 {
            Ok(())
        } else {
            Err(status(response.status()))
        }
    }
    pub(crate) fn presign(&self, key: &str, seconds: u32) -> Result<(String, u64)> {
        if !(1..=300).contains(&seconds) {
            return Err(invalid("download lifetime must be 1..300 seconds"));
        }
        let mut url = self.url(key)?;
        let at = time::OffsetDateTime::now_utc();
        let stamp = stamp(at);
        let expires_at_unix_ms = u64::try_from(at.unix_timestamp())
            .map_err(|_| unavailable())?
            .checked_add(u64::from(seconds))
            .and_then(|seconds| seconds.checked_mul(1000))
            .ok_or_else(unavailable)?;
        let scope = format!("{}/{}/s3/aws4_request", &stamp[..8], self.binding.region);
        let mut query = BTreeMap::from([
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".into()),
            ("X-Amz-Credential", format!("{}/{scope}", self.access)),
            ("X-Amz-Date", stamp.clone()),
            ("X-Amz-Expires", seconds.to_string()),
            ("X-Amz-SignedHeaders", "host".into()),
        ]);
        if let Some(token) = &self.token {
            query.insert("X-Amz-Security-Token", token.clone());
        }
        let query = query
            .into_iter()
            .map(|(k, v)| format!("{k}={}", encode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        let canonical = format!(
            "GET\n{}\n{}\nhost:{}\n\nhost\nUNSIGNED-PAYLOAD",
            url.path(),
            query,
            host(&url)
        );
        let signature = self.signature(&stamp, &scope, &canonical);
        url.set_query(Some(&format!("{query}&X-Amz-Signature={signature}")));
        Ok((url.into(), expires_at_unix_ms))
    }
}
fn component(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
struct UploadBody(std::io::Cursor<Vec<u8>>);
impl Read for UploadBody {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.0.read(output)?;
        // Only the fault-test binary contains this process termination point.
        // Sized streaming bodies feed the real HTTP connection incrementally.
        #[cfg(test)]
        if std::env::var("WORKFLOW_OBJECT_CHILD_STAGE").as_deref() == Ok("upload_half")
            && self.0.position() >= self.0.get_ref().len() as u64 / 2
        {
            std::process::exit(86);
        }
        Ok(count)
    }
}
fn stamp(at: time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}
fn host(url: &Url) -> String {
    let host = url.host_str().expect("validated URL");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.into(),
    }
}
fn mac(key: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut h = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts all key sizes");
    h.update(bytes);
    h.finalize().into_bytes().to_vec()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn status(status: reqwest::StatusCode) -> Error {
    Error::new(
        if status.as_u16() == 404 {
            ErrorCode::NotFound
        } else {
            ErrorCode::Storage
        },
        format!("S3 returned HTTP {}", status.as_u16()),
    )
}
fn bounded(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    reader
        .take(MAX_CONTENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 > MAX_CONTENT_BYTES {
        return Err(Error::new(ErrorCode::Budget, "object exceeds 64 MiB"));
    }
    Ok(bytes)
}

/// Worker-side bytes verification against a separately authenticated immutable reference.
/// The URL is a bearer credential; callers must keep it out of logs and durable events.
pub fn download(
    reference: &ArtifactRef,
    url: &str,
    allow_http_loopback: bool,
    ca_pem: Option<&[u8]>,
) -> Result<Vec<u8>> {
    validate_ref(reference)?;
    let target = Url::parse(url).map_err(|_| invalid("invalid download URL"))?;
    let local = target.host_str().is_some_and(|h| {
        h.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if !(target.scheme() == "https" || target.scheme() == "http" && local && allow_http_loopback)
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
    {
        return Err(invalid(
            "download requires HTTPS or explicit loopback fixture transport",
        ));
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .tls_backend_rustls();
    if let Some(pem) = ca_pem {
        client =
            client
                .tls_certs_merge([reqwest::Certificate::from_pem(pem)
                    .map_err(|_| invalid("invalid download CA"))?]);
    }
    let response = client
        .build()
        .map_err(|_| unavailable())?
        .get(target)
        .send()
        .map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(status(response.status()));
    }
    let bytes = bounded(response)?;
    if workflow_artifacts::reference(&reference.manifest.spec, &bytes)? != *reference {
        return Err(corrupt(
            "download content differs from the authenticated reference",
        ));
    }
    Ok(bytes)
}
