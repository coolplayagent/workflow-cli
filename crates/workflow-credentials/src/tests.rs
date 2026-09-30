use super::*;
use serde_json::json;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[test]
fn leases_rotate_without_binding_changes_and_reject_identity_audience_time_and_file_attacks() {
    let dir = std::env::temp_dir().join(format!("credential-contract-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("lease.json");
    let principal = Principal {
        tenant: "tenant-a".into(),
        project: "project".into(),
        actor: "worker".into(),
    };
    let reference = LeaseRef {
        path: path.clone(),
        principal: principal.clone(),
    };
    let envelope = json!({"schema_version":1,"principal":principal,"audience":"https://gateway.example/v1","not_before_unix_ms":1000,"expires_at_unix_ms":2000,"secret":"first-private-secret"});
    let write = |value: &Value| {
        use std::io::Write;
        let next = dir.join("next.json");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&next)
            .unwrap();
        f.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
        f.sync_all().unwrap();
        std::fs::rename(next, &path).unwrap();
    };
    write(&envelope);
    let first = reference
        .resolve("https://gateway.example/v1", 1000)
        .unwrap();
    assert_eq!(first.expose(), "first-private-secret");
    assert_eq!(format!("{first:?}"), "[REDACTED]");
    assert!(
        !serde_json::to_string(&reference)
            .unwrap()
            .contains(first.expose())
    );
    assert!(reference.resolve("https://other.example/v1", 1000).is_err());
    for now in [999, 2000, 3000] {
        assert!(
            reference
                .resolve("https://gateway.example/v1", now)
                .is_err()
        );
    }
    for (field, value) in [
        (
            "principal",
            json!({"tenant":"tenant-b","project":"project","actor":"worker"}),
        ),
        ("expires_at_unix_ms", json!(301001)),
        ("schema_version", json!(2)),
    ] {
        let mut changed = envelope.clone();
        changed[field] = value;
        write(&changed);
        assert!(
            reference
                .resolve("https://gateway.example/v1", 1000)
                .is_err()
        );
    }
    let mut rotated = envelope.clone();
    rotated["secret"] = json!("rotated-private-secret");
    write(&rotated);
    assert_eq!(
        reference
            .resolve("https://gateway.example/v1", 1000)
            .unwrap()
            .expose(),
        "rotated-private-secret"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        reference
            .resolve("https://gateway.example/v1", 1000)
            .is_err()
    );
    write(&envelope);
    let link = dir.join("link");
    std::fs::hard_link(&path, &link).unwrap();
    assert!(
        reference
            .resolve("https://gateway.example/v1", 1000)
            .is_err()
    );
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(read_private(&link, 32768).is_err());
    assert!(validate_sources("OLD_KEY", Some(&reference)).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn reflected_secrets_are_found_after_json_decoding_and_in_binary_artifacts() {
    let v: Value = serde_json::from_str(r#"{"\u006bey":{"nested":["s\u0065cret"]}}"#).unwrap();
    assert!(reflects(&v, "secret"));
    assert!(reflects(&v, "key"));
    assert!(reflects(
        &json!({"content":b"prefix secret suffix".to_vec()}),
        "secret"
    ));
    assert!(!reflects(&json!({"summary":"accepted"}), "secret"));
}
