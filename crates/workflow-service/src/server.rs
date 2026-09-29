use crate::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response as HttpResponse},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::{future::Future, io::Write, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet, time::timeout};
use workflow_runstore_postgres::access::AuthenticatedService;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerBinding {
    pub listen: std::net::SocketAddr,
    pub certificate_file: PathBuf,
    pub private_key: SecretRef,
    pub database: DatabaseBinding,
    pub max_connections: u32,
    pub max_operations: u32,
}
#[derive(Clone)]
struct ServerState {
    database: DatabaseBinding,
    operations: Arc<Semaphore>,
}
fn http_error(code: StatusCode) -> HttpResponse {
    (
        code,
        [(header::CACHE_CONTROL, "no-store")],
        "request rejected",
    )
        .into_response()
}
fn status(result: &Result<Response>) -> StatusCode {
    match result {
        Ok(_) => StatusCode::OK,
        Err(e) => match e.code {
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Storage | ErrorCode::Busy => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
            _ => StatusCode::CONFLICT,
        },
    }
}
struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + bytes.len() > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::other("response limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn operation(
    State(state): State<ServerState>,
    request: axum::extract::Request,
) -> HttpResponse {
    if request.uri().query().is_some() {
        return http_error(StatusCode::BAD_REQUEST);
    }
    let (parts, body) = request.into_parts();
    let mut auth = parts.headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = auth
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return http_error(StatusCode::UNAUTHORIZED);
    };
    if auth.next().is_some()
        || value.len() != 68
        || !value.starts_with("wf1_")
        || !value[4..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return http_error(StatusCode::UNAUTHORIZED);
    }
    let token = value.to_owned();
    if parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        != Some("application/json")
    {
        return http_error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let Ok(permit) = state.operations.clone().try_acquire_owned() else {
        return http_error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let bytes = match timeout(Duration::from_secs(5), to_bytes(body, MAX_REQUEST_BYTES)).await {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return http_error(StatusCode::PAYLOAD_TOO_LARGE),
        Err(_) => return http_error(StatusCode::REQUEST_TIMEOUT),
    };
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => return http_error(StatusCode::BAD_REQUEST),
    };
    if request.validate().is_err() {
        return http_error(StatusCode::BAD_REQUEST);
    }
    // The permit moves into the blocking operation. Dropping the HTTP connection
    // cannot free capacity while an admitted database mutation is still running.
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = state
            .database
            .connect()
            .and_then(AuthenticatedService::open)
            .and_then(|mut s| request.execute(&mut s, &token))
            .map_err(|e| Error::new(e.code, "service operation rejected"));
        let code = status(&result);
        let reply = Reply {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            result,
        };
        let mut bytes = Bounded(vec![]);
        serde_json::to_writer(&mut bytes, &reply).map_err(|_| unavailable())?;
        Ok::<_, Error>((code, bytes.0))
    });
    match timeout(Duration::from_secs(45), work).await {
        Ok(Ok(Ok((code, bytes)))) => (
            code,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from(bytes),
        )
            .into_response(),
        _ => http_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

/// Serve HTTP/1.1 only over verified TLS. The caller owns listener creation and
/// shutdown; connection and operation counts are bounded independently. Unknown
/// URLs/methods have no database access and no generic storage RPC escape hatch.
pub async fn serve(
    listener: TcpListener,
    binding: ServerBinding,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if !(1..=256).contains(&binding.max_connections)
        || !(1..=32).contains(&binding.max_operations)
        || binding.max_operations > binding.max_connections
    {
        return Err(invalid());
    }
    let cert = binding::read_bounded(&binding.certificate_file, 1048576)?;
    let certs = rustls_pemfile::certs(&mut cert.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    let key = binding.private_key.resolve()?;
    let key = rustls_pemfile::private_key(&mut key.expose().as_bytes())
        .map_err(|_| invalid())?
        .ok_or_else(invalid)?;
    let mut tls = rustls::ServerConfig::builder_with_provider(binding::provider())
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| invalid())?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let operations = Arc::new(Semaphore::new(binding.max_operations as usize));
    let app = Router::new()
        .route("/v1/operations", post(operation))
        .fallback(|| async { http_error(StatusCode::NOT_FOUND) })
        .with_state(ServerState {
            database: binding.database,
            operations: operations.clone(),
        });
    let slots = Arc::new(Semaphore::new(binding.max_connections as usize));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        // Reserve a connection slot before accept, including TLS handshake time.
        let permit = tokio::select! {_=&mut shutdown=>break,p=slots.clone().acquire_owned()=>p.map_err(|_|unavailable())?};
        let socket = tokio::select! {_=&mut shutdown=>break,r=listener.accept()=>r.map_err(|_|unavailable())?.0};
        while connections.try_join_next().is_some() {}
        let acceptor = acceptor.clone();
        let app = app.clone();
        connections.spawn(async move {
            let _permit = permit;
            let Ok(Ok(tls)) = timeout(Duration::from_secs(5), acceptor.accept(socket)).await else {
                return;
            };
            let mut http = hyper::server::conn::http1::Builder::new();
            http.keep_alive(false)
                .max_buf_size(32768)
                .max_headers(32)
                .header_read_timeout(Duration::from_secs(5))
                .timer(hyper_util::rt::TokioTimer::new());
            let _ = timeout(
                Duration::from_secs(55),
                http.serve_connection(
                    hyper_util::rt::TokioIo::new(tls),
                    hyper_util::service::TowerToHyperService::new(app),
                ),
            )
            .await;
        });
    }
    drop(listener);
    // Stop admission, then drain bounded connections and any detached DB work.
    timeout(Duration::from_secs(60), async {
        while connections.join_next().await.is_some() {}
        let _all = operations
            .acquire_many(binding.max_operations)
            .await
            .map_err(|_| unavailable())?;
        Ok::<_, Error>(())
    })
    .await
    .map_err(|_| unavailable())??;
    Ok(())
}

pub fn serve_foreground(
    binding: ServerBinding,
    ready: impl FnOnce(std::net::SocketAddr) -> Result<()>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(32)
        .enable_all()
        .build()
        .map_err(|_| unavailable())?;
    let result = runtime.block_on(async {
        let listener = TcpListener::bind(binding.listen)
            .await
            .map_err(|_| unavailable())?;
        ready(listener.local_addr().map_err(|_| unavailable())?)?;
        serve(listener, binding, async {
            #[cfg(unix)]
            {
                if let Ok(mut term) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_ = tokio::signal::ctrl_c()=>{}, _ = term.recv()=>{}}
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await
    });
    // Runtime::drop otherwise waits without a bound for spawn_blocking tasks,
    // even after serve's drain deadline. An unfinished operation remains an
    // uncertain outcome; shutdown never turns it into acknowledged success.
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
