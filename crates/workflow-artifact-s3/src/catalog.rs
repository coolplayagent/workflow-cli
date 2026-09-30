use crate::*;
pub(crate) struct Record {
    pub reference: ArtifactRef,
    pub key: String,
}
pub(crate) type Records = BTreeMap<String, Record>;
pub(crate) fn version(tx: &mut Transaction<'_>) -> Result<()> {
    let version: i32 = tx
        .query_one(
            "SELECT version FROM workflow_objects.schema_version WHERE singleton=true",
            &[],
        )
        .map_err(sql)?
        .get(0);
    if version != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "object catalog requires schema 1",
        ));
    }
    Ok(())
}
pub(crate) fn lock(tx: &mut Transaction<'_>, namespace: &str, objects: &S3Client) -> Result<()> {
    version(tx)?;
    tx.batch_execute("SET LOCAL synchronous_commit=on; SET LOCAL lock_timeout='5s'")
        .map_err(sql)?;
    let binding: String = tx
        .query_opt(
            "SELECT binding FROM workflow_objects.catalogs WHERE namespace=$1 FOR UPDATE",
            &[&namespace],
        )
        .map_err(sql)?
        .ok_or_else(|| Error::new(ErrorCode::NotFound, "object catalog namespace missing"))?
        .get(0);
    if binding != objects.identity()? {
        return Err(invalid(
            "object catalog is bound to another bucket/endpoint/prefix",
        ));
    }
    Ok(())
}
pub(crate) fn now(tx: &mut Transaction<'_>) -> Result<i64> {
    Ok(tx
        .query_one(
            "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
            &[],
        )
        .map_err(sql)?
        .get(0))
}
pub(crate) fn valid_key(key: &str, namespace: &str) -> bool {
    key.strip_prefix(&format!("{namespace}/upload-"))
        .is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}
pub(crate) fn read(tx: &mut Transaction<'_>, namespace: &str) -> Result<Records> {
    let row = tx
        .query_one(
            "SELECT sequence,chain FROM workflow_objects.catalogs WHERE namespace=$1",
            &[&namespace],
        )
        .map_err(sql)?;
    let sequence: i64 = row.get(0);
    let expected: String = row.get(1);
    if !(0..=512).contains(&sequence) {
        return Err(corrupt("object catalog exceeds capacity"));
    }
    let rows = tx.query("SELECT sequence,id,CASE WHEN octet_length(reference)<=65536 THEN reference ELSE NULL END,object_key,digest FROM workflow_objects.artifacts WHERE namespace=$1 ORDER BY sequence LIMIT 513", &[&namespace]).map_err(sql)?;
    let mut records = Records::new();
    let mut chain = digest(&"empty S3 artifact catalog")?;
    for (index, row) in rows.iter().enumerate() {
        let seq: i64 = row.get(0);
        let id: String = row.get(1);
        let doc: Option<String> = row.get(2);
        let key: String = row.get(3);
        let hash: String = row.get(4);
        if seq != index as i64 + 1 || !valid_key(&key, namespace) {
            return Err(corrupt("object catalog record identity mismatch"));
        }
        let reference: ArtifactRef = parse_message(
            doc.ok_or_else(|| corrupt("object manifest exceeds capacity"))?
                .as_bytes(),
        )?;
        validate_ref(&reference)?;
        if reference.artifact_id != id || digest(&(&reference, &key))? != hash {
            return Err(corrupt("object catalog record digest mismatch"));
        }
        validate_append(&records, &reference)?;
        chain = digest(&(chain, hash))?;
        records.insert(id, Record { reference, key });
    }
    if rows.len() as i64 != sequence || chain != expected {
        return Err(corrupt("object catalog differs from integrity head"));
    }
    Ok(records)
}
pub(crate) fn validate_append(records: &Records, reference: &ArtifactRef) -> Result<()> {
    if let Some(old) = records.get(&reference.artifact_id) {
        if old.reference == *reference {
            return Ok(());
        }
        return Err(corrupt("artifact identity reused"));
    }
    if records.len() >= 512
        || records
            .values()
            .map(|r| r.reference.manifest.bytes)
            .sum::<u64>()
            + reference.manifest.bytes
            > 512 * 1024 * 1024
    {
        return Err(Error::new(
            ErrorCode::Budget,
            "object catalog exceeds 512 artifacts or 512 MiB",
        ));
    }
    for input in &reference.manifest.spec.inputs {
        let parent = records
            .get(&input.artifact_id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "artifact dependency missing"))?;
        if parent.reference.link() != *input {
            return Err(corrupt("dependency digest mismatch"));
        }
        if parent.reference.manifest.spec.access != reference.manifest.spec.access {
            return Err(Error::new(
                ErrorCode::ScopeMismatch,
                "artifact belongs to another run",
            ));
        }
    }
    let ty = &reference.manifest.spec.artifact_type;
    if records.values().any(|r| {
        r.reference.manifest.spec.artifact_type.identity == ty.identity
            && r.reference.manifest.spec.artifact_type != *ty
    }) {
        return Err(Error::new(
            ErrorCode::TypeConflict,
            "artifact type version rebound",
        ));
    }
    Ok(())
}
pub(crate) fn append(
    tx: &mut Transaction<'_>,
    namespace: &str,
    records: &Records,
    reference: &ArtifactRef,
    key: &str,
) -> Result<()> {
    validate_append(records, reference)?;
    let hash = digest(&(reference, key))?;
    let prior: String = tx
        .query_one(
            "SELECT chain FROM workflow_objects.catalogs WHERE namespace=$1",
            &[&namespace],
        )
        .map_err(sql)?
        .get(0);
    let sequence = records.len() as i64 + 1;
    let document = String::from_utf8(to_message(reference)?).expect("JSON");
    tx.execute("INSERT INTO workflow_objects.artifacts(namespace,sequence,id,reference,object_key,digest) VALUES($1,$2,$3,$4,$5,$6)", &[&namespace,&sequence,&reference.artifact_id,&document,&key,&hash]).map_err(sql)?;
    let count = tx.execute("UPDATE workflow_objects.catalogs SET sequence=$2,chain=$3 WHERE namespace=$1 AND sequence=$4", &[&namespace,&sequence,&digest(&(prior,hash))?,&(sequence-1)]).map_err(sql)?;
    if count != 1 {
        return Err(corrupt("object catalog head CAS failed"));
    }
    Ok(())
}
