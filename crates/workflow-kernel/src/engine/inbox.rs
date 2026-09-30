use super::*;

impl Engine {
    /// Artifact subjects are frozen wait inputs, never supplied by a responder.
    pub fn wait_subjects(&self, instance_id: u64) -> Result<Vec<workflow_artifacts::ArtifactLink>> {
        let (frame, id) = self.locate(instance_id)?;
        let Some(policy) = self
            .bundle
            .wait_policy(&self.state.frames[&frame].workflow, &id)
        else {
            return Ok(vec![]);
        };
        policy
            .subjects
            .iter()
            .filter(|(_, kind)| **kind == SubjectKind::Artifact)
            .map(|(name, _)| {
                serde_json::from_value(
                    self.record(frame, &id)
                        .inputs
                        .get(name)
                        .cloned()
                        .unwrap_or_default(),
                )
                .map_err(|_| {
                    Error::new(
                        ErrorCode::InvalidSignal,
                        "approval artifact subject is invalid",
                    )
                })
            })
            .collect()
    }

    pub(super) fn receive_signal(&mut self, message: &SignalMessage) -> Result<()> {
        validate_signal(&self.state.run_digest, message)?;
        if self.state.inbox.contains_key(&message.message_id) {
            return Err(Error::new(
                ErrorCode::EventConflict,
                "message ID already received; retry the original event identity",
            ));
        }
        if self.state.inbox.len() >= MAX_INBOX_MESSAGES {
            return Err(Error::new(
                ErrorCode::BudgetExceeded,
                "durable inbox message limit reached",
            ));
        }
        self.state.inbox.insert(
            message.message_id.clone(),
            InboxEntry {
                message: message.clone(),
                received_revision: self.state.revision + 1,
                received_at_unix_ms: self.state.now_unix_ms,
                status: SignalStatus::Pending,
            },
        );
        Ok(())
    }

    pub(super) fn settle_inbox(&mut self, commands: &mut Vec<Command>) -> Result<bool> {
        let mut pending: Vec<_> = self
            .state
            .inbox
            .values()
            .filter(|e| e.status == SignalStatus::Pending)
            .map(|e| (e.received_revision, e.message.clone()))
            .collect();
        pending.sort_by_key(|(revision, _)| *revision);
        let mut progress = false;
        for (_, message) in pending {
            let rejection = self.signal_rejection(&message)?;
            let revision = self.state.revision + 1;
            let at_unix_ms = self.state.now_unix_ms;
            let status = if let Some(reason) = rejection {
                SignalStatus::Rejected {
                    revision,
                    at_unix_ms,
                    reason,
                }
            } else {
                let (frame, id) = self.locate(message.target.instance_id)?;
                if self.state.pause.is_some() || self.record(frame, &id).state == NodeState::Pending
                {
                    continue;
                }
                let NodeState::Waiting { deadline_unix_ms } = self.record(frame, &id).state else {
                    return Err(Error::new(ErrorCode::InvalidSignal, "wait is not active"));
                };
                self.signal(
                    message.target.instance_id,
                    &message.target.event,
                    message.decision == SignalDecision::Approve,
                    &message.outputs,
                    commands,
                )?;
                SignalStatus::Applied {
                    revision,
                    at_unix_ms,
                    admission_deadline_unix_ms: deadline_unix_ms.min(message.expires_at_unix_ms),
                }
            };
            self.state
                .inbox
                .get_mut(&message.message_id)
                .unwrap()
                .status = status;
            progress = true;
        }
        Ok(progress)
    }

    fn signal_rejection(&self, message: &SignalMessage) -> Result<Option<SignalRejection>> {
        use SignalRejection::*;
        if matches!(
            self.state.status,
            RunStatus::Cancelled | RunStatus::Cancelling
        ) {
            return Ok(Some(RunCancelled));
        }
        let Ok((frame, id)) = self.locate(message.target.instance_id) else {
            return Ok(Some(UnknownInstance));
        };
        let node = self.node(frame, &id);
        let NodeKind::Wait { event, .. } = &node.kind else {
            return Ok(Some(NotAWait));
        };
        if self.state.frames[&frame].definition_digest != message.target.definition_digest {
            return Ok(Some(DefinitionMismatch));
        }
        if event != &message.target.event {
            return Ok(Some(EventMismatch));
        }
        let record = self.record(frame, &id);
        if record.reason.as_deref() == Some("timed_out")
            || matches!(record.state, NodeState::Waiting { deadline_unix_ms } if deadline_unix_ms <= self.state.now_unix_ms)
        {
            return Ok(Some(WaitExpired));
        }
        if record.state.terminal() {
            return Ok(Some(AlreadySettled));
        }
        if message.expires_at_unix_ms <= self.state.now_unix_ms {
            return Ok(Some(Expired));
        }
        if workflow_validator::validate_values(&node.outputs, &message.outputs).is_err()
            && message.decision == SignalDecision::Approve
        {
            return Ok(Some(InvalidOutputs));
        }
        if let Some(reason) = crate::wait_policy::rejection(
            self.bundle
                .wait_policy(&self.state.frames[&frame].workflow, &id),
            message,
            self.state.inbox[&message.message_id].received_at_unix_ms,
            (record.state != NodeState::Pending).then_some(&record.inputs),
        ) {
            return Ok(Some(reason));
        }
        if record.state != NodeState::Pending
            && workflow_worker::digest(&record.inputs)? != message.target.input_digest
        {
            return Ok(Some(InputMismatch));
        }
        Ok(None)
    }
}
