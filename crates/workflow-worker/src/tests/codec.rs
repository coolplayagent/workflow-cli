use super::*;
use serde::ser::{SerializeMap, Serializer};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

// Frozen digest reference from 60963c7729acb953c0a0e154a523bb7871bba532.
fn original_digest<T: Serialize>(value: &T) -> Result<String> {
    let normalized: serde_json::Value = parse_message(&to_message(value)?)?;
    let bytes = serde_json::to_vec(&normalized)
        .map_err(|error| Error::new(ErrorCode::InvalidMessage, error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn fixture(name: &str) -> serde_json::Value {
    let root = if let Some(runfiles) = std::env::var_os("TEST_SRCDIR") {
        PathBuf::from(runfiles)
            .join(std::env::var_os("TEST_WORKSPACE").unwrap())
            .join("crates/workflow-worker")
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    };
    parse_message(&std::fs::read(root.join("fixtures").join(name)).unwrap()).unwrap()
}

fn typed_digests() -> BTreeMap<&'static str, String> {
    let request = request();
    let grant = ExecutionGrant::bind(&request).unwrap();
    let result = WorkResult {
        protocol_version: PROTOCOL_VERSION,
        request_digest: digest(&request).unwrap(),
        completed_at_unix_ms: 101,
        outcome: AdapterOutcome::Succeeded {
            outputs: Values::from([("value".into(), json!(7))]),
            evidence: vec![],
        },
        model_record: None,
    };
    assert_eq!(digest(&descriptor()), original_digest(&descriptor()));
    assert_eq!(digest(&request), original_digest(&request));
    assert_eq!(digest(&grant), original_digest(&grant));
    assert_eq!(digest(&result), original_digest(&result));
    BTreeMap::from([
        ("capability", digest(&descriptor()).unwrap()),
        ("request", digest(&request).unwrap()),
        ("grant", digest(&grant).unwrap()),
        ("result", digest(&result).unwrap()),
    ])
}

#[test]
fn canonical_digests_match_frozen_values_and_typed_records() {
    let expected = fixture("digest-goldens.json");
    for case in fixture("digest-values.json").as_array().unwrap() {
        let value = &case["value"];
        let actual = digest(value).unwrap();
        assert_eq!(Ok(actual.clone()), original_digest(value));
        assert_eq!(actual, expected["values"][case["id"].as_str().unwrap()]);
    }
    for (name, actual) in typed_digests() {
        assert_eq!(actual, expected["typed"][name]);
    }
}

struct Pairs<'a>(&'a [(&'a str, u64)]);
impl Serialize for Pairs<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[test]
fn custom_map_order_is_normalized_but_duplicate_keys_are_rejected() {
    let forward = Pairs(&[("a", 1), ("z", 2)]);
    let reversed = Pairs(&[("z", 2), ("a", 1)]);
    assert_eq!(digest(&forward), digest(&reversed));
    assert_eq!(digest(&forward), original_digest(&forward));
    let duplicate = Pairs(&[("key", 1), ("key", 2)]);
    for result in [digest(&duplicate), original_digest(&duplicate)] {
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidMessage);
        assert!(error.message.contains("duplicate key key"));
    }
    #[derive(Serialize)]
    struct Nested<T> {
        payload: T,
    }
    let nested = Nested { payload: duplicate };
    assert_eq!(digest(&nested), original_digest(&nested));
    assert!(
        digest(&nested)
            .unwrap_err()
            .message
            .contains("duplicate key key")
    );
}

struct Broken;
impl Serialize for Broken {
    fn serialize<S: Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("intentional serializer failure"))
    }
}

#[test]
fn serializer_errors_and_exact_byte_limit_match_the_frozen_path() {
    assert_eq!(digest(&Broken), original_digest(&Broken));
    assert_eq!(digest(&Broken).unwrap_err().code, ErrorCode::InvalidMessage);
    let at_limit = "x".repeat(MAX_MESSAGE_BYTES - 2);
    assert_eq!(
        serde_json::to_vec(&at_limit).unwrap().len(),
        MAX_MESSAGE_BYTES
    );
    assert_eq!(digest(&at_limit), original_digest(&at_limit));
    assert!(digest(&at_limit).is_ok());
    let over_limit = "x".repeat(MAX_MESSAGE_BYTES - 1);
    assert_eq!(
        serde_json::to_vec(&over_limit).unwrap().len(),
        MAX_MESSAGE_BYTES + 1
    );
    assert_eq!(digest(&over_limit), original_digest(&over_limit));
    assert_eq!(
        digest(&over_limit).unwrap_err().message,
        "message exceeds 2 MiB"
    );
}

#[test]
fn nesting_boundary_matches_the_actual_old_decoder_limit() {
    let first_rejected = fixture("digest-goldens.json")["first_rejected_array_depth"]
        .as_u64()
        .unwrap() as usize;
    assert!((2..=140).contains(&first_rejected));
    for depth in [first_rejected - 1, first_rejected, first_rejected + 1] {
        let nested = (0..depth).fold(json!(0), |value, _| json!([value]));
        let actual = digest(&nested);
        assert_eq!(actual, original_digest(&nested));
        assert_eq!(actual.is_err(), depth >= first_rejected);
    }
}
