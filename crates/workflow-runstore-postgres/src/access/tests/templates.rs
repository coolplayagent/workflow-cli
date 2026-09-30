use super::*;
use workflow_templates::*;
fn proposed() -> Candidate {
    let root = std::env::var("TEST_SRCDIR")
        .map(|p| std::path::PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap()))
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    let template: Template = serde_json::from_slice(
        &std::fs::read(root.join("examples/templates/feature.json")).unwrap(),
    )
    .unwrap();
    let compiled = template.validate().unwrap();
    // Only transaction/authorization tests use these synthetic references.
    // examples/templates/acceptance.py supplies actual execution evidence.
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
        proposed_by: "forged-owner".into(),
        reason: "Authorization contract fixture".into(),
        compatibility: "Initial fixture".into(),
        regressions: Regressions {
            template_digest: template.content_digest().unwrap(),
            cases,
        },
        template,
    }
}
fn policy() -> OwnerPolicy {
    OwnerPolicy {
        owners: [(
            "sdlc-owner".into(),
            ["author".into(), "owner".into(), "other".into()].into(),
        )]
        .into(),
    }
}
#[test]
#[ignore = "requires isolated WORKFLOW_TEST_POSTGRES"]
fn template_publication_authenticates_independent_owner_and_scope() {
    let mut f = Fixture::new();
    AuthenticatedService::configure_template_owners(
        &mut client(),
        &f.tenant,
        "project",
        None,
        &policy(),
    )
    .unwrap();
    let author = f.credential("author", Role::DefinitionMaintainer);
    let owner = f.credential("owner", Role::Approver);
    let self_review = f.credential("author", Role::Approver);
    let unlisted = f.credential("unlisted", Role::Approver);
    let viewer = f.credential("viewer", Role::Viewer);
    let candidate = proposed();
    assert!(
        f.service
            .propose_template(viewer.expose_secret(), &candidate)
            .is_err()
    );
    let key = f
        .service
        .propose_template(author.expose_secret(), &candidate)
        .unwrap();
    assert_eq!(
        f.service
            .template_candidate(viewer.expose_secret(), &key)
            .unwrap()
            .proposed_by,
        "author"
    );
    assert!(
        f.service
            .publish_template(author.expose_secret(), &key)
            .is_err()
    );
    for token in [
        author.expose_secret(),
        self_review.expose_secret(),
        unlisted.expose_secret(),
    ] {
        assert!(
            f.service
                .review_template(token, &key, ReviewDecision::Approve, "Attempted review")
                .is_err()
        );
    }
    let review = f
        .service
        .review_template(
            owner.expose_secret(),
            &key,
            ReviewDecision::Approve,
            "Owner checked fixture evidence",
        )
        .unwrap();
    assert_eq!(
        review,
        f.service
            .review_template(
                owner.expose_secret(),
                &key,
                ReviewDecision::Approve,
                "Owner checked fixture evidence"
            )
            .unwrap()
    );
    assert!(
        f.service
            .publish_template(owner.expose_secret(), &key)
            .is_err()
    );
    let published = f
        .service
        .publish_template(author.expose_secret(), &key)
        .unwrap();
    assert_eq!(
        published,
        f.service
            .publish_template(author.expose_secret(), &key)
            .unwrap()
    );
    let mut other = Fixture::new();
    let reader = other.credential("reader", Role::Viewer);
    assert!(
        other
            .service
            .template_candidate(reader.expose_secret(), &key)
            .is_err()
    );
    assert!(
        other
            .service
            .template_publication(reader.expose_secret(), &candidate.template.identity)
            .is_err()
    );
    let runner = f.credential("runner", Role::Runner);
    let mut instance: InstanceRequest = serde_json::from_value(serde_json::json!({"run_id":"template-start","parameters":{"request":"add numbers","repository":"repo","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","name":"candidate"},"business_facts":["pinned-source","isolated-tests"],"environment":{"mode":"shared","capabilities":[],"effect_targets":[],"approvers":["delivery-reviewer"]},"started_at_unix_ms":1000})).unwrap();
    instance.environment.capabilities = candidate
        .template
        .bundle
        .capabilities
        .iter()
        .map(|c| CapabilityBinding {
            capability: c.capability.clone(),
            contract_digest: workflow_worker::Capability::new(c.clone())
                .unwrap()
                .digest()
                .into(),
            host_binding: "worker".into(),
        })
        .collect();
    let start = plan(&published.candidate.template, &instance)
        .unwrap()
        .request;
    assert!(f.service.start(runner.expose_secret(), &start).is_ok());
    let before = f
        .service
        .get(runner.expose_secret(), &start.run_id)
        .unwrap();
    let mut upgraded = published.candidate.clone();
    upgraded.reason = "Upgrade reusable implementation child without changing existing runs".into();
    upgraded.template.identity.version = "2.0.0".into();
    upgraded.template.bundle.root.version = "2.0.0".into();
    for flow in &mut upgraded.template.bundle.workflows {
        flow.version = "2.0.0".into();
        for node in &mut flow.nodes {
            if let workflow_ir::NodeKind::Subworkflow { workflow } = &mut node.kind {
                workflow.version = "2.0.0".into();
            }
        }
    }
    for gate in &mut upgraded.template.bundle.postconditions {
        gate.workflow.version = "2.0.0".into();
    }
    for wait in &mut upgraded.template.bundle.wait_policies {
        wait.workflow.version = "2.0.0".into();
    }
    upgraded.regressions.template_digest = upgraded.template.content_digest().unwrap();
    let new_bundle = upgraded.template.validate().unwrap();
    for case in &mut upgraded.regressions.cases {
        case.bundle_digest = new_bundle.digest().into();
    }
    let next = f
        .service
        .propose_template(author.expose_secret(), &upgraded)
        .unwrap();
    f.service
        .review_template(
            owner.expose_secret(),
            &next,
            ReviewDecision::Approve,
            "Reviewed child upgrade fixture",
        )
        .unwrap();
    f.service
        .publish_template(author.expose_secret(), &next)
        .unwrap();
    let after = f
        .service
        .get(runner.expose_secret(), &start.run_id)
        .unwrap();
    assert_eq!(before, after);
    assert_ne!(after.bundle_digest, new_bundle.digest());
    assert_eq!(
        f.service
            .template_publication(
                viewer.expose_secret(),
                &published.candidate.template.identity
            )
            .unwrap(),
        published
    );
}
#[test]
#[ignore = "requires isolated WORKFLOW_TEST_POSTGRES"]
fn changed_owner_policy_invalidates_unpublished_reviews_and_rejections_stay_final() {
    let mut f = Fixture::new();
    let author = f.credential("author", Role::DefinitionMaintainer);
    let owner = f.credential("owner", Role::Approver);
    assert_eq!(
        AuthenticatedService::configure_template_owners(
            &mut client(),
            &f.tenant,
            "project",
            None,
            &policy()
        )
        .unwrap(),
        1
    );
    let mut candidate = proposed();
    let first = f
        .service
        .propose_template(author.expose_secret(), &candidate)
        .unwrap();
    f.service
        .review_template(
            owner.expose_secret(),
            &first,
            ReviewDecision::Approve,
            "Initial owner policy",
        )
        .unwrap();
    assert!(
        AuthenticatedService::configure_template_owners(
            &mut client(),
            &f.tenant,
            "project",
            None,
            &policy()
        )
        .is_err()
    );
    assert_eq!(
        AuthenticatedService::configure_template_owners(
            &mut client(),
            &f.tenant,
            "project",
            Some(1),
            &policy()
        )
        .unwrap(),
        2
    );
    assert!(
        f.service
            .publish_template(author.expose_secret(), &first)
            .is_err()
    );
    assert!(
        f.service
            .review_template(
                owner.expose_secret(),
                &first,
                ReviewDecision::Approve,
                "Initial owner policy"
            )
            .is_err()
    );
    candidate.reason = "Fresh candidate after owner policy change".into();
    let second = f
        .service
        .propose_template(author.expose_secret(), &candidate)
        .unwrap();
    f.service
        .review_template(
            owner.expose_secret(),
            &second,
            ReviewDecision::Reject,
            "Needs stronger acceptance",
        )
        .unwrap();
    assert!(
        f.service
            .publish_template(author.expose_secret(), &second)
            .is_err()
    );
    assert!(
        f.service
            .review_template(
                owner.expose_secret(),
                &second,
                ReviewDecision::Approve,
                "Override rejection"
            )
            .is_err()
    );
    candidate.reason = "Corrected candidate reviewed under current owner policy".into();
    let third = f
        .service
        .propose_template(author.expose_secret(), &candidate)
        .unwrap();
    f.service
        .review_template(
            owner.expose_secret(),
            &third,
            ReviewDecision::Approve,
            "Current owner policy",
        )
        .unwrap();
    assert!(
        f.service
            .publish_template(author.expose_secret(), &third)
            .is_ok()
    );
}
#[test]
#[ignore = "requires isolated WORKFLOW_TEST_POSTGRES"]
fn competing_template_reviews_cannot_overwrite_each_other() {
    let mut f = Fixture::new();
    AuthenticatedService::configure_template_owners(
        &mut client(),
        &f.tenant,
        "project",
        None,
        &policy(),
    )
    .unwrap();
    let author = f.credential("author", Role::DefinitionMaintainer);
    let owners = [
        f.credential("owner", Role::Approver),
        f.credential("other", Role::Approver),
    ];
    let key = f
        .service
        .propose_template(author.expose_secret(), &proposed())
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = owners
        .into_iter()
        .map(|owner| {
            let key = key.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut service = AuthenticatedService::open(client()).unwrap();
                barrier.wait();
                service.review_template(
                    owner.expose_secret(),
                    &key,
                    ReviewDecision::Approve,
                    "Concurrent owner",
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let winner = results
        .into_iter()
        .find_map(std::result::Result::ok)
        .unwrap();
    assert_eq!(
        f.service
            .publish_template(author.expose_secret(), &key)
            .unwrap()
            .review,
        winner
    );
}
