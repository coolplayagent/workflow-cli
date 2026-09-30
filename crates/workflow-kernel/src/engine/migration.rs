use super::*;

fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}
fn nodes(bundle: &BundleSpec) -> BTreeMap<MigrationNode, &Node> {
    bundle
        .workflows
        .iter()
        .flat_map(|w| {
            w.nodes.iter().map(move |node| {
                (
                    MigrationNode {
                        workflow: VersionRef {
                            id: w.id.clone(),
                            version: w.version.clone(),
                        },
                        node_id: node.id.clone(),
                    },
                    node,
                )
            })
        })
        .collect()
}
fn timeout(node: &Node) -> Option<u64> {
    match node.kind {
        NodeKind::Wait { timeout_ms, .. } => Some(timeout_ms),
        NodeKind::Loop { deadline_ms, .. } => Some(deadline_ms),
        _ => None,
    }
}
fn gates(bundle: &BundleSpec, node: &MigrationNode) -> Result<String> {
    Ok(workflow_worker::digest(
        &bundle
            .postconditions
            .iter()
            .filter(|p| p.workflow == node.workflow && p.node_id == node.node_id)
            .collect::<Vec<_>>(),
    )?)
}
fn approval(bundle: &BundleSpec, node: &MigrationNode) -> Result<String> {
    Ok(workflow_worker::digest(
        &bundle
            .wait_policies
            .iter()
            .filter(|p| p.workflow == node.workflow && p.node_id == node.node_id)
            .collect::<Vec<_>>(),
    )?)
}

