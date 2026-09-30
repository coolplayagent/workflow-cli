use super::*;

/// One PostgreSQL statement snapshot of the complete scope audit. There is no
/// live pagination loop that includes its own newly appended export audit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditExport {
    pub schema_version: u32,
    pub tenant: String,
    pub project: String,
    pub exported_by: String,
    pub entries: Vec<AuditEntry>,
    pub digest: String,
}
impl AuditExport {
    fn content_digest(&self) -> Result<String> {
        let value = serde_json::to_value((
            self.schema_version,
            &self.tenant,
            &self.project,
            &self.exported_by,
            &self.entries,
        ))
        .map_err(|_| corrupt("audit export encoding failed"))?;
        let bytes =
            serde_json::to_vec(&value).map_err(|_| corrupt("audit export encoding failed"))?;
        if bytes.len() > 8_388_608 {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "audit export exceeds bounded archive capacity",
            ));
        }
        Ok(hash(&bytes))
    }
    pub fn verify(&self) -> Result<()> {
        for id in [&self.tenant, &self.project, &self.exported_by] {
            validate_id(id)?;
        }
        if self.schema_version != 1
            || self.entries.len() > 10_000
            || self
                .entries
                .iter()
                .any(|e| e.sequence <= 0 || e.at_unix_ms <= 0)
            || self
                .entries
                .windows(2)
                .any(|e| e[0].sequence >= e[1].sequence)
            || self.content_digest()? != self.digest
        {
            return Err(corrupt("audit export content differs"));
        }
        Ok(())
    }
}
impl AuthenticatedService {
    /// Export is limited to 10,000 entries; exceeding the limit rejects the
    /// entire export rather than silently delivering a partial archive.
    pub fn export_audit(&mut self, token: &str) -> Result<AuditExport> {
        self.transact(token, &[Role::Administrator,Role::Recovery], "export_audit", "audit", |tx,who| {
            let rows=tx.query("SELECT sequence,actor,credential_id,operation,resource,outcome,at_unix_ms FROM workflow_access.audit WHERE tenant=$1 AND project=$2 ORDER BY sequence LIMIT 10001", &[&who.tenant,&who.project]).map_err(storage)?;
            if rows.len() > 10_000 { return Err(Error::new(ErrorCode::InvalidRequest,"audit export exceeds bounded archive capacity")); }
            let entries=rows.iter().map(|r| AuditEntry{sequence:r.get(0),actor:r.get(1),credential_id:r.get(2),operation:r.get(3),resource:r.get(4),outcome:r.get(5),at_unix_ms:r.get(6)}).collect();
            let mut report=AuditExport{schema_version:1,tenant:who.tenant.clone(),project:who.project.clone(),exported_by:who.actor.clone(),entries,digest:String::new()};
            report.digest=report.content_digest()?;
            report.verify()?;
            Ok(report)
        })
    }
}
