use super::*;
use crate::tests::Database;
use std::{
    collections::BTreeMap,
    sync::{Arc, Barrier},
};
fn sample() -> Template {
    let root = std::env::var("TEST_SRCDIR")
        .map(|p| std::path::PathBuf::from(p).join(std::env::var("TEST_WORKSPACE").unwrap()))
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    serde_json::from_slice(&std::fs::read(root.join("examples/templates/feature.json")).unwrap())
        .unwrap()
}
fn proposed(template: Template) -> Candidate {
    // Synthetic evidence validates catalog transactions, not business execution.
    let compiled = template.validate().unwrap();
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
        reason: "Exercise catalog contracts".into(),
        compatibility: "Immutable versions".into(),
        regressions: Regressions {
            template_digest: template.content_digest().unwrap(),
            cases,
        },
        template,
    }
}
fn owners() -> OwnerPolicy {
    OwnerPolicy {
        owners: BTreeMap::from([(
            "sdlc-owner".into(),
            ["owner", "other", "author"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )]),
    }
}
fn approve(c: &mut SqliteTemplateCatalog, candidate: &Candidate) -> String {
    let key = c.propose(candidate, "author").unwrap();
    c.review(
        &key,
        "owner",
        ReviewDecision::Approve,
        "Review unit regression references",
        1000,
    )
    .unwrap();
    key
}
#[test]
fn publication_is_reviewed_immutable_and_preserves_shared_child_contracts() {
    let db = Database::new();
    let mut c = SqliteTemplateCatalog::create(&db.0, &owners()).unwrap();
    let first = proposed(sample());
    let key = c.propose(&first, "author").unwrap();
    assert!(c.publish(&key).is_err());
    assert!(
        c.review(&key, "unlisted", ReviewDecision::Approve, "No grant", 1000)
            .is_err()
    );
    assert!(
        c.review(&key, "author", ReviewDecision::Approve, "Self review", 1000)
            .is_err()
    );
    let key = approve(&mut c, &first);
    let original = c.publish(&key).unwrap();
    assert_eq!(original, c.publish(&key).unwrap());
    let mut changed = first.clone();
    changed.reason = "Different review metadata".into();
    let changed_key = approve(&mut c, &changed);
    assert!(c.publish(&changed_key).is_err());
    let mut v2 = sample();
    v2.identity.version = "2.0.0".into();
    v2.bundle.root.version = "2.0.0".into();
    v2.bundle.workflows[0].version = "2.0.0".into();
    for p in &mut v2.bundle.postconditions {
        p.workflow.version = "2.0.0".into();
    }
    for p in &mut v2.bundle.wait_policies {
        p.workflow.version = "2.0.0".into();
    }
    v2.bundle.workflows[1].nodes[0].bindings.insert(
        "feedback".into(),
        workflow_ir::Binding::Literal {
            value: serde_json::json!("New child behavior"),
        },
    );
    let conflict = approve(&mut c, &proposed(v2.clone()));
    assert!(
        c.publish(&conflict)
            .unwrap_err()
            .message
            .contains("shared subworkflow")
    );
    v2.bundle.workflows[1].version = "2.0.0".into();
    for node in &mut v2.bundle.workflows[0].nodes {
        if let workflow_ir::NodeKind::Subworkflow { workflow } = &mut node.kind {
            workflow.version = "2.0.0".into();
        }
    }
    let new_key = approve(&mut c, &proposed(v2.clone()));
    let next = c.publish(&new_key).unwrap();
    assert_ne!(next.digest, original.digest);
    drop(c);
    let reopened = SqliteTemplateCatalog::open(&db.0, true).unwrap();
    assert_eq!(reopened.get(&first.template.identity).unwrap(), original);
    assert_eq!(reopened.get(&v2.identity).unwrap(), next);
    assert!(
        SqliteTemplateCatalog::create(
            &db.0,
            &OwnerPolicy {
                owners: BTreeMap::from([("sdlc-owner".into(), ["replacement".into()].into())])
            }
        )
        .is_err()
    );
}
#[test]
fn concurrent_owner_reviews_commit_one_immutable_winner() {
    let db = Database::new();
    let mut c = SqliteTemplateCatalog::create(&db.0, &owners()).unwrap();
    let key = c.propose(&proposed(sample()), "author").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["owner", "other"]
        .into_iter()
        .map(|actor| {
            let path = db.0.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut c = SqliteTemplateCatalog::open(path, false).unwrap();
                barrier.wait();
                c.review(
                    &key,
                    actor,
                    ReviewDecision::Approve,
                    "Concurrent review",
                    1000,
                )
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    let winner = outcomes.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(
        c.review(
            &key,
            &winner.actor,
            ReviewDecision::Approve,
            "Concurrent review",
            2000
        )
        .unwrap(),
        winner
    );
    assert!(
        c.review(
            &key,
            &winner.actor,
            ReviewDecision::Reject,
            "Changed mind",
            2000
        )
        .is_err()
    );
    assert_eq!(c.publish(&key).unwrap().review, winner);
}
