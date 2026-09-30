use super::*;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Local,
    Shared,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilityBinding {
    pub capability: VersionRef,
    pub contract_digest: String,
    /// Logical host binding name; contains no provider credential or endpoint.
    pub host_binding: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub mode: ExecutionMode,
    pub capabilities: Vec<CapabilityBinding>,
    pub effect_targets: BTreeSet<String>,
    pub approvers: BTreeSet<String>,
    #[serde(default)]
    pub model_policies: Vec<ModelBinding>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    pub policy: workflow_worker::ModelPolicyBinding,
    pub host_binding: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceRequest {
    pub run_id: String,
    pub parameters: Values,
    pub business_facts: BTreeSet<String>,
    pub environment: Environment,
    pub started_at_unix_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub workflow: VersionRef,
    pub node: String,
    pub capability: VersionRef,
    pub contract_digest: String,
    pub host_binding: String,
    pub writes: bool,
    pub target: Option<VersionRef>,
    pub model_policy: Option<workflow_worker::ModelPolicyBinding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub template_digest: String,
    pub bundle_digest: String,
    /// A shared Start operation supplies authoritative database time, so its
    /// actual run digest can differ from this deterministic local preview.
    pub proposed_run_digest: String,
    pub environment_digest: String,
    pub dependencies: Vec<VersionRef>,
    pub actions: Vec<Action>,
    pub gates: Vec<workflow_kernel::Postcondition>,
    pub waits: Vec<workflow_kernel::WaitPolicyBinding>,
    pub budget: Budget,
    pub request: workflow_runstore::StartRun,
}
/// Pure and deterministic: validate definitions, business facts, parameters and
/// declared host contracts, then explain possible actions without executing any.
pub fn plan(t: &Template, i: &InstanceRequest) -> Result<Plan> {
    let bundle = t.validate()?;
    name(&i.run_id, "run_id")?;
    if i.business_facts.len() > 64 || i.parameters.len() > 64 {
        return Err(invalid("parameters", "instance exceeds bounded inputs"));
    }
    for key in t.applicability.requires.keys() {
        if !i.business_facts.contains(key) {
            return Err(invalid(
                format!("business_facts.{key}"),
                "required business prerequisite is not attested",
            ));
        }
    }
    for key in t.applicability.excludes.keys() {
        if i.business_facts.contains(key) {
            return Err(invalid(
                format!("business_facts.{key}"),
                "template does not apply to this business context",
            ));
        }
    }
    for key in i.parameters.keys() {
        if !t.parameters.contains_key(key) {
            return Err(invalid(format!("parameters.{key}"), "unknown parameter"));
        }
    }
    let mut inputs = Values::new();
    for (key, p) in &t.parameters {
        let path = format!("parameters.{key}");
        let value = i.parameters.get(key).or(p.default.as_ref());
        if !p.project_overridable
            && i.parameters
                .get(key)
                .is_some_and(|v| Some(v) != p.default.as_ref())
        {
            return Err(invalid(path, "project override is not permitted"));
        }
        match value {
            Some(v)
                if p.field.value_type.accepts(v)
                    && (p.allowed_values.is_empty() || p.allowed_values.contains(v)) =>
            {
                inputs.insert(key.clone(), v.clone());
            }
            Some(_) => return Err(invalid(path, "parameter type or allowed value differs")),
            None if p.field.required => return Err(invalid(path, "required parameter missing")),
            None => {}
        }
    }
    let env = &i.environment;
    if env.capabilities.len() > 256 || env.effect_targets.len() > 256 || env.approvers.len() > 128 {
        return Err(invalid("environment", "binding inventory exceeds limits"));
    }
    let mut bound = BTreeMap::new();
    for c in &env.capabilities {
        reference(&c.capability, "environment.capabilities")?;
        name(&c.host_binding, "environment.capabilities.host_binding")?;
        if !hash(&c.contract_digest) || bound.insert(identity_key(&c.capability), c).is_some() {
            return Err(invalid(
                "environment.capabilities",
                "duplicate or invalid capability contract",
            ));
        }
    }
    let mut contracts = BTreeMap::new();
    for c in &t.bundle.capabilities {
        let capability = workflow_worker::Capability::new(c.clone())
            .map_err(|e| invalid("bundle.capabilities", e.message))?;
        let key = identity_key(&c.capability);
        let binding = bound.get(&key).ok_or_else(|| {
            invalid(
                format!("environment.capabilities.{key}"),
                "required host capability missing",
            )
        })?;
        if binding.contract_digest != capability.digest() {
            return Err(invalid(
                format!("environment.capabilities.{key}"),
                "host contract differs from frozen definition",
            ));
        }
        contracts.insert(key, (c, binding));
    }
    for wait in &t.bundle.wait_policies {
        if wait.policy.responders.is_disjoint(&env.approvers) {
            return Err(invalid(
                "environment.approvers",
                "no allowed responder for a required wait",
            ));
        }
    }
    let mut actions = vec![];
    if env.model_policies.len() > 128 {
        return Err(invalid(
            "environment.model_policies",
            "model binding inventory exceeds limit",
        ));
    }
    let mut models = BTreeMap::new();
    for m in &env.model_policies {
        m.policy
            .validate()
            .map_err(|_| invalid("environment.model_policies", "invalid model policy binding"))?;
        name(&m.host_binding, "environment.model_policies.host_binding")?;
        if models.insert(identity_key(&m.policy.policy), m).is_some() {
            return Err(invalid(
                "environment.model_policies",
                "duplicate model policy binding",
            ));
        }
    }
    for p in &t.bundle.model_policies {
        let checked = workflow_models::Policy::new(p.clone())
            .map_err(|e| invalid("bundle.model_policies", e.message))?;
        if models
            .get(&identity_key(&p.policy))
            .is_none_or(|b| &b.policy != checked.binding())
        {
            return Err(invalid(
                "environment.model_policies",
                "required frozen model policy is missing or differs",
            ));
        }
    }
    for w in &t.bundle.workflows {
        for node in &w.nodes {
            if let NodeKind::Task { capability, policy } = &node.kind {
                let (c, b) = contracts[&identity_key(capability)];
                let target = t
                    .bundle
                    .effect_bindings
                    .iter()
                    .find(|e| {
                        e.workflow.id == w.id
                            && e.workflow.version == w.version
                            && e.node_id == node.id
                    })
                    .map(|e| e.policy.target.clone());
                if target
                    .as_ref()
                    .is_some_and(|target| !env.effect_targets.contains(&identity_key(target)))
                {
                    return Err(invalid(
                        format!("environment.effect_targets.{}", node.id),
                        "effect target is outside project authorization",
                    ));
                }
                actions.push(Action {
                    workflow: VersionRef {
                        id: w.id.clone(),
                        version: w.version.clone(),
                    },
                    node: node.id.clone(),
                    capability: capability.clone(),
                    contract_digest: b.contract_digest.clone(),
                    host_binding: policy
                        .as_ref()
                        .map(|p| models[&identity_key(p)].host_binding.clone())
                        .unwrap_or_else(|| b.host_binding.clone()),
                    writes: c.effects != workflow_worker::EffectContract::ReadOnly,
                    target,
                    model_policy: policy
                        .as_ref()
                        .map(|p| models[&identity_key(p)].policy.clone()),
                });
            }
        }
    }
    let (engine, _) = workflow_kernel::Engine::start(
        bundle.clone(),
        &i.run_id,
        inputs.clone(),
        i.started_at_unix_ms,
        workflow_kernel::Limits::default(),
    )
    .map_err(|e| invalid("parameters", e.message))?;
    Ok(Plan {
        template_digest: t.content_digest()?,
        bundle_digest: bundle.digest().into(),
        proposed_run_digest: engine.snapshot().run_digest.clone(),
        environment_digest: digest(env)?,
        dependencies: t
            .bundle
            .workflows
            .iter()
            .map(|w| VersionRef {
                id: w.id.clone(),
                version: w.version.clone(),
            })
            .collect(),
        actions,
        gates: t.bundle.postconditions.clone(),
        waits: t.bundle.wait_policies.clone(),
        budget: t.budget.clone(),
        request: workflow_runstore::StartRun {
            schema_version: 1,
            run_id: i.run_id.clone(),
            bundle: t.bundle.clone(),
            inputs,
            started_at_unix_ms: i.started_at_unix_ms,
            limits: workflow_kernel::Limits::default(),
        },
    })
}
#[derive(Clone, Debug, Serialize)]
pub struct TemplateDiff {
    pub source_digest: String,
    pub target_digest: String,
    pub changed_sections: Vec<String>,
    pub changes: Vec<Change>,
    pub mandatory_gates_changed: bool,
    pub new_review_required: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Change {
    /// JSON pointer into the reviewed template, including complete before/after
    /// values for additions, removals and replacements.
    pub path: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
}
fn changes(
    path: String,
    a: Option<&serde_json::Value>,
    b: Option<&serde_json::Value>,
    result: &mut Vec<Change>,
) {
    if a == b {
        return;
    }
    if let (Some(serde_json::Value::Object(a)), Some(serde_json::Value::Object(b))) = (a, b) {
        for key in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
            changes(
                format!("{}/{}", path, key.replace('~', "~0").replace('/', "~1")),
                a.get(key),
                b.get(key),
                result,
            );
        }
    } else if let (Some(serde_json::Value::Array(a)), Some(serde_json::Value::Array(b))) = (a, b) {
        for index in 0..a.len().max(b.len()) {
            changes(
                format!("{path}/{index}"),
                a.get(index),
                b.get(index),
                result,
            );
        }
    } else {
        result.push(Change {
            path,
            before: a.cloned(),
            after: b.cloned(),
        });
    }
}
pub fn diff(a: &Template, b: &Template) -> Result<TemplateDiff> {
    let source_digest = a.content_digest()?;
    let target_digest = b.content_digest()?;
    let av = serde_json::to_value(a).map_err(|_| invalid("$", "encoding failed"))?;
    let bv = serde_json::to_value(b).map_err(|_| invalid("$", "encoding failed"))?;
    let changed_sections = av
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| av[*k] != bv[*k])
        .cloned()
        .collect();
    let mut details = vec![];
    changes(String::new(), Some(&av), Some(&bv), &mut details);
    Ok(TemplateDiff {
        source_digest: source_digest.clone(),
        target_digest: target_digest.clone(),
        changed_sections,
        changes: details,
        mandatory_gates_changed: a.bundle.postconditions != b.bundle.postconditions
            || a.bundle.wait_policies != b.bundle.wait_policies
            || a.bundle.effect_bindings != b.bundle.effect_bindings,
        new_review_required: source_digest != target_digest,
    })
}
