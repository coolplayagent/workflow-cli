use crate::*;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use workflow_kernel::{CompiledBundle, Engine};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Seed {
    pub run_id: String,
    pub inputs: Values,
    pub started_at_unix_ms: u64,
    pub limits: Limits,
}
#[derive(Clone)]
pub(crate) struct Recovered {
    pub engine: Engine,
    pub events: Vec<RecordedEvent>,
    pub outbox: Vec<OutboxEntry>,
    pub checkpoint_revision: u64,
    pub replayed_events: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateCheckpoint {
    snapshot: Snapshot,
    events_digest: String,
    commands_digest: String,
    previous_digest: String,
}
pub(crate) fn append_commands(
    out: &mut Vec<OutboxEntry>,
    engine: &Engine,
    transition: &Transition,
) -> Result<()> {
    for (i, command) in transition.commands.iter().enumerate() {
        out.push(command_entry(
            &engine.snapshot().run_digest,
            &engine.snapshot().run_id,
            out.len() as u64 + 1,
            transition.revision,
            i as u32,
            command.clone(),
        )?);
    }
    Ok(())
}
pub(crate) fn recover(
    c: &Connection,
    id: &str,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<Recovered> {
    recover_inner(c, id, artifacts, false)
}
pub(crate) fn audit(
    c: &Connection,
    id: &str,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<Recovered> {
    recover_inner(c, id, artifacts, true)
}
/// Every read verifies journal identity/digests and the immutable state checkpoint.
/// Ordinary recovery applies only the tail. An explicit audit independently
/// replays the complete journal and checks all retained checkpoint states.
fn recover_inner(
    c: &Connection,
    id: &str,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
    full_audit: bool,
) -> Result<Recovered> {
    validate_id(id)?;
    check_version(c)?;
    let (run_digest, bundle_digest, seed_json, seed_digest): (String, String, String, String) = c
        .query_row(
            "SELECT run_digest,bundle_digest,seed,seed_digest FROM runs WHERE run_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| Error::new(ErrorCode::NotFound, "run not found"))?;
    let seed: Seed = decode(&seed_json, &seed_digest)?;
    if seed.run_id != id {
        return Err(corrupt("run seed identity mismatch"));
    }
    let bundle_json: String = c
        .query_row(
            "SELECT document FROM bundles WHERE digest=?1",
            [&bundle_digest],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| corrupt("missing bundle"))?;
    let spec: BundleSpec =
        workflow_worker::parse_message(bundle_json.as_bytes()).map_err(|e| corrupt(e.message))?;
    let bundle = CompiledBundle::compile(spec).map_err(|e| corrupt(e.message))?;
    if bundle.digest() != bundle_digest {
        return Err(corrupt("bundle digest mismatch"));
    }
    crate::bindings::lock_bindings(c, &bundle, false)?;
    let (mut engine, initial) = Engine::start(
        bundle.clone(),
        id,
        seed.inputs.clone(),
        seed.started_at_unix_ms,
        seed.limits.clone(),
    )?;
    if engine.snapshot().run_digest != run_digest {
        return Err(corrupt("run fingerprint mismatch"));
    }
    let (revision, snapshot_json, state_digest): (i64, String, String) = c
        .query_row(
            "SELECT revision,snapshot,state_digest FROM heads WHERE run_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| corrupt("missing run head"))?;
    let expected: Snapshot = decode(&snapshot_json, &state_digest)?;
    if revision < 1 || revision as u64 != expected.revision || expected.run_id != id {
        return Err(corrupt("head identity/revision mismatch"));
    }
    let mut events = vec![];
    let mut event_hashes = BTreeMap::from([(1, digest(&"empty state event journal")?)]);
    let mut event_chain = event_hashes[&1].clone();
    let mut q=c.prepare("SELECT revision,event_id,document,digest FROM events WHERE run_id=?1 ORDER BY revision").map_err(storage)?;
    let mut rows = q.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let rev = counter(row, 0)?;
        let event_id: String = row.get(1).map_err(storage)?;
        let text: String = row.get(2).map_err(storage)?;
        let hash: String = row.get(3).map_err(storage)?;
        let event: Event = decode(&text, &hash)?;
        if rev != events.len() as u64 + 2
            || event.expected_revision + 1 != rev
            || event.event_id != event_id
            || event.run_id != id
            || event.run_digest != run_digest
        {
            return Err(corrupt("event sequence/identity mismatch"));
        }
        if let workflow_kernel::EventKind::MigrateDefinition { plan } = &event.kind {
            let target = CompiledBundle::compile(plan.request.target_bundle.clone())?;
            crate::migration::retained_bundle(c, &target, false)?;
        }
        event_chain = digest(&(event_chain, &hash))?;
        event_hashes.insert(rev, event_chain.clone());
        events.push(RecordedEvent {
            revision: rev,
            digest: hash,
            event,
        });
    }
    if events.len() as u64 + 1 != expected.revision {
        return Err(corrupt("event journal differs from head"));
    }
    let mut outbox = vec![];
    let mut command_hashes = BTreeMap::new();
    let mut command_chain = digest(&"empty state command journal")?;
    let mut q=c.prepare("SELECT sequence,revision,command_index,command_id,document,digest FROM outbox WHERE run_id=?1 ORDER BY sequence").map_err(storage)?;
    let mut rows = q.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let sequence = counter(row, 0)?;
        let rev = counter(row, 1)?;
        let index: u32 = row.get(2).map_err(storage)?;
        let command_id: String = row.get(3).map_err(storage)?;
        let text: String = row.get(4).map_err(storage)?;
        let hash: String = row.get(5).map_err(storage)?;
        let actual: OutboxEntry = decode(&text, &hash)?;
        let canonical = command_entry(
            &run_digest,
            id,
            sequence,
            rev,
            index,
            actual.command.clone(),
        )?;
        if sequence != outbox.len() as u64 + 1
            || actual != canonical
            || actual.command_id != command_id
            || rev > expected.revision
            || outbox
                .last()
                .is_some_and(|p: &OutboxEntry| p.revision > rev)
        {
            return Err(corrupt("outbox identity/order mismatch"));
        }
        command_chain = digest(&(command_chain, hash))?;
        command_hashes.insert(rev, command_chain.clone());
        outbox.push(actual);
    }
    let command_hash_at = |rev: u64| -> Result<String> {
        Ok(command_hashes
            .range(..=rev)
            .next_back()
            .map(|(_, s)| s.clone())
            .unwrap_or(digest(&"empty state command journal")?))
    };
    // Legacy checkpoints remain immutable and independently checksummed. They are
    // compared during the single full replay, never replayed once per prefix.
    let mut legacy = BTreeMap::new();
    let mut q = c
        .prepare(
            "SELECT revision,document,digest FROM checkpoints WHERE run_id=?1 ORDER BY revision",
        )
        .map_err(storage)?;
    let mut rows = q.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let rev = counter(row, 0)?;
        let cp: Checkpoint = decode(
            &row.get::<_, String>(1).map_err(storage)?,
            &row.get::<_, String>(2).map_err(storage)?,
        )?;
        Engine::check_checkpoint(&bundle, &cp).map_err(|e| corrupt(e.message))?;
        if cp.run_id != id
            || rev != cp.events.len() as u64 + 1
            || rev > expected.revision
            || cp.events.iter().zip(&events).any(|(a, b)| a != &b.event)
        {
            return Err(corrupt("checkpoint journal prefix mismatch"));
        }
        legacy.insert(rev, cp);
    }
    let mut states = BTreeMap::new();
    let mut previous_state_digest = digest(&"empty state checkpoints")?;
    let mut q=c.prepare("SELECT revision,document,digest FROM state_checkpoints WHERE run_id=?1 ORDER BY revision").map_err(storage)?;
    let mut rows = q.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let rev = counter(row, 0)?;
        let cp: StateCheckpoint = decode(
            &row.get::<_, String>(1).map_err(storage)?,
            &row.get::<_, String>(2).map_err(storage)?,
        )?;
        if cp.previous_digest != previous_state_digest
            || cp.snapshot.revision != rev
            || cp.snapshot.run_id != id
            || cp.snapshot.run_digest != run_digest
            || event_hashes.get(&rev) != Some(&cp.events_digest)
            || command_hash_at(rev)? != cp.commands_digest
        {
            return Err(corrupt("state checkpoint journal binding mismatch"));
        }
        previous_state_digest = digest(&cp)?;
        states.insert(rev, cp);
    }
    let checkpoint_revision = states
        .keys()
        .chain(legacy.keys())
        .max()
        .copied()
        .ok_or_else(|| corrupt("missing initial checkpoint"))?;
    let wanted = if expected.revision < 16 {
        1
    } else {
        expected.revision / 16 * 16
    };
    if checkpoint_revision < wanted || checkpoint_revision > expected.revision {
        return Err(corrupt("missing periodic checkpoint"));
    }
    let checkpoint_matches = |engine: &Engine| -> Result<()> {
        let rev = engine.snapshot().revision;
        if let Some(cp) = states.get(&rev)
            && &cp.snapshot != engine.snapshot()
        {
            return Err(corrupt("state checkpoint disagrees with replay"));
        }
        if let Some(cp) = legacy.get(&rev)
            && &engine.checkpoint()? != cp
        {
            return Err(corrupt("legacy checkpoint disagrees with replay"));
        }
        Ok(())
    };
    let mut calculated = vec![];
    let start_revision = if !full_audit && let Some((&rev, cp)) = states.last_key_value() {
        let envelope = Engine::bind_verified_state(
            &bundle,
            id.into(),
            seed.inputs,
            seed.started_at_unix_ms,
            seed.limits,
            events
                .iter()
                .take((rev - 1) as usize)
                .map(|e| e.event.clone())
                .collect(),
            &cp.snapshot,
        )?;
        engine = Engine::restore_verified_state(bundle, envelope, cp.snapshot.clone())
            .map_err(|e| corrupt(e.message))?;
        calculated.extend(outbox.iter().filter(|e| e.revision <= rev).cloned());
        rev
    } else {
        append_commands(&mut calculated, &engine, &initial)?;
        checkpoint_matches(&engine)?;
        1
    };
    for event in events.iter().filter(|e| e.revision > start_revision) {
        let transition = engine
            .apply(event.event.clone())
            .map_err(|e| corrupt(e.message))?;
        if transition.duplicate || transition.revision != event.revision {
            return Err(corrupt("non-advancing persisted event"));
        }
        append_commands(&mut calculated, &engine, &transition)?;
        checkpoint_matches(&engine)?;
    }
    if engine.snapshot() != &expected || calculated != outbox {
        return Err(corrupt("recovered state/commands disagree with journal"));
    }
    let mut query = c
        .prepare("SELECT sequence,document,digest FROM receipts WHERE run_id=?1 ORDER BY sequence")
        .map_err(storage)?;
    let mut rows = query.query([id]).map_err(storage)?;
    let mut receipt_sequence = 1;
    let mut chain = digest(&"empty delivery journal")?;
    while let Some(row) = rows.next().map_err(storage)? {
        let sequence = counter(row, 0)?;
        if sequence != receipt_sequence {
            return Err(corrupt("delivery receipt order gap"));
        }
        let text: String = row.get(1).map_err(storage)?;
        let hash: String = row.get(2).map_err(storage)?;
        let receipt: DeliveryReceipt = decode(&text, &hash)?;
        validate_id(&receipt.delivery_id).map_err(|e| corrupt(e.message))?;
        let entry = outbox
            .get_mut((sequence - 1) as usize)
            .ok_or_else(|| corrupt("receipt has no command"))?;
        if receipt.run_id != id
            || receipt.command_id != entry.command_id
            || receipt.command_digest != entry.command_digest
        {
            return Err(corrupt("receipt targets a different command"));
        }
        chain = digest(&(chain, hash))?;
        entry.receipt = Some(receipt);
        receipt_sequence += 1;
    }
    let (delivery_sequence, delivery_chain): (i64, String) = c
        .query_row(
            "SELECT sequence,chain_digest FROM delivery_heads WHERE run_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| corrupt("missing delivery head"))?;
    if delivery_sequence != number(receipt_sequence - 1)? || delivery_chain != chain {
        return Err(corrupt("delivery receipt journal differs from its head"));
    }
    let recovered = Recovered {
        engine,
        events,
        outbox,
        checkpoint_revision,
        replayed_events: expected.revision - start_revision,
    };
    crate::execution::read(c, &recovered, artifacts)?;
    crate::inbox::verify_subjects(&recovered.engine, artifacts)?;
    Ok(recovered)
}
pub(crate) fn write_checkpoint(c: &Connection, engine: &Engine) -> Result<()> {
    write_state_checkpoint(c, engine)
}
pub(crate) fn write_state_checkpoint(c: &Connection, engine: &Engine) -> Result<()> {
    let id = &engine.snapshot().run_id;
    let chain = |table: &str, initial: &str| -> Result<String> {
        let order = if table == "events" {
            "revision"
        } else {
            "sequence"
        };
        let mut q = c
            .prepare(&format!(
                "SELECT digest FROM {table} WHERE run_id=?1 ORDER BY {order}"
            ))
            .map_err(storage)?;
        let mut rows = q.query([id]).map_err(storage)?;
        let mut chain = digest(&initial)?;
        while let Some(row) = rows.next().map_err(storage)? {
            chain = digest(&(chain, row.get::<_, String>(0).map_err(storage)?))?;
        }
        Ok(chain)
    };
    let previous_digest = c
        .query_row(
            "SELECT digest FROM state_checkpoints WHERE run_id=?1 ORDER BY revision DESC LIMIT 1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(storage)?
        .unwrap_or(digest(&"empty state checkpoints")?);
    let cp = StateCheckpoint {
        previous_digest,
        snapshot: engine.snapshot().clone(),
        events_digest: chain("events", "empty state event journal")?,
        commands_digest: chain("outbox", "empty state command journal")?,
    };
    c.execute(
        "INSERT INTO state_checkpoints(run_id,revision,document,digest) VALUES(?1,?2,?3,?4)",
        params![
            id,
            number(engine.snapshot().revision)?,
            document(&cp)?,
            digest(&cp)?
        ],
    )
    .map_err(storage)?;
    Ok(())
}

fn counter(row: &rusqlite::Row, index: usize) -> Result<u64> {
    let n: i64 = row.get(index).map_err(storage)?;
    u64::try_from(n).map_err(|_| corrupt("negative stored counter"))
}
