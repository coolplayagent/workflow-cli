//! A historical acceptance report, not permission for a future write. The
//! authority creates it from a fully verified consistent snapshot.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    Accepted,
    AcceptedWithExceptions,
    Incomplete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCheck {
    pub event_id: String,
    pub revision: u64,
    pub instance_id: u64,
    pub evaluation: workflow_gates::GateEvaluation,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceManifest {
    pub schema_version: u32,
    pub run_id: String,
    pub run_digest: String,
    pub bundle_digest: String,
    pub revision: u64,
    pub completed_at_unix_ms: u64,
    pub status: AcceptanceStatus,
    pub requirements: Vec<workflow_kernel::Postcondition>,
    pub artifacts: Vec<workflow_artifacts::ArtifactRef>,
    /// Includes failed repair rounds and original FAIL/UNKNOWN under exception.
    pub checks: Vec<AcceptanceCheck>,
    pub approvals: Vec<workflow_kernel::InboxEntry>,
    pub effects: Vec<workflow_effects::EffectState>,
    pub snapshot_digest: String,
    pub execution_digest: String,
    pub digest: String,
}
impl AcceptanceManifest {
    fn content_digest(&self) -> Result<String> {
        let mut value = serde_json::to_value(self)
            .map_err(|_| Error::new(ErrorCode::InvalidRequest, "acceptance encoding failed"))?;
        value.as_object_mut().unwrap().remove("digest");
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| Error::new(ErrorCode::InvalidRequest, "acceptance encoding failed"))?;
        if bytes.len() > 8_388_608 {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "acceptance manifest exceeds 8 MiB; no partial report emitted",
            ));
        }
        Ok(workflow_worker::digest(&value)?)
    }
    pub fn seal(&mut self) -> Result<()> {
        self.digest = self.content_digest()?;
        Ok(())
    }
    /// Integrity only. Trust the scoped authority that emitted the report;
    /// a caller can recompute an unsigned digest and does not gain authority.
    pub fn verify(&self) -> Result<()> {
        if self.schema_version != 1 || self.digest != self.content_digest()? {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "acceptance manifest integrity mismatch",
            ));
        }
        Ok(())
    }
}