impl Engine {
    pub fn plan_migration(&self, request: &MigrationRequest) -> Result<MigrationPlan> {
        let protected = |spec: &BundleSpec| {
            spec.effect_bindings.iter().any(|b| b.release.is_some())
                || (!spec.postconditions.is_empty()
                    && spec
                        .wait_policies
                        .iter()
                        .any(|w| w.policy.kind == WaitKind::HumanApproval))
        };
        if protected(self.initial_bundle.spec()) || protected(self.bundle.spec()) {
            return Err(invalid(
                "a protected delivery run keeps its original graph and checks; changed contracts require a new run",
            ));
        }
        if self.state.status != RunStatus::Running || self.state.pause.is_none() {
            return Err(invalid(
                "definition migration requires a paused running source",
            ));
        }
        if !workflow_validator::identifier(&request.migration_id)
            || request.decision_summary.trim().is_empty()
            || request.decision_summary.len() > 4096
            || request.decision_summary.contains('\0')
            || request.node_mapping.len() > 4096
        {
            return Err(invalid(
                "migration requires a stable identity, bounded summary and node mapping",
            ));
        }
        let target = CompiledBundle::compile(request.target_bundle.clone())?;
        workflow_validator::validate_values(&target.root().inputs, &request.target_inputs)
            .map_err(|e| invalid(&e.message))?;
        let old = self.bundle.spec();
        let next = target.spec();
        let old_nodes = nodes(old);
        let new_nodes = nodes(next);
        let mut mappings = BTreeMap::new();
        for from in old_nodes.keys() {
            let workflow = if from.workflow == old.root {
                Some(next.root.clone())
            } else if next
                .workflows
                .iter()
                .any(|w| w.id == from.workflow.id && w.version == from.workflow.version)
            {
                Some(from.workflow.clone())
            } else {
                let candidates: Vec<_> = next
                    .workflows
                    .iter()
                    .filter(|w| w.id == from.workflow.id)
                    .collect();
                if candidates.len() == 1 {
                    Some(VersionRef {
                        id: candidates[0].id.clone(),
                        version: candidates[0].version.clone(),
                    })
                } else {
                    None
                }
            };
            let candidate = workflow.map(|workflow| MigrationNode {
                workflow,
                node_id: from.node_id.clone(),
            });
            mappings.insert(
                from.clone(),
                candidate.filter(|n| new_nodes.contains_key(n)),
            );
        }
        let mut overridden = BTreeSet::new();
        for mapping in &request.node_mapping {
            if !old_nodes.contains_key(&mapping.source)
                || !overridden.insert(mapping.source.clone())
                || mapping
                    .target
                    .as_ref()
                    .is_some_and(|n| !new_nodes.contains_key(n))
            {
                return Err(invalid(
                    "node mapping must name unique existing source and target nodes",
                ));
            }
            mappings.insert(mapping.source.clone(), mapping.target.clone());
        }
        let mut selected = BTreeSet::new();
        let mut impacts = vec![];
        for (source, target) in &mappings {
            if target.as_ref().is_some_and(|n| !selected.insert(n.clone())) {
                return Err(invalid("multiple old nodes cannot map to one target node"));
            }
            impacts.push(MigrationNodeImpact {
                source: source.clone(),
                target: target.clone(),
                definition_changed: target
                    .as_ref()
                    .is_none_or(|n| old_nodes[source] != new_nodes[n]),
                gate_changed: match target {
                    Some(n) => gates(old, source)? != gates(next, n)?,
                    None => true,
                },
                approval_policy_changed: match target {
                    Some(n) => approval(old, source)? != approval(next, n)?,
                    None => true,
                },
            });
        }
        let mut invalidated = vec![];
        let mut timers = vec![];
        for (frame_id, frame) in &self.state.frames {
            for (node_id, instance) in &frame.nodes {
                let node = MigrationNode {
                    workflow: frame.workflow.clone(),
                    node_id: node_id.clone(),
                };
                invalidated.push(InvalidatedInstance {
                    frame_id: *frame_id,
                    node: node.clone(),
                    instance_id: instance.instance_id,
                    state: serde_json::to_value(&instance.state)
                        .map_err(|_| invalid("node state encoding"))?["state"]
                        .as_str()
                        .ok_or_else(|| invalid("node state tag missing"))?
                        .into(),
                    input_digest: workflow_worker::digest(&instance.inputs)?,
                    output_digest: workflow_worker::digest(&instance.outputs)?,
                    gate_decision_digest: instance
                        .gate_decision
                        .as_ref()
                        .map(workflow_worker::digest)
                        .transpose()?,
                });
                let deadline = match instance.state {
                    NodeState::Waiting { deadline_unix_ms } => Some(deadline_unix_ms),
                    NodeState::Child {
                        deadline_unix_ms, ..
                    } => deadline_unix_ms,
                    _ => None,
                };
                if let Some(old_deadline_unix_ms) = deadline {
                    let mapped = mappings[&node].clone();
                    timers.push(MigrationTimer {
                        instance_id: instance.instance_id,
                        source: node,
                        old_deadline_unix_ms,
                        target_timeout_ms: mapped.as_ref().and_then(|n| timeout(new_nodes[n])),
                        target: mapped,
                    });
                }
            }
        }
        let mut canonical = request.clone();
        canonical.target_bundle = next.clone();
        canonical
            .node_mapping
            .sort_by(|a, b| a.source.cmp(&b.source));
        let plan = MigrationPlan {
            schema_version: 1,
            run_id: self.state.run_id.clone(),
            run_digest: self.state.run_digest.clone(),
            source_revision: self.state.revision,
            source_state_digest: workflow_worker::digest(&self.state)?,
            source_bundle_digest: self.bundle.digest().into(),
            target_bundle_digest: target.digest().into(),
            inputs_changed: self.state.frames[&1].inputs != request.target_inputs,
            request: canonical,
            nodes: impacts,
            added_nodes: new_nodes
                .keys()
                .filter(|n| !selected.contains(*n))
                .cloned()
                .collect(),
            invalidated_instances: invalidated,
            invalidated_messages: self.state.inbox.keys().cloned().collect(),
            timers,
        };
        // Retained plan/events/checkpoints share the bounded wire encoding.
        workflow_worker::to_message(&plan)?;
        Ok(plan)
    }

    pub(super) fn migrate_definition(
        &mut self,
        plan: &MigrationPlan,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        for timer in &plan.timers {
            commands.push(Command::CancelTimer {
                instance_id: timer.instance_id,
            });
        }
        for entry in self.state.inbox.values_mut() {
            if entry.status == SignalStatus::Pending {
                entry.status = SignalStatus::Rejected {
                    revision: self.state.revision + 1,
                    at_unix_ms: self.state.now_unix_ms,
                    reason: SignalRejection::DefinitionMismatch,
                };
            }
        }
        let target = CompiledBundle::compile(plan.request.target_bundle.clone())?;
        let root = target.spec().root.clone();
        self.state.bundle_digest = target.digest().into();
        self.bundle = Arc::new(target);
        // Frame 1 remains the active root. Historical frames are reconstructed
        // from the original seed plus versioned events; instance IDs never repeat.
        let next_frame = self.state.next_frame_id;
        self.state.frames.clear();
        self.state.next_frame_id = 1;
        self.add_frame(&root, plan.request.target_inputs.clone())?;
        self.state.next_frame_id = self.state.next_frame_id.max(next_frame);
        self.state.pause = Some(Pause {
            reason: format!(
                "definition migration {} requires explicit resume",
                plan.request.migration_id
            ),
            at_unix_ms: self.state.now_unix_ms,
        });
        Ok(())
    }
}
