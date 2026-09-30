use super::*;
use std::process::Command;

fn fixture(namespace: &str) -> S3ArtifactStore {
    let db = std::env::var("WORKFLOW_TEST_POSTGRES").expect("disposable PostgreSQL required");
    let objects = S3Client::new(
        S3Binding {
            endpoint: std::env::var("WORKFLOW_TEST_S3").expect("disposable S3 endpoint required"),
            region: "us-east-1".into(),
            bucket: "workflow-artifacts".into(),
            prefix: "workflow-tests".into(),
            allow_http_loopback: true,
        },
        "workflowfixture".into(),
        "workflow-fixture-secret".into(),
        None,
        None,
    )
    .unwrap();
    S3ArtifactStore::create(
        Client::connect(&db, postgres::NoTls).unwrap(),
        objects,
        namespace,
    )
    .unwrap()
}
fn spec(name: &str, inputs: Vec<ArtifactLink>) -> PublishSpec {
    PublishSpec {
        schema_version: 1,
        artifact_type: ArtifactType {
            identity: workflow_ir::VersionRef {
                id: "fixture.report".into(),
                version: "1.0.0".into(),
            },
            content: ContentSchema::Utf8,
        },
        producer: Producer {
            run_id: "object-run".into(),
            node_instance_id: name.into(),
            attempt_id: format!("attempt-{name}"),
            request_digest: digest(&name).unwrap(),
            input_digest: digest(&inputs).unwrap(),
        },
        source_revision: SourceRevision {
            repository: "fixture".into(),
            revision: "a".repeat(40),
        },
        inputs,
        access: AccessScope::Run {
            run_id: "object-run".into(),
        },
        retention: Retention::RunDependency,
    }
}
fn unique() -> String {
    format!(
        "objects-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    )
}

#[test]
fn binding_refuses_remote_plaintext_credentials_in_url_paths_and_debug_secret_leaks() {
    let good = S3Binding {
        endpoint: "http://127.0.0.1:9000".into(),
        region: "us-east-1".into(),
        bucket: "workflow-artifacts".into(),
        prefix: "tests".into(),
        allow_http_loopback: true,
    };
    for endpoint in [
        "http://example.com",
        "https://a:b@example.com",
        "https://example.com/path",
        "https://example.com?token=secret",
    ] {
        let mut binding = good.clone();
        binding.endpoint = endpoint.into();
        assert!(
            S3Client::new(binding, "access".into(), "secret-value".into(), None, None).is_err()
        );
    }
    let client = S3Client::new(good, "access".into(), "secret-value".into(), None, None).unwrap();
    assert!(!format!("{client:?}").contains("secret-value"));
    assert!(client.presign("test/key", 301).is_err());
    assert!(client.presign("../key", 1).is_err());
}

#[test]
#[ignore = "requires disposable PostgreSQL and real MinIO; mandatory CI object contract"]
fn real_s3_roundtrip_lineage_download_auth_integrity_and_process_crashes() {
    let namespace = unique();
    let mut target = fixture(&namespace);
    let root = std::env::temp_dir().join(&namespace);
    let mut local = workflow_artifact_local::LocalArtifactStore::create(&root).unwrap();
    let requirement = local
        .publish(
            &spec("requirements", vec![]),
            &mut b"requirements".as_slice(),
        )
        .unwrap();
    let design = local
        .publish(
            &spec("design", vec![requirement.link()]),
            &mut b"design".as_slice(),
        )
        .unwrap();
    let release = local
        .publish(
            &spec("release", vec![design.link()]),
            &mut b"release".as_slice(),
        )
        .unwrap();
    for reference in local.lineage(&release.link()).unwrap() {
        assert_eq!(
            target
                .publish(
                    &reference.manifest.spec,
                    &mut local.read(&reference.link()).unwrap().as_slice()
                )
                .unwrap(),
            reference
        );
    }
    assert_eq!(
        target.lineage(&release.link()).unwrap(),
        local.lineage(&release.link()).unwrap()
    );
    assert_eq!(target.read(&release.link()).unwrap(), b"release");
    assert_eq!(
        target
            .publish(&release.manifest.spec, &mut b"release".as_slice())
            .unwrap(),
        release
    );
    let download_grant = target.grant_download(&release.link(), 1).unwrap();
    assert_eq!(
        download(&download_grant.artifact, &download_grant.url, true, None).unwrap(),
        b"release"
    );
    let key = target
        .with_catalog(|_, r| Ok(r[&release.artifact_id].key.clone()))
        .unwrap();
    let changed_url = download_grant
        .url
        .replace(&key, &format!("{namespace}/upload-{}", "0".repeat(64)));
    assert!(download(&release, &changed_url, true, None).is_err());
    assert!(
        reqwest::blocking::Client::new()
            .put(&download_grant.url)
            .body("forged")
            .send()
            .unwrap()
            .status()
            .is_client_error()
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(download(&release, &download_grant.url, true, None).is_err());
    let wrong_type = ArtifactType {
        content: ContentSchema::Bytes,
        ..release.manifest.spec.artifact_type.clone()
    };
    assert_eq!(
        verify_expected(&target, &release.link(), &wrong_type)
            .unwrap_err()
            .code,
        ErrorCode::TypeConflict
    );
    let mut rebound = spec("rebound", vec![]);
    rebound.artifact_type = wrong_type;
    assert_eq!(
        target
            .publish(&rebound, &mut b"data".as_slice())
            .unwrap_err()
            .code,
        ErrorCode::TypeConflict
    );
    let mut cross_run = spec("cross-run", vec![release.link()]);
    cross_run.producer.run_id = "another-run".into();
    cross_run.access = AccessScope::Run {
        run_id: "another-run".into(),
    };
    assert_eq!(
        target
            .publish(&cross_run, &mut b"data".as_slice())
            .unwrap_err()
            .code,
        ErrorCode::ScopeMismatch
    );
    // Corruption/missing bytes also invalidate downstream consumers, not only direct reads.
    let input_key = target
        .with_catalog(|_, r| Ok(r[&requirement.artifact_id].key.clone()))
        .unwrap();
    target.objects.delete(&input_key).unwrap();
    assert!(target.verify(&release.link()).is_err());
    target.objects.put(&input_key, b"modified").unwrap();
    assert!(target.verify(&release.link()).is_err());
    assert!(target.cleanup_orphans(None, 100).is_err());
    target.objects.delete(&input_key).unwrap();
    target.objects.put(&input_key, b"requirements").unwrap();
    assert_eq!(target.verify(&release.link()).unwrap(), release);
    drop(target);
    for stage in [
        "reserved",
        "upload_half",
        "object_uploaded",
        "manifest_written",
        "after_commit",
    ] {
        let outcome = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::crash_child", "--ignored", "--nocapture"])
            .env("WORKFLOW_OBJECT_CHILD_NAMESPACE", &namespace)
            .env("WORKFLOW_OBJECT_CHILD_STAGE", stage)
            .output()
            .unwrap();
        assert!(
            !outcome.status.success(),
            "child must terminate at fault point: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        let mut reopened = fixture(&namespace);
        let body = crash_body(stage);
        let expected = reference(&spec(stage, vec![]), &body).unwrap();
        if stage == "after_commit" {
            assert_eq!(reopened.verify(&expected.link()).unwrap(), expected);
        } else {
            assert_eq!(
                reopened.verify(&expected.link()).unwrap_err().code,
                ErrorCode::NotFound
            );
        }
        reopened
            .connection
            .borrow_mut()
            .execute(
                "UPDATE workflow_objects.uploads SET expires_at=0 WHERE namespace=$1",
                &[&namespace],
            )
            .unwrap();
        let cleanup = reopened.cleanup_orphans(None, 100).unwrap();
        if stage != "after_commit" {
            assert!(cleanup.orphan_delete_requests > 0);
        }
        // Independent process restart returns the exact existing identity or safely republishes.
        assert_eq!(
            reopened
                .publish(&expected.manifest.spec, &mut body.as_slice())
                .unwrap(),
            expected
        );
        assert_eq!(reopened.verify(&release.link()).unwrap(), release);
    }
    let mut children = vec![];
    for _ in 0..2 {
        children.push(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::crash_child", "--ignored", "--nocapture"])
                .env("WORKFLOW_OBJECT_CHILD_NAMESPACE", &namespace)
                .env("WORKFLOW_OBJECT_CHILD_STAGE", "concurrent")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let reopened = fixture(&namespace);
    let expected = reference(&spec("concurrent", vec![]), &crash_body("concurrent")).unwrap();
    assert_eq!(reopened.verify(&expected.link()).unwrap(), expected);
    assert_eq!(
        reopened
            .retained_manifests()
            .unwrap()
            .iter()
            .filter(|r| r.artifact_id == expected.artifact_id)
            .count(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "subprocess fault entry point"]
fn crash_child() {
    let Ok(namespace) = std::env::var("WORKFLOW_OBJECT_CHILD_NAMESPACE") else {
        return;
    };
    let stage = std::env::var("WORKFLOW_OBJECT_CHILD_STAGE").unwrap();
    fixture(&namespace)
        .publish_internal(
            &spec(&stage, vec![]),
            &mut crash_body(&stage).as_slice(),
            |point| {
                if point == stage {
                    std::process::exit(86);
                }
            },
        )
        .unwrap();
    if stage != "concurrent" {
        panic!("fault point was not reached");
    }
}
fn crash_body(stage: &str) -> Vec<u8> {
    if stage == "upload_half" {
        vec![b'x'; 8 * 1024 * 1024]
    } else {
        b"process-crash".to_vec()
    }
}
