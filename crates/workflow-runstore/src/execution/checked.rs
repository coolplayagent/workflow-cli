//! Single-run compare-and-acquire. Only AdmissionConflict proves a rejected
//! precondition with no write; transport or commit failures may be ambiguous.
use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcquireCheckedV1 {
    pub schema_version: u32,
    pub lease: LeaseRequest,
    pub expected_run_digest: String,
    pub expected_bundle_digest: String,
    pub expected_snapshot_digest: String,
    pub expected_execution_digest: String,
    pub expected_snapshot_revision: u64,
}

impl AcquireCheckedV1 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.expected_snapshot_revision == 0
            || !(1..=MAX_LEASE_MS).contains(&self.lease.ttl_ms)
            || [
                &self.expected_run_digest,
                &self.expected_bundle_digest,
                &self.expected_snapshot_digest,
                &self.expected_execution_digest,
            ]
            .iter()
            .any(|digest| {
                digest.len() != 71
                    || !digest.starts_with("sha256:")
                    || !digest.as_bytes()[7..]
                        .iter()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            })
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "checked acquire requires schema 1, positive revision, valid TTL and SHA-256 digests",
            ));
        }
        for id in [
            &self.lease.run_id,
            &self.lease.owner,
            &self.lease.acquisition_id,
        ] {
            validate_id(id)?;
        }
        Ok(())
    }

    /// Call only with recovered snapshot and validated execution records inside
    /// the same write transaction that will append Acquired. This does not grant
    /// authority outside that transaction, or replace the normal lease checks.
    pub fn check_baseline(&self, snapshot: &Snapshot, records: &[ExecutionRecord]) -> Result<()> {
        self.validate()?;
        if self.lease.run_id != snapshot.run_id
            || self.expected_run_digest != snapshot.run_digest
            || self.expected_bundle_digest != snapshot.bundle_digest
            || self.expected_snapshot_revision != snapshot.revision
            || self.expected_snapshot_digest != workflow_worker::digest(snapshot)?
            || self.expected_execution_digest != workflow_worker::digest(&records)?
        {
            return Err(Error::new(
                ErrorCode::AdmissionConflict,
                "checked acquire baseline does not match current run state",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AcquireCheckedV1 {
        AcquireCheckedV1 {
            schema_version: 1,
            lease: LeaseRequest {
                run_id: "test-run".into(),
                owner: "test-owner".into(),
                acquisition_id: "test-acquisition".into(),
                ttl_ms: 100,
            },
            expected_run_digest: format!("sha256:{}", "a".repeat(64)),
            expected_bundle_digest: format!("sha256:{}", "b".repeat(64)),
            expected_snapshot_digest: format!("sha256:{}", "c".repeat(64)),
            expected_execution_digest: format!("sha256:{}", "d".repeat(64)),
            expected_snapshot_revision: 1,
        }
    }

    #[test]
    fn checked_acquire_requires_every_field_and_rejects_unknown_and_duplicate_keys() {
        let original = serde_json::to_value(request()).unwrap();
        for field in original.as_object().unwrap().keys() {
            let mut value = original.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                workflow_worker::parse_message::<AcquireCheckedV1>(
                    &serde_json::to_vec(&value).unwrap()
                )
                .is_err(),
                "missing {field}"
            );
        }
        let mut unknown = original.clone();
        unknown["unchecked"] = serde_json::json!(true);
        assert!(serde_json::from_value::<AcquireCheckedV1>(unknown).is_err());
        let duplicate =
            serde_json::to_string(&original)
                .unwrap()
                .replacen("{", "{\"schema_version\":1,", 1);
        assert!(workflow_worker::parse_message::<AcquireCheckedV1>(duplicate.as_bytes()).is_err());
        let mut nested = original;
        nested["lease"]["unchecked"] = serde_json::json!(true);
        assert!(serde_json::from_value::<AcquireCheckedV1>(nested).is_err());
    }

    #[test]
    fn checked_acquire_validates_version_revision_lease_and_canonical_digests() {
        request().validate().unwrap();
        for (field, changed) in [
            ("schema_version", serde_json::json!(2)),
            ("expected_snapshot_revision", serde_json::json!(0)),
            ("expected_run_digest", serde_json::json!("invalid")),
            (
                "expected_bundle_digest",
                serde_json::json!(format!("sha256:{}", "A".repeat(64))),
            ),
            ("expected_snapshot_digest", serde_json::json!("sha256:")),
            (
                "expected_execution_digest",
                serde_json::json!(format!("sha256:{}", "g".repeat(64))),
            ),
        ] {
            let mut value = serde_json::to_value(request()).unwrap();
            value[field] = changed;
            let request: AcquireCheckedV1 = serde_json::from_value(value).unwrap();
            assert_eq!(
                request.validate().unwrap_err().code,
                ErrorCode::InvalidRequest
            );
        }
        for ttl in [0, MAX_LEASE_MS + 1] {
            let mut request = request();
            request.lease.ttl_ms = ttl;
            assert_eq!(
                request.validate().unwrap_err().code,
                ErrorCode::InvalidRequest
            );
        }
        let mut request = request();
        request.lease.owner.clear();
        assert_eq!(
            request.validate().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            serde_json::to_value(ErrorCode::AdmissionConflict).unwrap(),
            "admission_conflict"
        );
        assert_eq!(
            serde_json::to_value(ErrorCode::LeaseConflict).unwrap(),
            "lease_conflict"
        );
        assert!(
            crate::schema("acquire-checked-v1")
                .unwrap()
                .contains("expected_execution_digest")
        );
    }
}
