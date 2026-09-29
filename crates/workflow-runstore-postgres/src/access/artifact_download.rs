use super::*;
use workflow_artifacts::{ArtifactLink, ArtifactReader, ArtifactRef};

const READERS: &[Role] = &[
    Role::DefinitionMaintainer,
    Role::Viewer,
    Role::Runner,
    Role::Approver,
    Role::Scheduler,
    Role::Worker,
    Role::Recovery,
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDownloadGrant {
    pub download_id: String,
    pub artifact: ArtifactRef,
    pub expires_at_unix_ms: u64,
    pub chunk_bytes: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDownloadChunk {
    pub offset: u64,
    pub content: Vec<u8>,
    pub next_offset: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCleanup {
    pub uploads: u64,
    pub downloads: u64,
}

fn authorized(
    tx: &mut Transaction<'_>,
    who: &Identity,
    link: &ArtifactLink,
    assignment: Option<&str>,
) -> Result<ArtifactRef> {
    workflow_artifacts::validate_link(link)?;
    let run: String=tx.query_opt("SELECT run_id FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND id=$3", &[&who.tenant,&who.project,&link.artifact_id]).map_err(storage)?.ok_or_else(denied)?.get(0);
    if who.role == Role::Worker.name() {
        let assignment = assignment.ok_or_else(denied)?;
        let (task, policy) = artifact_upload::assigned(tx, who, assignment, false)?;
        if task.lease.run_id != run {
            return Err(denied());
        }
        let catalog = artifact_catalog::read(tx, who, &run)?.ok_or_else(denied)?;
        for (input, ty) in policy.input_links(&task.task.request)? {
            workflow_artifacts::verify_expected(&catalog, &input, &ty)?;
            for item in catalog.closure(&input)? {
                if item.link() == *link {
                    return Ok(item);
                }
            }
        }
        return Err(denied());
    }
    if assignment.is_some() {
        return Err(denied());
    }
    // Lock and verify the run and its evidence before disclosing catalog data.
    who.read(tx, &run, |s| s.get(&run).map(|_| ()))?;
    let catalog = artifact_catalog::read(tx, who, &run)?.ok_or_else(denied)?;
    catalog.verify(link).map_err(Into::into)
}

impl AuthenticatedService {
    pub fn grant_artifact_download(
        &mut self,
        token: &str,
        link: &ArtifactLink,
        assignment: Option<&str>,
        ttl_ms: u64,
    ) -> Result<ArtifactDownloadGrant> {
        self.transact(token,READERS,"artifact_download_grant",&link.artifact_id,|tx,who| {
            artifact_catalog::check(tx)?;
            if !(1..=300_000).contains(&ttl_ms) {return Err(denied());}
            let artifact=authorized(tx,who,link,assignment)?;
            let at=now(tx)?;
            let expires=at.checked_add(ttl_ms as i64).ok_or_else(denied)?.min(who.deadline.get());
            tx.execute("DELETE FROM workflow_artifacts.downloads WHERE credential_id=$1 AND expires_at<=$2", &[&who.id,&at]).map_err(storage)?;
            // Serialize grant reservations for this principal without upgrading
            // the credential lock used for revocation ordering.
            tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended($1,57465234))", &[&who.id]).map_err(storage)?;
            let count:i64=tx.query_one("SELECT count(*) FROM workflow_artifacts.downloads WHERE credential_id=$1", &[&who.id]).map_err(storage)?.get(0);
            if count>=1000 {return Err(Error::new(ErrorCode::Busy,"download grant capacity unavailable"));}
            let id=random("download-")?;
            tx.execute("INSERT INTO workflow_artifacts.downloads(id,tenant,project,credential_id,artifact_id,assignment_id,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)", &[&id,&who.tenant,&who.project,&who.id,&link.artifact_id,&assignment,&expires]).map_err(storage)?;
            who.fence(at,expires);
            Ok(ArtifactDownloadGrant{download_id:id,artifact,expires_at_unix_ms:expires as u64,chunk_bytes:ARTIFACT_CHUNK_BYTES as u32})
        })
    }

    pub fn artifact_download_chunk(
        &mut self,
        token: &str,
        download: &str,
        offset: u64,
    ) -> Result<ArtifactDownloadChunk> {
        self.transact(token,READERS,"artifact_download_chunk",download,|tx,who| {
            artifact_catalog::check(tx)?;
            let row=tx.query_opt("SELECT artifact_id,assignment_id,expires_at FROM workflow_artifacts.downloads WHERE tenant=$1 AND project=$2 AND credential_id=$3 AND id=$4", &[&who.tenant,&who.project,&who.id,&download]).map_err(storage)?.ok_or_else(denied)?;
            let artifact_id:String=row.get(0);let assignment:Option<String>=row.get(1);let expires:i64=row.get(2);
            who.fence(who.issued,expires);live(tx,who)?;
            let suffix=artifact_id.strip_prefix("artifact-").ok_or_else(denied)?;
            let link=ArtifactLink{digest:format!("sha256:{suffix}"),artifact_id};
            let artifact=authorized(tx,who,&link,assignment.as_deref())?;
            // Match upload/grant lock ordering: assignment/run before ticket.
            // The unlocked first lookup is only discovery; verify it again.
            let locked=tx.query_opt("SELECT artifact_id,assignment_id,expires_at FROM workflow_artifacts.downloads WHERE tenant=$1 AND project=$2 AND credential_id=$3 AND id=$4 FOR SHARE", &[&who.tenant,&who.project,&who.id,&download]).map_err(storage)?.ok_or_else(denied)?;
            if locked.get::<_,String>(0)!=link.artifact_id || locked.get::<_,Option<String>>(1)!=assignment || locked.get::<_,i64>(2)!=expires {return Err(corrupt("download grant changed"));}
            live(tx,who)?;
            if offset>artifact.manifest.bytes || (offset != artifact.manifest.bytes && !offset.is_multiple_of(ARTIFACT_CHUNK_BYTES as u64)) {return Err(denied());}
            let content:Vec<u8>=tx.query_one("SELECT substring(content FROM $4 FOR $5) FROM workflow_artifacts.artifacts WHERE tenant=$1 AND project=$2 AND id=$3", &[&who.tenant,&who.project,&link.artifact_id,&((offset+1) as i32),&(ARTIFACT_CHUNK_BYTES as i32)]).map_err(storage)?.get(0);
            let end=offset+content.len() as u64;
            if content.len() as u64 != (artifact.manifest.bytes-offset).min(ARTIFACT_CHUNK_BYTES as u64) {return Err(corrupt("artifact download length changed"));}
            Ok(ArtifactDownloadChunk{offset,content,next_offset:(end<artifact.manifest.bytes).then_some(end)})
        })
    }

    /// Reclaim only expired transfer metadata/chunks. Retained manifests and
    /// committed content are never removed by this operation.
    pub fn cleanup_artifact_transfers(
        &mut self,
        token: &str,
        limit: u32,
    ) -> Result<ArtifactCleanup> {
        self.transact(token,&[Role::Administrator,Role::Recovery],"artifact_cleanup","transfers",|tx,who| {
            artifact_catalog::check(tx)?;validate_limit(limit)?;let at=now(tx)?;
            let uploads=tx.execute("DELETE FROM workflow_artifacts.uploads WHERE id IN (SELECT id FROM workflow_artifacts.uploads WHERE tenant=$1 AND project=$2 AND expires_at<=$3 ORDER BY id LIMIT $4 FOR UPDATE SKIP LOCKED)", &[&who.tenant,&who.project,&at,&i64::from(limit)]).map_err(storage)?;
            let downloads=tx.execute("DELETE FROM workflow_artifacts.downloads WHERE id IN (SELECT id FROM workflow_artifacts.downloads WHERE tenant=$1 AND project=$2 AND expires_at<=$3 ORDER BY id LIMIT $4 FOR UPDATE SKIP LOCKED)", &[&who.tenant,&who.project,&at,&i64::from(limit)]).map_err(storage)?;
            Ok(ArtifactCleanup{uploads,downloads})
        })
    }
}
