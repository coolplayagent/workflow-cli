use crate::*;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
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
}
pub(crate) fn append_commands(
    out: &mut Vec<OutboxEntry>,
    engine: &Engine,
    transition: &Transition,
) -> Result<()> {
    for (i, c) in transition.commands.iter().enumerate() {
        out.push(command_entry(
            &engine.snapshot().run_digest,
            &engine.snapshot().run_id,
            out.len() as u64 + 1,
            transition.revision,
            i as u32,
            c.clone(),
        )?);
    }
    Ok(())
}
/// Read under one caller-owned transaction. Rebuild full history and independently
/// restore the latest checkpoint plus tail. Verify every persisted command intent.
pub(crate) fn recover(
    c: &Connection,
    id: &str,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
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
    let (mut full, initial) = Engine::start(
        bundle.clone(),
        id,
        seed.inputs,
        seed.started_at_unix_ms,
        seed.limits,
    )
    .map_err(|e| corrupt(e.message))?;
    if full.snapshot().run_digest != run_digest {
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
    let revision = u64::try_from(revision).map_err(|_| corrupt("negative revision"))?;
    let expected: Snapshot = decode(&snapshot_json, &state_digest)?;
    if revision != expected.revision || expected.run_id != id {
        return Err(corrupt("head identity/revision mismatch"));
    }
    let mut outbox = vec![];
    append_commands(&mut outbox, &full, &initial)?;
    let mut events = vec![];
    let mut query=c.prepare("SELECT revision,event_id,document,digest FROM events WHERE run_id=?1 ORDER BY revision").map_err(storage)?;
    let mut rows = query.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let rev = counter(row, 0)?;
        let event_id: String = row.get(1).map_err(storage)?;
        let text: String = row.get(2).map_err(storage)?;
        let hash: String = row.get(3).map_err(storage)?;
        let event: Event = decode(&text, &hash)?;
        if event.event_id != event_id || rev != full.snapshot().revision + 1 {
            return Err(corrupt("event sequence/identity mismatch"));
        }
        if let workflow_kernel::EventKind::MigrateDefinition { plan } = &event.kind {
            let target = CompiledBundle::compile(plan.request.target_bundle.clone())?;
            crate::migration::retained_bundle(c, &target, false)?;
        }
        let t = full.apply(event.clone()).map_err(|e| corrupt(e.message))?;
        if t.duplicate || t.revision != rev {
            return Err(corrupt("non-advancing persisted event"));
        }
        append_commands(&mut outbox, &full, &t)?;
        events.push(RecordedEvent {
            revision: rev,
            digest: hash,
            event,
        });
    }
    if full.snapshot() != &expected {
        return Err(corrupt("full journal replay disagrees with stored head"));
    }
    // Validate every retained checkpoint, including prefix continuity. A corrupted
    // older checkpoint must not silently disappear behind a more recent one.
    let mut checkpoint_revision = 0;
    let mut latest = None;
    let mut query = c
        .prepare(
            "SELECT revision,document,digest FROM checkpoints WHERE run_id=?1 ORDER BY revision",
        )
        .map_err(storage)?;
    let mut rows = query.query([id]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        let rev = counter(row, 0)?;
        let text: String = row.get(1).map_err(storage)?;
        let hash: String = row.get(2).map_err(storage)?;
        let cp: Checkpoint = decode(&text, &hash)?;
        let expected_revision = if checkpoint_revision == 0 {
            1
        } else if checkpoint_revision == 1 {
            16
        } else {
            checkpoint_revision + 16
        };
        if rev != expected_revision
            || rev > revision
            || cp.run_id != id
            || cp.events.len() as u64 + 1 != rev
            || cp.events.iter().zip(&events).any(|(a, b)| a != &b.event)
        {
            return Err(corrupt("checkpoint journal prefix/sequence mismatch"));
        }
        let restored = Engine::restore(bundle.clone(), cp).map_err(|e| corrupt(e.message))?;
        if restored.snapshot().revision != rev || restored.snapshot().run_digest != run_digest {
            return Err(corrupt("checkpoint revision mismatch"));
        }
        checkpoint_revision = rev;
        latest = Some(restored);
    }
    let mut restored = latest.ok_or_else(|| corrupt("missing initial checkpoint"))?;
    let wanted = if revision < 16 { 1 } else { revision / 16 * 16 };
    if checkpoint_revision != wanted {
        return Err(corrupt("missing periodic checkpoint"));
    }
    for event in events.iter().filter(|e| e.revision > checkpoint_revision) {
        restored
            .apply(event.event.clone())
            .map_err(|e| corrupt(e.message))?;
    }
    if restored.snapshot() != full.snapshot() {
        return Err(corrupt("checkpoint plus tail disagrees with full replay"));
    }
    let mut query=c.prepare("SELECT sequence,revision,command_index,command_id,document,digest FROM outbox WHERE run_id=?1 ORDER BY sequence").map_err(storage)?;
    let mut rows = query.query([id]).map_err(storage)?;
    let mut count = 0;
    while let Some(row) = rows.next().map_err(storage)? {
        let sequence = counter(row, 0)?;
        let revision = counter(row, 1)?;
        let index: u32 = row.get(2).map_err(storage)?;
        let command_id: String = row.get(3).map_err(storage)?;
        let text: String = row.get(4).map_err(storage)?;
        let hash: String = row.get(5).map_err(storage)?;
        let actual: OutboxEntry = decode(&text, &hash)?;
        let expected = outbox
            .get(count)
            .ok_or_else(|| corrupt("extra outbox command"))?;
        if &actual != expected
            || sequence != expected.sequence
            || revision != expected.revision
            || index != expected.command_index
            || command_id != expected.command_id
        {
            return Err(corrupt("outbox differs from deterministic intents"));
        }
        count += 1;
    }
    if count != outbox.len() {
        return Err(corrupt("missing outbox command"));
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
        engine: full,
        events,
        outbox,
        checkpoint_revision,
    };
    crate::execution::read(c, &recovered, artifacts)?;
    crate::inbox::verify_subjects(&recovered.engine, artifacts)?;
    Ok(recovered)
}
pub(crate) fn write_checkpoint(c: &Connection, engine: &Engine) -> Result<()> {
    let cp = engine.checkpoint()?;
    c.execute(
        "INSERT INTO checkpoints(run_id,revision,document,digest) VALUES(?1,?2,?3,?4)",
        params![
            cp.run_id,
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
