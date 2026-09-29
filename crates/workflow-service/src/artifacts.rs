use crate::*;
use workflow_artifacts::{ArtifactLink, ArtifactRef};
use workflow_runstore_postgres::access::{ARTIFACT_CHUNK_BYTES, ArtifactUploadRequest};

fn call(client: &RemoteClient, operation: Operation) -> Result<Response> {
    client.call(&Request {
        protocol_version: PROTOCOL_VERSION,
        request_id: "artifact-transfer".into(),
        operation,
    })
}
fn published(
    reference: ArtifactRef,
    request: &ArtifactUploadRequest,
    content: &[u8],
) -> Result<ArtifactRef> {
    if reference.manifest.spec.artifact_type != request.artifact_type
        || workflow_artifacts::reference(&reference.manifest.spec, content)
            .map_err(|_| invalid())?
            != reference
    {
        return Err(invalid());
    }
    Ok(reference)
}
impl RemoteClient {
    /// A caller can explicitly retry this operation with the same upload request
    /// ID and bytes. The begin response reconciles already committed chunks.
    /// Transport failures return immediately; no mutation is blindly replayed.
    pub fn upload_artifact(
        &self,
        request: &ArtifactUploadRequest,
        content: &[u8],
    ) -> Result<ArtifactRef> {
        if content.len() as u64 > workflow_artifacts::MAX_CONTENT_BYTES
            || content.len() as u64 != request.bytes
            || workflow_artifacts::content_digest(content) != request.content_digest
        {
            return Err(invalid());
        }
        workflow_artifacts::validate_content(&request.artifact_type, content)
            .map_err(|_| invalid())?;
        let Response::ArtifactUpload(mut progress) = call(
            self,
            Operation::ArtifactBegin {
                request: Box::new(request.clone()),
            },
        )?
        else {
            return Err(invalid());
        };
        if progress.expected_bytes != request.bytes || progress.received_bytes > request.bytes {
            return Err(invalid());
        }
        if let Some(done) = progress.completed {
            return published(done, request, content);
        }
        let id = progress.upload_id.clone();
        let mut offset = progress.received_bytes as usize;
        if offset != content.len() && !offset.is_multiple_of(ARTIFACT_CHUNK_BYTES) {
            return Err(invalid());
        }
        while offset < content.len() {
            let end = (offset + ARTIFACT_CHUNK_BYTES).min(content.len());
            let Response::ArtifactUpload(next) = call(
                self,
                Operation::ArtifactPut {
                    upload_id: id.clone(),
                    offset: offset as u64,
                    content: content[offset..end].to_vec(),
                },
            )?
            else {
                return Err(invalid());
            };
            if next.upload_id != id
                || next.expected_bytes != request.bytes
                || next.received_bytes < end as u64
                || next.received_bytes > request.bytes
            {
                return Err(invalid());
            }
            if let Some(done) = next.completed {
                return published(done, request, content);
            }
            if next.received_bytes != request.bytes
                && !next
                    .received_bytes
                    .is_multiple_of(ARTIFACT_CHUNK_BYTES as u64)
            {
                return Err(invalid());
            }
            progress = next;
            offset = progress.received_bytes as usize;
        }
        let Response::Artifact(done) = call(self, Operation::ArtifactComplete { upload_id: id })?
        else {
            return Err(invalid());
        };
        published(*done, request, content)
    }

    /// Every chunk requires this client's bearer plus the credential-bound,
    /// expiring download grant. Verify the complete manifest/type/content again
    /// before returning bytes to a consumer or writing a local output file.
    pub fn download_artifact(
        &self,
        link: &ArtifactLink,
        assignment: Option<&str>,
        ttl_ms: u64,
    ) -> Result<(ArtifactRef, Vec<u8>)> {
        workflow_artifacts::validate_link(link).map_err(|_| invalid())?;
        let Response::ArtifactGrant(grant) = call(
            self,
            Operation::ArtifactGrant {
                artifact: link.clone(),
                assignment_id: assignment.map(str::to_owned),
                ttl_ms,
            },
        )?
        else {
            return Err(invalid());
        };
        workflow_artifacts::validate_ref(&grant.artifact).map_err(|_| invalid())?;
        if grant.artifact.link() != *link || grant.chunk_bytes != ARTIFACT_CHUNK_BYTES as u32 {
            return Err(invalid());
        }
        let expected = grant.artifact.manifest.bytes as usize;
        let mut content = Vec::with_capacity(expected);
        while content.len() < expected {
            let offset = content.len();
            let Response::ArtifactChunk(chunk) = call(
                self,
                Operation::ArtifactGet {
                    download_id: grant.download_id.clone(),
                    offset: offset as u64,
                },
            )?
            else {
                return Err(invalid());
            };
            let end = (offset + ARTIFACT_CHUNK_BYTES).min(expected);
            if chunk.offset != offset as u64
                || chunk.content.len() != end - offset
                || chunk.next_offset != (end < expected).then_some(end as u64)
            {
                return Err(invalid());
            }
            content.extend_from_slice(&chunk.content);
        }
        if workflow_artifacts::reference(&grant.artifact.manifest.spec, &content)
            .map_err(|_| invalid())?
            != grant.artifact
        {
            return Err(invalid());
        }
        Ok((grant.artifact, content))
    }
}
