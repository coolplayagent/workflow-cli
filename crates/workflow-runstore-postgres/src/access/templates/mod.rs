use super::*;
use workflow_templates::{Candidate, OwnerPolicy, Publication, Review, ReviewDecision};

fn invalid(_: workflow_templates::Error) -> Error {
    Error::new(
        ErrorCode::InvalidRequest,
        "invalid template or review contract",
    )
}
fn check(tx: &mut Transaction<'_>) -> Result<()> {
    let exists: bool = tx
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_templates')",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if !exists {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "template owners are not configured",
        ));
    }
    let version: i32 = tx
        .query_one(
            "SELECT version FROM workflow_templates.schema_version WHERE singleton=true",
            &[],
        )
        .map_err(storage)?
        .get(0);
    if version != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedStorage,
            "unsupported template catalog schema",
        ));
    }
    Ok(())
}
fn candidate(tx: &mut Transaction<'_>, who: &Identity, digest: &str) -> Result<Candidate> {
    let row=tx.query_opt("SELECT document FROM workflow_templates.candidates WHERE tenant=$1 AND project=$2 AND digest=$3 FOR SHARE",&[&who.tenant,&who.project,&digest]).map_err(storage)?.ok_or_else(denied)?;
    let c: Candidate =
        serde_json::from_str(row.get(0)).map_err(|_| corrupt("invalid template candidate"))?;
    if c.content_digest().map_err(invalid)? != digest {
        return Err(corrupt("template candidate digest differs"));
    }
    Ok(c)
}
fn owners(tx: &mut Transaction<'_>, who: &Identity) -> Result<(i64, OwnerPolicy)> {
    let row=tx.query_opt("SELECT revision,document FROM workflow_templates.owners WHERE tenant=$1 AND project=$2 FOR SHARE",&[&who.tenant,&who.project]).map_err(storage)?.ok_or_else(denied)?;
    let policy: OwnerPolicy =
        serde_json::from_str(row.get(1)).map_err(|_| corrupt("invalid template owner policy"))?;
    policy.validate().map_err(invalid)?;
    Ok((row.get(0), policy))
}
fn publication(
    tx: &mut Transaction<'_>,
    who: &Identity,
    id: &workflow_ir::VersionRef,
) -> Result<Publication> {
    let row=tx.query_opt("SELECT digest,document FROM workflow_templates.publications WHERE tenant=$1 AND project=$2 AND id=$3 AND version=$4",&[&who.tenant,&who.project,&id.id,&id.version]).map_err(storage)?.ok_or_else(denied)?;
    let result: Publication =
        serde_json::from_str(row.get(1)).map_err(|_| corrupt("invalid template publication"))?;
    result.verify().map_err(invalid)?;
    if result.digest != row.get::<_, String>(0) || &result.candidate.template.identity != id {
        return Err(corrupt("template publication identity differs"));
    }
    Ok(result)
}
impl AuthenticatedService {
    /// Trusted host provisioning, absent from RPC. Existing owner policies use
    /// revision CAS, and their history is preserved independently from reviews.
    pub fn configure_template_owners(
        client: &mut Client,
        tenant: &str,
        project: &str,
        expected: Option<u64>,
        policy: &OwnerPolicy,
    ) -> Result<u64> {
        validate_id(tenant)?;
        validate_id(project)?;
        policy.validate().map_err(invalid)?;
        let mut tx = client.transaction().map_err(storage)?;
        PostgresRunStore::transaction_settings(&mut tx)?;
        check_schema(&mut tx)?;
        tx.query_one("SELECT pg_advisory_xact_lock(57465236)", &[])
            .map_err(storage)?;
        let exists: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='workflow_templates')",
                &[],
            )
            .map_err(storage)?
            .get(0);
        if !exists {
            tx.batch_execute(include_str!("schema.sql"))
                .map_err(storage)?;
        }
        check(&mut tx)?;
        if !tx.query_one("SELECT EXISTS(SELECT 1 FROM workflow_access.credentials WHERE tenant=$1 AND project=$2)",&[&tenant,&project]).map_err(storage)?.get::<_,bool>(0) {return Err(denied());}
        let old:Option<i64>=tx.query_opt("SELECT revision FROM workflow_templates.owners WHERE tenant=$1 AND project=$2 FOR UPDATE",&[&tenant,&project]).map_err(storage)?.map(|r|r.get(0));
        if old.map(|v| v as u64) != expected {
            return Err(Error::new(
                ErrorCode::BindingConflict,
                "template owner policy revision differs",
            ));
        }
        let revision = old
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| corrupt("owner revision exhausted"))?;
        let document =
            serde_json::to_string(policy).map_err(|_| corrupt("owner policy encoding failed"))?;
        let at = now(&mut tx)?;
        tx.execute("INSERT INTO workflow_templates.owners VALUES($1,$2,$3,$4) ON CONFLICT(tenant,project) DO UPDATE SET revision=EXCLUDED.revision,document=EXCLUDED.document",&[&tenant,&project,&revision,&document]).map_err(storage)?;
        tx.execute(
            "INSERT INTO workflow_templates.owner_history VALUES($1,$2,$3,$4,$5)",
            &[&tenant, &project, &revision, &document, &at],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(revision as u64)
    }
    pub fn propose_template(&mut self, token: &str, proposed: &Candidate) -> Result<String> {
        self.transact(token,&[Role::DefinitionMaintainer],"propose_template","template",|tx,who| {
            check(tx)?; owners(tx,who)?;
            let mut c=proposed.clone(); c.proposed_by=who.actor.clone();
            let digest=c.content_digest().map_err(invalid)?;
            let document=serde_json::to_string(&c).map_err(|_|corrupt("candidate encoding failed"))?;
            tx.execute("INSERT INTO workflow_templates.candidates VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",&[&who.tenant,&who.project,&digest,&document]).map_err(storage)?;
            if candidate(tx,who,&digest)?!=c {return Err(corrupt("candidate identity conflict"));}
            audit(tx,who,"template_proposed",&digest,"accepted")?;
            Ok(digest)
        })
    }
    pub fn template_candidate(&mut self, token: &str, digest: &str) -> Result<Candidate> {
        self.transact(
            token,
            operations::READ,
            "template_candidate",
            "template",
            |tx, who| {
                check(tx)?;
                candidate(tx, who, digest)
            },
        )
    }
    pub fn review_template(
        &mut self,
        token: &str,
        digest: &str,
        decision: ReviewDecision,
        reason: &str,
    ) -> Result<Review> {
        self.transact(token,&[Role::Approver],"review_template","template",|tx,who| {
            check(tx)?;
            let (revision,policy)=owners(tx,who)?;
            let c=candidate(tx,who,digest)?;
            if !policy.permits(&c.template.owner_role,&who.actor) {return Err(denied());}
            let review=Review{candidate_digest:digest.into(),actor:who.actor.clone(),owner_role:c.template.owner_role.clone(),decision,reason:reason.into(),reviewed_at_unix_ms:now(tx)? as u64};
            review.validate(&c).map_err(invalid)?;
            let document=serde_json::to_string(&review).map_err(|_|corrupt("review encoding failed"))?;
            // A unique key handles competing reviewers. Inspect the committed
            // winner, rather than overwriting an earlier approval or rejection.
            tx.execute("INSERT INTO workflow_templates.reviews VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",&[&who.tenant,&who.project,&digest,&revision,&document]).map_err(storage)?;
            let row=tx.query_one("SELECT owner_revision,document FROM workflow_templates.reviews WHERE tenant=$1 AND project=$2 AND candidate=$3",&[&who.tenant,&who.project,&digest]).map_err(storage)?;
            let saved:Review=serde_json::from_str(row.get(1)).map_err(|_|corrupt("invalid saved template review"))?;
            saved.validate(&c).map_err(invalid)?;
            if row.get::<_,i64>(0)!=revision || saved.actor!=review.actor || saved.decision!=review.decision || saved.reason!=review.reason {return Err(Error::new(ErrorCode::ReceiptConflict,"template already reviewed; propose a new candidate"));}
            audit(tx,who,"template_reviewed",digest,&workflow_templates::digest(&saved).map_err(invalid)?)?;
            Ok(saved)
        })
    }
    pub fn publish_template(&mut self, token: &str, digest: &str) -> Result<Publication> {
        self.transact(token,&[Role::DefinitionMaintainer],"publish_template","template",|tx,who| {
            check(tx)?;
            let (revision,policy)=owners(tx,who)?;
            let c=candidate(tx,who,digest)?;
            let row=tx.query_opt("SELECT owner_revision,document FROM workflow_templates.reviews WHERE tenant=$1 AND project=$2 AND candidate=$3 FOR SHARE",&[&who.tenant,&who.project,&digest]).map_err(storage)?.ok_or_else(denied)?;
            let review:Review=serde_json::from_str(row.get(1)).map_err(|_|corrupt("invalid template review"))?;
            if revision!=row.get::<_,i64>(0) || !policy.permits(&c.template.owner_role,&review.actor) {return Err(denied());}
            let p=Publication::new(c,review).map_err(invalid)?;
            let id=&p.candidate.template.identity;
            let compiled=p.candidate.template.validate().map_err(invalid)?;
            bind_versions(tx,&who.tenant,&who.project,workflow_runstore_sqlite::bundle_bindings(&compiled)?,true)?;
            let at=now(tx)?;
            let document=serde_json::to_string(&p).map_err(|_|corrupt("publication encoding failed"))?;
            tx.execute("INSERT INTO workflow_templates.publications VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",&[&who.tenant,&who.project,&id.id,&id.version,&p.digest,&document,&who.actor,&at]).map_err(storage)?;
            let saved=publication(tx,who,id)?;
            if saved!=p {return Err(Error::new(ErrorCode::BindingConflict,"template version is immutable"));}
            tx.execute("INSERT INTO workflow_access.published_bundles(tenant,project,digest,published_by) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",&[&who.tenant,&who.project,&compiled.digest(),&who.id]).map_err(storage)?;
            audit(tx,who,"template_published",&p.digest,"accepted")?;
            Ok(p)
        })
    }
    pub fn template_publication(
        &mut self,
        token: &str,
        id: &workflow_ir::VersionRef,
    ) -> Result<Publication> {
        self.transact(
            token,
            operations::READ,
            "template_publication",
            "template",
            |tx, who| {
                check(tx)?;
                publication(tx, who, id)
            },
        )
    }
}
