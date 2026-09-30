use crate::*;
use workflow_kernel::{EventKind, InboxEntry, NodeState, WaitTarget};
use workflow_worker::Clock;

/// Every accepted subject remains a recovery dependency. A newly applied early
/// callback is checked by the same transaction that releases its successors.
pub(crate) fn verify_subjects(
    engine: &workflow_kernel::Engine,
    artifacts: Option<&dyn workflow_artifacts::ArtifactReader>,
) -> Result<()> {
    for entry in engine.snapshot().inbox.values() {
        if !matches!(entry.status, workflow_kernel::SignalStatus::Applied { .. }) {
            continue;
        }
        for link in engine.wait_subjects(entry.message.target.instance_id)? {
            let reader = artifacts.ok_or_else(|| {
                Error::new(
                    ErrorCode::ArtifactUnavailable,
                    "approval subject store unavailable",
                )
            })?;
            let reference = reader.verify(&link).map_err(|_| {
                Error::new(
                    ErrorCode::ArtifactRejected,
                    "approval subject is missing or corrupt",
                )
            })?;
            if reference.manifest.spec.access
                != (workflow_artifacts::AccessScope::Run {
                    run_id: engine.snapshot().run_id.clone(),
                })
                || reference.link() != link
            {
                return Err(Error::new(
                    ErrorCode::ArtifactRejected,
                    "approval subject belongs to another run",
                ));
            }
        }
    }
    Ok(())
}

impl SqliteRunStore {
    pub(crate) fn receive_signal_internal(
        &mut self,
        submission: &SignalSubmission,
        clock: &dyn Clock,
        hook: impl Fn(&str),
    ) -> Result<SignalReceipt> {
        workflow_worker::to_message(submission)?;
        if submission.schema_version != 1 {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "signal submission requires schema 1",
            ));
        }
        hook("before_transaction");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut r = crate::recovery::recover(&tx, &submission.run_id, self.artifacts.as_deref())?;
        if r.engine.snapshot().run_digest != submission.run_digest {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "signal belongs to a different immutable run",
            ));
        }
        workflow_kernel::validate_signal(&submission.run_digest, &submission.message)?;
        if let Some(entry) = r
            .engine
            .snapshot()
            .inbox
            .get(&submission.message.message_id)
        {
            if entry.message != submission.message {
                return Err(Error::new(
                    ErrorCode::SignalConflict,
                    "message identity already binds different content",
                ));
            }
            let receipt = SignalReceipt {
                run_id: submission.run_id.clone(),
                run_revision: r.engine.snapshot().revision,
                duplicate: true,
                entry: entry.clone(),
            };
            tx.commit().map_err(storage)?;
            return Ok(receipt);
        }
        let now = clock.now_unix_ms()?;
        let event = Event {
            event_id: format!("inbox-{}", &digest(&submission.message.message_id)?[7..]),
            run_id: submission.run_id.clone(),
            run_digest: submission.run_digest.clone(),
            expected_revision: r.engine.snapshot().revision,
            at_unix_ms: now,
            kind: EventKind::ReceiveSignal {
                message: Box::new(submission.message.clone()),
            },
        };
        let committed =
            crate::writes::persist_event(&tx, &mut r, &event, self.artifacts.as_deref(), &hook)?;
        let entry = committed.snapshot.inbox[&submission.message.message_id].clone();
        let receipt = SignalReceipt {
            run_id: submission.run_id.clone(),
            run_revision: committed.snapshot.revision,
            duplicate: false,
            entry,
        };
        check_signal_admission(&committed.snapshot, clock.now_unix_ms()?)?;
        hook("before_commit");
        tx.commit().map_err(storage)?;
        hook("after_commit");
        Ok(receipt)
    }
}

impl InboxStore for SqliteRunStore {
    fn receive_signal(
        &mut self,
        submission: &SignalSubmission,
        clock: &dyn Clock,
    ) -> Result<SignalReceipt> {
        self.receive_signal_internal(submission, clock, |_| {})
    }
    fn inbox(&mut self, id: &str, after: u64, limit: u32) -> Result<Page<InboxEntry, u64>> {
        validate_limit(limit)?;
        self.read(id, |r| {
            let mut items: Vec<_> = r
                .engine
                .snapshot()
                .inbox
                .values()
                .filter(|e| e.received_revision > after)
                .cloned()
                .collect();
            items.sort_by_key(|e| e.received_revision);
            let more = items.len() > limit as usize;
            items.truncate(limit as usize);
            let next_cursor = more.then(|| items.last().unwrap().received_revision);
            Ok(Page { items, next_cursor })
        })
    }
    fn waits(&mut self, id: &str, after: u64, limit: u32) -> Result<Page<WaitRegistration, u64>> {
        validate_limit(limit)?;
        self.read(id, |r| {
            let s = r.engine.snapshot();
            let mut items = vec![];
            for frame in s.frames.values() {
                let workflow = r
                    .engine
                    .bundle()
                    .spec()
                    .workflows
                    .iter()
                    .find(|w| w.id == frame.workflow.id && w.version == frame.workflow.version)
                    .ok_or_else(|| corrupt("missing wait workflow"))?;
                for (id, node) in &frame.nodes {
                    if let NodeState::Waiting { deadline_unix_ms } = node.state {
                        if node.instance_id <= after {
                            continue;
                        }
                        let definition = workflow
                            .nodes
                            .iter()
                            .find(|n| n.id == *id)
                            .ok_or_else(|| corrupt("missing wait definition"))?;
                        let workflow_ir::NodeKind::Wait { event, .. } = &definition.kind else {
                            return Err(corrupt("waiting state requires a wait node"));
                        };
                        let target = WaitTarget {
                            instance_id: node.instance_id,
                            definition_digest: frame.definition_digest.clone(),
                            input_digest: digest(&node.inputs)?,
                            event: event.clone(),
                        };
                        let policy = r.engine.bundle().wait_policy(&frame.workflow, id).cloned();
                        let subjects = policy
                            .as_ref()
                            .map(|p| {
                                p.subjects
                                    .keys()
                                    .filter_map(|key| {
                                        node.inputs.get(key).map(|v| (key.clone(), v.clone()))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        items.push(WaitRegistration {
                            workflow_id: frame.workflow.id.clone(),
                            workflow_version: frame.workflow.version.clone(),
                            node_id: id.clone(),
                            routes: workflow
                                .edges
                                .iter()
                                .filter(|edge| edge.from == *id)
                                .filter_map(|edge| {
                                    let route = match edge.route {
                                        workflow_ir::Route::Accepted => "accepted",
                                        workflow_ir::Route::Rejected => "rejected",
                                        workflow_ir::Route::TimedOut => "timed_out",
                                        _ => return None,
                                    };
                                    Some((route.into(), edge.to.clone()))
                                })
                                .collect(),
                            policy,
                            subjects,
                            correlation_id: workflow_kernel::signal_correlation(
                                &s.run_digest,
                                &target,
                            )?,
                            target,
                            deadline_unix_ms,
                            paused: s.pause.is_some(),
                        });
                    }
                }
            }
            items.sort_by_key(|e| e.target.instance_id);
            let more = items.len() > limit as usize;
            items.truncate(limit as usize);
            let next_cursor = more.then(|| items.last().unwrap().target.instance_id);
            Ok(Page { items, next_cursor })
        })
    }
}
