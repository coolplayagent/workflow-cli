use crate::*;
use std::sync::Mutex;
use workflow_runstore_postgres::access::AuthenticatedService;

/// One versioned application operation. Implementations must preserve request
/// bounds, authentication and response matching, and never replay a mutation
/// automatically after an uncertain outcome.
pub trait TaskTransport {
    fn call(&self, request: &Request) -> Result<Response>;
}

impl TaskTransport for RemoteClient {
    fn call(&self, request: &Request) -> Result<Response> {
        RemoteClient::call(self, request)
    }
}

/// In-process composition of the same authenticated application handler used
/// by HTTPS. The credential is resolved on every call to preserve revocation
/// and rotation semantics. This does not bypass the shared authority store.
pub struct InProcessTransport {
    service: Mutex<AuthenticatedService>,
    credential: SecretRef,
}
impl InProcessTransport {
    pub fn new(service: AuthenticatedService, credential: SecretRef) -> Self {
        Self {
            service: Mutex::new(service),
            credential,
        }
    }
}
impl TaskTransport for InProcessTransport {
    fn call(&self, request: &Request) -> Result<Response> {
        encode_request(request)?;
        let secret = self.credential.resolve()?;
        validate_credential(&secret)?;
        let result = request.execute(
            &mut *self.service.lock().map_err(|_| unavailable())?,
            secret.expose(),
        );
        let succeeded = result.is_ok();
        let reply = Reply {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            result,
        };
        let bytes = serde_json::to_vec(&reply).map_err(|_| unavailable())?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(unavailable());
        }
        accept_reply(request, reply, succeeded)
    }
}

pub(crate) fn encode_request(request: &Request) -> Result<Vec<u8>> {
    request.validate()?;
    let bytes = serde_json::to_vec(request).map_err(|_| invalid())?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid());
    }
    Ok(bytes)
}
pub(crate) fn validate_credential(secret: &Secret) -> Result<()> {
    if secret.expose().len() != 68
        || !secret.expose().starts_with("wf1_")
        || !secret.expose()[4..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid());
    }
    Ok(())
}
pub(crate) fn accept_reply(request: &Request, reply: Reply, succeeded: bool) -> Result<Response> {
    if reply.protocol_version != PROTOCOL_VERSION || reply.request_id != request.request_id {
        return Err(invalid());
    }
    match reply.result {
        Ok(response) if succeeded && request.accepts(&response) => Ok(response),
        Err(e) if !succeeded => Err(Error::new(e.code, "application operation rejected")),
        _ => Err(invalid()),
    }
}
