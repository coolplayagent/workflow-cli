use super::*;
use workflow_artifacts::{ArtifactRef, ArtifactType};

pub const ARTIFACT_CHUNK_BYTES: usize = 65_536;
const MAX_ACTIVE_UPLOADS: i64 = 64;
const MAX_RUN_UPLOADS: i64 = 512;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadRequest {
    pub request_id: String,
    pub assignment_id: String,
    pub artifact_type: ArtifactType,
    pub bytes: u64,
    pub content_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUploadStatus {
    pub upload_id: String,
    pub received_bytes: u64,
    pub expected_bytes: u64,
    pub expires_at_unix_ms: u64,
    pub completed: Option<ArtifactRef>,
}

pub(super) fn assigned(
    tx: &mut Transaction<'_>,
    who: &Identity,
    assignment: &str,
    write: bool,
) -> Result<(tasks::Assignment, ArtifactPolicy)> {
    validate_id(assignment)?;
    let task = tasks::load(tx, who, assignment)?;
    if task.settled {
        return Err(Error::new(
            ErrorCode::ReceiptConflict,
            "assignment already settled",
        ));
    }
    let policy = who
        .capabilities
        .iter()
        .find(|c| c.matches(&task.task.request))
        .and_then(|c| c.artifacts.clone())
        .ok_or_else(denied)?;
    policy.validate()?;
    if write {
        tx.query_one("SELECT run_id FROM workflow_authority.runs WHERE tenant=$1 AND project=$2 AND run_id=$3 FOR UPDATE", &[&who.tenant,&who.project,&task.lease.run_id]).map_err(storage)?;
    }
    let at = now(tx)? as u64;
    who.read(tx, &task.lease.run_id, |store| {
        tasks::check_lease(store, &task.lease, &task.task, at)
    })?;
    Ok((task, policy))
}

fn capacity(tx: &mut Transaction<'_>, who: &Identity, run: &str, bytes: u64) -> Result<()> {
    let stored = tx.query_one("SELECT count(*),COALESCE(sum(octet_length(content)),0)::bigint FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&who.tenant,&who.project,&run]).map_err(storage)?;
    let at = now(tx)?;
    let active = tx.query_one("SELECT count(*) FILTER(WHERE completed_id IS NULL AND expires_at>$4),COALESCE(sum(expected_bytes) FILTER(WHERE completed_id IS NULL AND expires_at>$4),0)::bigint,count(*) FROM workflow_artifacts.uploads WHERE tenant=$1 AND project=$2 AND run_id=$3", &[&who.tenant,&who.project,&run,&at]).map_err(storage)?;
    let count: i64 = stored.get(0);
    let retained: i64 = stored.get(1);
    let uploads: i64 = active.get(0);
    let reserved: i64 = active.get(1);
    let transfers: i64 = active.get(2);
    if count < 0 || retained < 0 || uploads < 0 || reserved < 0 {
        return Err(corrupt("artifact capacity accounting invalid"));
    }
    if transfers >= MAX_RUN_UPLOADS
        || uploads >= MAX_ACTIVE_UPLOADS
        || count + uploads >= artifact_catalog::MAX_RUN_ARTIFACTS as i64
        || (retained as u64)
            .checked_add(reserved as u64)
            .and_then(|n| n.checked_add(bytes))
            .is_none_or(|n| n > artifact_catalog::MAX_RUN_CONTENT_BYTES)
    {
        return Err(Error::new(ErrorCode::Busy, "artifact capacity unavailable"));
    }
    Ok(())
}

fn status(row: &postgres::Row) -> Result<ArtifactUploadStatus> {
    let id: String = row.get(0);
    let document: String = row.get(1);
    let reference: ArtifactRef =
        serde_json::from_str(&document).map_err(|_| corrupt("upload manifest invalid"))?;
    workflow_artifacts::validate_ref(&reference)?;
    let received: i64 = row.get(2);
    let expected: i64 = row.get(3);
    let expires: i64 = row.get(4);
    let completed: Option<String> = row.get(5);
    if received < 0
        || expected < received
        || expected as u64 != reference.manifest.bytes
        || expires <= 0
        || completed
            .as_ref()
            .is_some_and(|id| id != &reference.artifact_id || received != expected)
    {
        return Err(corrupt("upload progress invalid"));
    }
    Ok(ArtifactUploadStatus {
        upload_id: id,
        received_bytes: received as u64,
        expected_bytes: expected as u64,
        expires_at_unix_ms: expires as u64,
        completed: completed.map(|_| reference),
    })
}

fn locked_upload(
    tx: &mut Transaction<'_>,
    who: &Identity,
    id: &str,
) -> Result<(ArtifactUploadStatus, ArtifactRef)> {
    artifact_catalog::check(tx)?;
    // Discover the assignment without a row lock, then use the common ordering:
    // credential -> assignment -> run -> upload. Cleanup can delete an expired
    // upload between reads; the locked lookup must therefore check it again.
    let assignment: String = tx.query_opt("SELECT assignment_id FROM workflow_artifacts.uploads WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND id=$4", &[&who.tenant,&who.project,&who.id,&id]).map_err(storage)?.ok_or_else(denied)?.get(0);
    let (task, policy) = assigned(tx, who, &assignment, true)?;
    let row = tx.query_opt("SELECT id,reference,received_bytes,expected_bytes,expires_at,completed_id,run_id,assignment_id FROM workflow_artifacts.uploads WHERE tenant=$1 AND project=$2 AND worker_id=$3 AND id=$4 FOR UPDATE", &[&who.tenant,&who.project,&who.id,&id]).map_err(storage)?.ok_or_else(denied)?;
    let value = status(&row)?;
    let reference: ArtifactRef =
        serde_json::from_str(row.get(1)).map_err(|_| corrupt("upload manifest invalid"))?;
    if row.get::<_, String>(6) != task.lease.run_id
        || row.get::<_, String>(7) != assignment
        || reference.manifest.spec
            != policy
                .publication_spec(&task.task.request, &reference.manifest.spec.artifact_type)?
    {
        return Err(corrupt("upload differs from assigned task"));
    }
    who.fence(
        task.lease.issued_at_unix_ms as i64,
        value.expires_at_unix_ms as i64,
    );
    live(tx, who)?;
    if value.completed.is_some() {
        let stored: String=tx.query_opt("SELECT reference FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND id=$3 AND run_id=$4", &[&who.tenant,&who.project,&reference.artifact_id,&task.lease.run_id]).map_err(storage)?.ok_or_else(||corrupt("completed upload missing artifact"))?.get(0);
        let artifact: ArtifactRef = serde_json::from_str(&stored)
            .map_err(|_| corrupt("completed upload manifest invalid"))?;
        if artifact != reference {
            return Err(corrupt("completed upload manifest changed"));
        }
    }
    Ok((value, reference))
}

impl AuthenticatedService {
    pub fn put_artifact_chunk(
        &mut self,
        token: &str,
        upload: &str,
        offset: u64,
        content: &[u8],
    ) -> Result<ArtifactUploadStatus> {
        self.transact(token, &[Role::Worker], "artifact_chunk", upload, |tx, who| {
            let (mut progress, reference) = locked_upload(tx, who, upload)?;
            let end = offset.checked_add(content.len() as u64).ok_or_else(denied)?;
            if content.is_empty() || content.len() > ARTIFACT_CHUNK_BYTES
                || !offset.is_multiple_of(ARTIFACT_CHUNK_BYTES as u64)
                || end > progress.expected_bytes
                || (content.len() != ARTIFACT_CHUNK_BYTES && end != progress.expected_bytes)
            { return Err(denied()); }
            if progress.completed.is_some() {
                let saved: Vec<u8>=tx.query_one("SELECT substring(content FROM $4 FOR $5) FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND id=$3", &[&who.tenant,&who.project,&reference.artifact_id,&((offset+1) as i32),&(content.len() as i32)]).map_err(storage)?.get(0);
                if saved != content {return Err(Error::new(ErrorCode::ReceiptConflict,"upload chunk changed"));}
                return Ok(progress);
            }
            if offset < progress.received_bytes {
                let saved: Vec<u8>=tx.query_opt("SELECT content FROM workflow_artifacts.chunks WHERE upload_id=$1 AND byte_offset=$2", &[&upload,&(offset as i64)]).map_err(storage)?.ok_or_else(||corrupt("upload chunk missing"))?.get(0);
                if saved != content {return Err(Error::new(ErrorCode::ReceiptConflict,"upload chunk changed"));}
                return Ok(progress);
            }
            if offset != progress.received_bytes {return Err(Error::new(ErrorCode::ReceiptConflict,"upload chunk out of order"));}
            tx.execute("INSERT INTO workflow_artifacts.chunks(upload_id,byte_offset,content) VALUES($1,$2,$3)", &[&upload,&(offset as i64),&content]).map_err(storage)?;
            tx.execute("UPDATE workflow_artifacts.uploads SET received_bytes=$2 WHERE id=$1", &[&upload,&(end as i64)]).map_err(storage)?;
            progress.received_bytes=end;
            Ok(progress)
        })
    }

    pub fn complete_artifact_upload(&mut self, token: &str, upload: &str) -> Result<ArtifactRef> {
        self.transact(token, &[Role::Worker], "artifact_complete", upload, |tx, who| {
            let (progress,reference)=locked_upload(tx,who,upload)?;
            if let Some(done)=progress.completed {return Ok(done);}
            if progress.received_bytes != progress.expected_bytes {return Err(Error::new(ErrorCode::ReceiptConflict,"upload incomplete"));}
            let rows=tx.query("SELECT byte_offset,content FROM workflow_artifacts.chunks WHERE upload_id=$1 ORDER BY byte_offset LIMIT 1025", &[&upload]).map_err(storage)?;
            if rows.len()>1024 {return Err(corrupt("upload chunk count invalid"));}
            let mut content=Vec::with_capacity(progress.expected_bytes as usize);
            for row in rows {
                let offset:i64=row.get(0);let bytes:Vec<u8>=row.get(1);
                if offset != content.len() as i64 || bytes.is_empty() || bytes.len()>ARTIFACT_CHUNK_BYTES || content.len()+bytes.len()>progress.expected_bytes as usize {return Err(corrupt("upload chunks invalid"));}
                content.extend_from_slice(&bytes);
            }
            if workflow_artifacts::reference(&reference.manifest.spec,&content)? != reference {return Err(Error::new(ErrorCode::ArtifactRejected,"upload content differs from manifest"));}
            let catalog=artifact_catalog::read(tx,who,&reference.manifest.spec.producer.run_id)?.ok_or_else(denied)?;
            for input in &reference.manifest.spec.inputs {workflow_artifacts::ArtifactReader::verify(&catalog,input)?;}
            let document=serde_json::to_string(&reference).map_err(|_|corrupt("artifact manifest invalid"))?;
            let at=now(tx)?;
            tx.execute("INSERT INTO workflow_artifacts.artifacts(tenant,project,run_id,id,reference,content,created_at) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING", &[&who.tenant,&who.project,&reference.manifest.spec.producer.run_id,&reference.artifact_id,&document,&content,&at]).map_err(storage)?;
            tx.execute("UPDATE workflow_artifacts.uploads SET completed_id=$2 WHERE id=$1", &[&upload,&reference.artifact_id]).map_err(storage)?;
            tx.execute("DELETE FROM workflow_artifacts.chunks WHERE upload_id=$1", &[&upload]).map_err(storage)?;
            Ok(reference)
        })
    }

    pub fn begin_artifact_upload(
        &mut self,
        token: &str,
        request: &ArtifactUploadRequest,
    ) -> Result<ArtifactUploadStatus> {
        self.transact(token, &[Role::Worker], "artifact_begin", &request.assignment_id, |tx, who| {
            artifact_catalog::check(tx)?;
            validate_id(&request.request_id)?;
            let (task,policy)=assigned(tx,who,&request.assignment_id,true)?;
            let spec=policy.publication_spec(&task.task.request,&request.artifact_type)?;
            let catalog=artifact_catalog::read(tx,who,&task.lease.run_id)?.ok_or_else(denied)?;
            for (link,expected) in policy.input_links(&task.task.request)? {
                workflow_artifacts::verify_expected(&catalog,&link,&expected)?;
            }
            let reference=workflow_artifacts::from_manifest(workflow_artifacts::Manifest{spec,content_digest:request.content_digest.clone(),bytes:request.bytes})?;
            let document=serde_json::to_string(&reference).map_err(|_| corrupt("upload manifest invalid"))?;
            if let Some(row)=tx.query_opt("SELECT id,reference,received_bytes,expected_bytes,expires_at,completed_id FROM workflow_artifacts.uploads WHERE worker_id=$1 AND assignment_id=$2 AND request_id=$3 FOR UPDATE", &[&who.id,&request.assignment_id,&request.request_id]).map_err(storage)? {
                if row.get::<_,String>(1)!=document {return Err(Error::new(ErrorCode::ReceiptConflict,"upload request changed"));}
                let value=status(&row)?;who.fence(task.lease.issued_at_unix_ms as i64,value.expires_at_unix_ms as i64);return Ok(value);
            }
            let at=now(tx)?;
            tx.execute("DELETE FROM workflow_artifacts.uploads WHERE id IN (SELECT id FROM workflow_artifacts.uploads WHERE tenant=$1 AND project=$2 AND run_id=$3 AND expires_at<=$4 ORDER BY id LIMIT 128 FOR UPDATE SKIP LOCKED)", &[&who.tenant,&who.project,&task.lease.run_id,&at]).map_err(storage)?;
            capacity(tx,who,&task.lease.run_id,request.bytes)?;
            let id=random("upload-")?;
            tx.execute("INSERT INTO workflow_artifacts.uploads(id,tenant,project,run_id,worker_id,assignment_id,request_id,reference,expected_bytes,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)", &[&id,&who.tenant,&who.project,&task.lease.run_id,&who.id,&request.assignment_id,&request.request_id,&document,&(request.bytes as i64),&task.expires]).map_err(storage)?;
            Ok(ArtifactUploadStatus{upload_id:id,received_bytes:0,expected_bytes:request.bytes,expires_at_unix_ms:task.expires as u64,completed:None})
        })
    }
}
