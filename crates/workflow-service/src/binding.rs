use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    File { path: PathBuf },
    Environment { name: String },
}
pub struct Secret(String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl SecretRef {
    pub fn resolve(&self) -> Result<Secret> {
        let value = match self {
            Self::File { path } => {
                String::from_utf8(read_private(path, 65536)?).map_err(|_| invalid())?
            }
            Self::Environment { name } => {
                if name.is_empty()
                    || name.len() > 128
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err(invalid());
                }
                std::env::var(name).map_err(|_| invalid())?
            }
        };
        if value.is_empty() || value.len() > 65536 {
            return Err(invalid());
        }
        Ok(Secret(value.trim_end_matches(['\r', '\n']).into()))
    }
}
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    read_file(std::fs::File::open(path).map_err(|_| invalid())?, limit)
}
fn read_file(file: std::fs::File, limit: usize) -> Result<Vec<u8>> {
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    let mut bytes = vec![];
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() > limit {
        return Err(invalid());
    }
    Ok(bytes)
}
pub fn read_private(path: &Path, limit: usize) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| invalid())?;
        let meta = file.metadata().map_err(|_| invalid())?;
        // SAFETY: geteuid has no pointer arguments or side effects.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 || meta.nlink() != 1
        {
            return Err(invalid());
        }
        read_file(file, limit)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, limit);
        Err(invalid())
    }
}
/// Exclusive private output for freshly issued credentials. No stdout serialization.
pub fn write_credential(
    path: &Path,
    credential: &workflow_runstore_postgres::access::IssuedCredential,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| invalid())?;
        f.write_all(credential.expose_secret().as_bytes())
            .map_err(|_| unavailable())?;
        f.sync_all().map_err(|_| unavailable())?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })
            .and_then(|f| f.sync_all())
            .map_err(|_| unavailable())?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, credential);
        Err(invalid())
    }
}
pub(crate) fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}
pub(crate) fn roots(path: &Path) -> Result<rustls::RootCertStore> {
    let pem = read_bounded(path, 1048576)?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    if certs.is_empty() {
        return Err(invalid());
    }
    let mut roots = rustls::RootCertStore::empty();
    for c in certs {
        roots.add(c).map_err(|_| invalid())?;
    }
    Ok(roots)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseBinding {
    pub connection: SecretRef,
    pub transport: DatabaseTransport,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DatabaseTransport {
    Tls { ca_file: PathBuf },
    Local,
}
impl DatabaseBinding {
    pub fn connect(&self) -> Result<postgres::Client> {
        let secret = self.connection.resolve()?;
        let mut config: postgres::Config = secret.expose().parse().map_err(|_| invalid())?;
        config.connect_timeout(Duration::from_secs(5));
        match &self.transport {
            DatabaseTransport::Tls { ca_file } => {
                config.ssl_mode(postgres::config::SslMode::Require);
                let tls = rustls::ClientConfig::builder_with_provider(provider())
                    .with_safe_default_protocol_versions()
                    .map_err(|_| invalid())?
                    .with_root_certificates(roots(ca_file)?)
                    .with_no_client_auth();
                config
                    .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
                    .map_err(|_| unavailable())
            }
            DatabaseTransport::Local => {
                if config.get_hosts().is_empty()
                    || config.get_hosts().iter().any(|h| match h {
                        postgres::config::Host::Tcp(host) => !host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback()),
                        #[cfg(unix)]
                        postgres::config::Host::Unix(_) => false,
                    })
                    || config.get_hostaddrs().iter().any(|a| !a.is_loopback())
                {
                    return Err(invalid());
                }
                config.ssl_mode(postgres::config::SslMode::Disable);
                config.connect(postgres::NoTls).map_err(|_| unavailable())
            }
        }
    }
}
