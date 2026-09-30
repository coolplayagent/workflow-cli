use super::*;
use serde_json::json;
fn fixture<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let root = std::env::var("TEST_SRCDIR")
        .map(|p| std::path::PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap()))
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    serde_json::from_slice(
        &std::fs::read(root.join(format!("examples/templates/{name}.json"))).unwrap(),
    )
    .unwrap()
}
#[test]
fn portable_templates_share_frozen_child_and_explain_all_possible_writes() {
    let mut children = vec![];
    for (kind, writes) in [("defect", 2), ("feature", 0), ("release", 4)] {
        let t: Template = fixture(kind);
        let local: InstanceRequest = fixture(&format!("{kind}-local"));
        let shared: InstanceRequest = fixture(&format!("{kind}-shared"));
        let a = plan(&t, &local).unwrap();
        let b = plan(&t, &shared).unwrap();
        assert_eq!(a.bundle_digest, b.bundle_digest);
        assert_eq!(a.proposed_run_digest, b.proposed_run_digest);
        assert_ne!(a.environment_digest, b.environment_digest);
        assert_eq!(a.actions.iter().filter(|a| a.writes).count(), writes);
        assert_eq!(a.waits.len(), 2);
        assert_eq!(a.gates.len(), 4);
        assert!(
            a.dependencies
                .iter()
                .any(|d| d.id == "sdlc.implementation-attempt")
        );
        children.push(
            t.bundle
                .workflows
                .iter()
                .find(|w| w.id == "sdlc.implementation-attempt")
                .unwrap()
                .clone(),
        );
    }
    assert!(children.windows(2).all(|w| w[0] == w[1]));
}
#[test]
fn rejects_bad_parameters_missing_capabilities_and_unauthorized_environments_before_start() {
    let t: Template = fixture("defect");
    let i: InstanceRequest = fixture("defect-local");
    let mut bad = i.clone();
    bad.parameters.remove("revision");
    assert_eq!(plan(&t, &bad).unwrap_err().path, "parameters.revision");
    bad = i.clone();
    bad.parameters.insert("revision".into(), json!(42));
    assert_eq!(plan(&t, &bad).unwrap_err().path, "parameters.revision");
    bad = i.clone();
    bad.environment.capabilities.clear();
    assert!(
        plan(&t, &bad)
            .unwrap_err()
            .path
            .starts_with("environment.capabilities")
    );
    bad = i.clone();
    bad.environment.capabilities[0].contract_digest = format!("sha256:{}", "0".repeat(64));
    assert!(
        plan(&t, &bad)
            .unwrap_err()
            .path
            .starts_with("environment.capabilities")
    );
    bad = i.clone();
    bad.environment.approvers.clear();
    assert_eq!(plan(&t, &bad).unwrap_err().path, "environment.approvers");
    bad = i.clone();
    bad.environment.effect_targets.clear();
    assert!(
        plan(&t, &bad)
            .unwrap_err()
            .path
            .starts_with("environment.effect_targets")
    );
    bad = i.clone();
    bad.business_facts.insert("emergency-bypass".into());
    assert!(
        plan(&t, &bad)
            .unwrap_err()
            .path
            .starts_with("business_facts")
    );
    let mut fixed = t.clone();
    fixed
        .parameters
        .get_mut("name")
        .unwrap()
        .project_overridable = false;
    fixed.parameters.get_mut("name").unwrap().default = Some(json!("fixed-name"));
    assert_eq!(plan(&fixed, &i).unwrap_err().path, "parameters.name");
    bad = i;
    bad.parameters.remove("name");
    assert_eq!(
        plan(&fixed, &bad).unwrap().request.inputs["name"],
        "fixed-name"
    );
}
#[test]
fn safety_policy_edits_are_definition_changes_requiring_fresh_review() {
    let mut unmanaged: Template = fixture("defect");
    unmanaged.bundle.effect_bindings.clear();
    assert_eq!(
        unmanaged.validate().unwrap_err().path,
        "bundle.effect_bindings"
    );
    let t: Template = fixture("release");
    let mut b = t.clone();
    b.bundle.postconditions[0].policy.requirements[0].max_age_ms += 1;
    let d = diff(&t, &b).unwrap();
    assert!(d.new_review_required && d.mandatory_gates_changed);
    assert!(
        d.changes
            .iter()
            .any(|c| c.path.ends_with("max_age_ms") && c.before == Some(json!(600000)))
    );
    b = t.clone();
    b.bundle.effect_bindings[0]
        .release
        .as_mut()
        .unwrap()
        .approvals
        .clear();
    assert_eq!(b.validate().unwrap_err().path, "bundle.effect_bindings");
    b = t;
    b.budget.max_wait_ms = 1;
    assert_eq!(b.validate().unwrap_err().path, "budget.max_wait_ms");
}
fn candidate() -> Candidate {
    // Synthetic references test validation only. The CLI acceptance matrix
    // produces real manifests/artifacts for all ten execution cases.
    let t: Template = fixture("feature");
    let compiled = t.validate().unwrap();
    let cases = [ExecutionMode::Local, ExecutionMode::Shared]
        .into_iter()
        .flat_map(|mode| {
            Scenario::ALL
                .into_iter()
                .map(move |scenario| (mode, scenario))
        })
        .map(|(mode, scenario)| {
            let accepted = matches!(scenario, Scenario::Success | Scenario::Rework);
            RegressionCase {
                mode,
                scenario,
                run_digest: digest(&"unit-run").unwrap(),
                bundle_digest: compiled.digest().into(),
                report_digest: digest(&"unit-report").unwrap(),
                artifacts: vec![digest(&"unit-artifact").unwrap()],
                terminal_status: if accepted { "succeeded" } else { "failed" }.into(),
                accepted,
                repair_rounds: 2,
            }
        })
        .collect();
    Candidate {
        proposed_by: "author".into(),
        reason: "Unit-test validation".into(),
        compatibility: "Same pinned business contract".into(),
        regressions: Regressions {
            template_digest: t.content_digest().unwrap(),
            cases,
        },
        template: t,
    }
}
#[test]
fn review_binds_exact_candidate_and_complete_regression_matrix() {
    let c = candidate();
    let r = Review {
        candidate_digest: c.content_digest().unwrap(),
        actor: "owner".into(),
        owner_role: c.template.owner_role.clone(),
        decision: ReviewDecision::Approve,
        reason: "Verified unit-test candidate".into(),
        reviewed_at_unix_ms: 1000,
    };
    let p = Publication::new(c.clone(), r.clone()).unwrap();
    p.verify().unwrap();
    let mut invalid = c.clone();
    invalid.regressions.cases.pop();
    assert!(invalid.validate().is_err());
    invalid = c.clone();
    invalid.regressions.cases[0].accepted = false;
    assert!(invalid.validate().is_err());
    invalid = c.clone();
    invalid.reason = "Changed after review".into();
    assert!(Publication::new(invalid, r.clone()).is_err());
    let mut bad = r.clone();
    bad.actor = "author".into();
    assert!(Publication::new(c.clone(), bad).is_err());
    bad = r;
    bad.decision = ReviewDecision::Reject;
    assert!(Publication::new(c, bad).is_err());
}
