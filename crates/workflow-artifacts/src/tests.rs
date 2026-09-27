use super::*;
#[test]
fn references_bind_payload_type_and_producer_and_strict_json_rejects_duplicates() {
    let ty = ArtifactType {
        identity: workflow_ir::VersionRef {
            id: "report".into(),
            version: "1.0.0".into(),
        },
        content: ContentSchema::Json {
            value_type: workflow_ir::ValueType::Boolean,
        },
    };
    let mut spec = PublishSpec {
        schema_version: 1,
        artifact_type: ty,
        producer: Producer {
            run_id: "r".into(),
            node_instance_id: "n".into(),
            attempt_id: "a".into(),
            request_digest: content_digest(b"request"),
            input_digest: content_digest(b"input"),
        },
        source_revision: SourceRevision {
            repository: "repo".into(),
            revision: "a".repeat(40),
        },
        inputs: vec![],
        access: AccessScope::Run { run_id: "r".into() },
        retention: Retention::RunDependency,
    };
    let r = reference(&spec, b"true").unwrap();
    validate_ref(&r).unwrap();
    assert!(reference(&spec, b"1").is_err());
    spec.producer.attempt_id = "b".into();
    assert_ne!(
        reference(&spec, b"true").unwrap().artifact_id,
        r.artifact_id
    );
    assert!(parse_message::<serde_json::Value>(br#"{"x":{"a":1,"a":2}}"#).is_err());
    let mut bad = r;
    bad.manifest.spec.source_revision.revision = "b".repeat(40);
    assert!(validate_ref(&bad).is_err());
    // Rust callers can construct shapes deeper than the JSON decoder permits.
    // Reject them through the explicit type budget before serializing the spec.
    let mut deep = workflow_ir::ValueType::Boolean;
    for _ in 0..200 {
        deep = workflow_ir::ValueType::Array {
            items: Box::new(deep),
        };
    }
    spec.artifact_type.content = ContentSchema::Json { value_type: deep };
    assert_eq!(validate_spec(&spec).unwrap_err().code, ErrorCode::Budget);
}
