use super::*;

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn audit_export_is_complete_scoped_role_checked_and_tamper_evident() {
    let mut a = Fixture::new();
    let mut b = Fixture::new();
    let recovery = a.credential("recovery", Role::Recovery);
    for role in [
        Role::DefinitionMaintainer,
        Role::Viewer,
        Role::Runner,
        Role::Approver,
        Role::SignalSource,
        Role::Scheduler,
        Role::Worker,
    ] {
        let credential = a.credential("limited", role);
        assert_eq!(
            a.service
                .export_audit(credential.expose_secret())
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
    }
    let export = a.service.export_audit(a.admin.expose_secret()).unwrap();
    export.verify().unwrap();
    assert_eq!(export.tenant, a.tenant);
    assert_eq!(export.project, "project");
    assert_eq!(
        export
            .entries
            .iter()
            .filter(|e| e.operation == "export_audit" && e.outcome == "denied")
            .count(),
        7
    );
    assert!(
        !export
            .entries
            .iter()
            .any(|e| e.operation == "export_audit" && e.outcome == "accepted")
    );
    let later = a.service.export_audit(recovery.expose_secret()).unwrap();
    assert_eq!(later.entries.len(), export.entries.len() + 1);
    assert_eq!(later.entries.last().unwrap().operation, "export_audit");
    let other = b.service.export_audit(b.admin.expose_secret()).unwrap();
    assert_eq!(other.tenant, b.tenant);
    let serialized = serde_json::to_string(&export).unwrap();
    for token in [
        a.admin.expose_secret(),
        b.admin.expose_secret(),
        recovery.expose_secret(),
    ] {
        assert!(!serialized.contains(token));
    }
    assert!(!serialized.contains(&b.tenant));
    let mut forged = export.clone();
    forged.tenant = b.tenant;
    assert!(forged.verify().is_err());
    let mut forged = export.clone();
    forged.entries[0].outcome = "edited".into();
    assert!(forged.verify().is_err());
    let mut forged = export;
    forged.entries.reverse();
    assert!(forged.verify().is_err());
    // A bounded export must fail whole rather than silently truncate history.
    client().execute("INSERT INTO workflow_access.audit(tenant,project,actor,credential_id,operation,resource,outcome) SELECT $1,'project','fixture','fixture','fixture','fixture','accepted' FROM generate_series(1,10001)", &[&a.tenant]).unwrap();
    assert!(a.service.export_audit(a.admin.expose_secret()).is_err());
}

#[test]
#[ignore = "requires disposable PostgreSQL; mandatory postgres CI job runs this"]
fn provider_lease_principal_is_checked_in_the_assignment_delivery_transaction() {
    let start = super::effects::effect_request();
    let (mut f, scheduler, worker, lease) = super::effects::setup(&start);
    let assignment = super::effects::dispatch(&mut f, &scheduler, &worker, &lease);
    let principal = workflow_credentials::Principal {
        tenant: f.tenant.clone(),
        project: "project".into(),
        actor: "effect-worker".into(),
    };
    for changed in 0..3 {
        let mut wrong = principal.clone();
        match changed {
            0 => wrong.tenant = "elsewhere".into(),
            1 => wrong.project = "elsewhere".into(),
            _ => wrong.actor = "elsewhere".into(),
        };
        assert_eq!(
            f.service
                .effect_assignment_bound(worker.expose_secret(), &assignment, Some(&wrong))
                .unwrap_err()
                .code,
            ErrorCode::Unauthorized
        );
    }
    // Failed principal checks did not consume single-use delivery.
    let attempt = f
        .service
        .effect_assignment_bound(worker.expose_secret(), &assignment, Some(&principal))
        .unwrap();
    assert_eq!(attempt.intent.run_id, start.run_id);
    assert_eq!(
        f.service
            .effect_assignment_bound(worker.expose_secret(), &assignment, Some(&principal))
            .unwrap_err()
            .code,
        ErrorCode::ReceiptConflict
    );
    f.service
        .revoke(f.admin.expose_secret(), &worker.id)
        .unwrap();
    assert!(
        f.service
            .observe_assigned_effect(
                worker.expose_secret(),
                &assignment,
                &super::effects::receipt(&attempt)
            )
            .is_err()
    );
    let outstanding = f
        .service
        .outstanding_effects(f.admin.expose_secret(), "", 100)
        .unwrap();
    assert!(
        outstanding
            .items
            .iter()
            .any(|a| a.assignment_id == assignment && a.worker_revoked && a.delivered)
    );
}
