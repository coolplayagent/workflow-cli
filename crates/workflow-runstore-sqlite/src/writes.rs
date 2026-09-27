use crate::recovery::{Seed, append_commands, recover, write_checkpoint};
use crate::*;
use rusqlite::{OptionalExtension, params};
use workflow_kernel::{CompiledBundle, Engine};

impl SqliteRunStore {
    pub(crate) fn start_internal(
        &mut self,
        r: &StartRun,
        hook: impl Fn(&str),
    ) -> Result<Committed> {
        if r.schema_version != 1 {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "only run start schema 1 is supported",
            ));
        }
        workflow_worker::to_message(r)?;
        let bundle = CompiledBundle::compile(r.bundle.clone())?;
        let (engine, transition) = Engine::start(
            bundle,
            r.run_id.as_str(),
            r.inputs.clone(),
            r.started_at_unix_ms,
            r.limits.clone(),
        )?;
        let seed = Seed {
            run_id: r.run_id.clone(),
            inputs: r.inputs.clone(),
            started_at_unix_ms: r.started_at_unix_ms,
            limits: r.limits.clone(),
        };
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        check_version(&tx)?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT run_digest FROM runs WHERE run_id=?1",
                [&r.run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(hash) = exists {
            if hash != engine.snapshot().run_digest {
                return Err(Error::new(
                    ErrorCode::StartConflict,
                    "run ID already has a different immutable start",
                ));
            }
            let current = recover(&tx, &r.run_id)?;
            let snapshot = current.engine.snapshot().clone();
            tx.commit().map_err(storage)?;
            return Ok(Committed {
                transition: Transition {
                    revision: snapshot.revision,
                    duplicate: true,
                    commands: vec![],
                },
                snapshot,
            });
        }
        let bundle = engine.bundle();
        crate::bindings::lock_bindings(&tx, bundle, true)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT document FROM bundles WHERE digest=?1",
                [bundle.digest()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(existing) = existing {
            if existing != document(bundle.spec())? {
                return Err(corrupt("stored bundle content mismatch"));
            }
        } else {
            tx.execute(
                "INSERT INTO bundles(digest,document) VALUES(?1,?2)",
                params![bundle.digest(), document(bundle.spec())?],
            )
            .map_err(storage)?;
        }
        tx.execute("INSERT INTO runs(run_id,run_digest,bundle_digest,seed,seed_digest) VALUES(?1,?2,?3,?4,?5)",params![r.run_id,engine.snapshot().run_digest,bundle.digest(),document(&seed)?,digest(&seed)?]).map_err(storage)?;
        tx.execute(
            "INSERT INTO heads(run_id,revision,snapshot,state_digest) VALUES(?1,1,?2,?3)",
            params![
                r.run_id,
                document(engine.snapshot())?,
                digest(engine.snapshot())?
            ],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT INTO delivery_heads(run_id,sequence,chain_digest) VALUES(?1,0,?2)",
            params![r.run_id, digest(&"empty delivery journal")?],
        )
        .map_err(storage)?;
        crate::execution::init_head(&tx, &r.run_id)?;
        hook("state_written");
        write_checkpoint(&tx, &engine)?;
        let mut commands = vec![];
        append_commands(&mut commands, &engine, &transition)?;
        write_commands(&tx, &commands)?;
        hook("before_commit");
        tx.commit().map_err(storage)?;
        hook("after_commit");
        Ok(Committed {
            snapshot: engine.snapshot().clone(),
            transition,
        })
    }
    pub(crate) fn apply_internal(
        &mut self,
        event: &Event,
        hook: impl Fn(&str),
    ) -> Result<Committed> {
        workflow_worker::to_message(event)?;
        validate_id(&event.run_id)?;
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut current = recover(&tx, &event.run_id)?;
        let result = persist_event(&tx, &mut current, event, &hook)?;
        hook("before_commit");
        tx.commit().map_err(storage)?;
        hook("after_commit");
        Ok(result)
    }
    pub(crate) fn acknowledge_delivery(
        &mut self,
        receipt: &DeliveryReceipt,
    ) -> Result<OutboxEntry> {
        workflow_worker::to_message(receipt)?;
        validate_id(&receipt.run_id)?;
        validate_id(&receipt.delivery_id)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let current = recover(&tx, &receipt.run_id)?;
        let entry = persist_receipt(&tx, &current, receipt)?;
        tx.commit().map_err(storage)?;
        Ok(entry)
    }
}
fn write_commands(c: &Connection, commands: &[OutboxEntry]) -> Result<()> {
    for e in commands {
        c.execute("INSERT INTO outbox(run_id,sequence,revision,command_index,command_id,document,digest) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![e.run_id,number(e.sequence)?,number(e.revision)?,e.command_index,e.command_id,document(e)?,digest(e)?]).map_err(storage)?;
    }
    Ok(())
}

pub(crate) fn persist_event(
    c: &Connection,
    current: &mut crate::recovery::Recovered,
    event: &Event,
    hook: impl Fn(&str),
) -> Result<Committed> {
    let old_revision = current.engine.snapshot().revision;
    let transition = current.engine.apply(event.clone())?;
    if transition.duplicate {
        let snapshot = current.engine.snapshot().clone();
        return Ok(Committed {
            snapshot,
            transition,
        });
    }
    c.execute(
        "INSERT INTO events(run_id,revision,event_id,document,digest) VALUES(?1,?2,?3,?4,?5)",
        params![
            event.run_id,
            number(transition.revision)?,
            event.event_id,
            document(event)?,
            digest(event)?
        ],
    )
    .map_err(storage)?;
    hook("event_written");
    let changed=c.execute("UPDATE heads SET revision=?2,snapshot=?3,state_digest=?4 WHERE run_id=?1 AND revision=?5",params![event.run_id,number(transition.revision)?,document(current.engine.snapshot())?,digest(current.engine.snapshot())?,number(old_revision)?]).map_err(storage)?;
    if changed != 1 {
        return Err(corrupt(
            "head update did not affect exactly one expected revision",
        ));
    }
    hook("state_written");
    if transition.revision % 16 == 0 {
        write_checkpoint(c, &current.engine)?;
    }
    let previous = current.outbox.len();
    append_commands(&mut current.outbox, &current.engine, &transition)?;
    write_commands(c, &current.outbox[previous..])?;
    Ok(Committed {
        snapshot: current.engine.snapshot().clone(),
        transition,
    })
}

pub(crate) fn persist_receipt(
    c: &Connection,
    current: &crate::recovery::Recovered,
    receipt: &DeliveryReceipt,
) -> Result<OutboxEntry> {
    let entry = current
        .outbox
        .iter()
        .find(|e| e.command_id == receipt.command_id)
        .ok_or_else(|| Error::new(ErrorCode::NotFound, "command not found in this run"))?;
    if entry.command_digest != receipt.command_digest {
        return Err(Error::new(
            ErrorCode::ReceiptConflict,
            "receipt command digest mismatch",
        ));
    }
    if let Some(existing) = &entry.receipt {
        if existing != receipt {
            return Err(Error::new(
                ErrorCode::ReceiptConflict,
                "command already has a different immutable delivery receipt",
            ));
        }
        let entry = entry.clone();
        return Ok(entry);
    }
    if current
        .outbox
        .iter()
        .any(|e| e.sequence < entry.sequence && e.receipt.is_none())
    {
        return Err(Error::new(
            ErrorCode::DeliveryOrder,
            "earlier commands must be acknowledged first",
        ));
    }
    c.execute(
        "INSERT INTO receipts(run_id,sequence,document,digest) VALUES(?1,?2,?3,?4)",
        params![
            receipt.run_id,
            number(entry.sequence)?,
            document(receipt)?,
            digest(receipt)?
        ],
    )
    .map_err(storage)?;
    let previous: String = c
        .query_row(
            "SELECT chain_digest FROM delivery_heads WHERE run_id=?1",
            [&receipt.run_id],
            |r| r.get(0),
        )
        .map_err(storage)?;
    let changed = c
        .execute(
            "UPDATE delivery_heads SET sequence=?2,chain_digest=?3 WHERE run_id=?1 AND sequence=?4",
            params![
                receipt.run_id,
                number(entry.sequence)?,
                digest(&(previous, digest(receipt)?))?,
                number(entry.sequence - 1)?
            ],
        )
        .map_err(storage)?;
    if changed != 1 {
        return Err(corrupt("delivery head mismatch"));
    }
    let mut entry = entry.clone();
    entry.receipt = Some(receipt.clone());
    Ok(entry)
}
