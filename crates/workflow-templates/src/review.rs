use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerPolicy {
    pub owners: BTreeMap<String, BTreeSet<String>>,
}
impl OwnerPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.owners.is_empty() || self.owners.len() > 32 {
            return Err(invalid("owners", "declare 1..32 process owner roles"));
        }
        for (role, actors) in &self.owners {
            name(role, "owners.role")?;
            if actors.is_empty() || actors.len() > 128 {
                return Err(invalid(
                    "owners.actors",
                    "declare 1..128 actors per owner role",
                ));
            }
            for actor in actors {
                name(actor, "owners.actor")?;
            }
        }
        Ok(())
    }
    pub fn permits(&self, role: &str, actor: &str) -> bool {
        self.owners.get(role).is_some_and(|a| a.contains(actor))
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    Success,
    Rework,
    Rejected,
    TimedOut,
    Failed,
}
impl Scenario {
    pub const ALL: [Self; 5] = [
        Self::Success,
        Self::Rework,
        Self::Rejected,
        Self::TimedOut,
        Self::Failed,
    ];
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegressionCase {
    pub scenario: Scenario,
    pub mode: ExecutionMode,
    pub run_digest: String,
    pub bundle_digest: String,
    pub report_digest: String,
    pub artifacts: Vec<String>,
    pub terminal_status: String,
    pub accepted: bool,
    pub repair_rounds: u32,
}
/// Evidence references are assertions supplied for review. Digests provide
/// integrity, not proof of test execution. The authenticated owner reviews the
/// actual immutable reports before approving this exact candidate digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Regressions {
    pub template_digest: String,
    pub cases: Vec<RegressionCase>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub template: Template,
    pub proposed_by: String,
    pub reason: String,
    pub compatibility: String,
    pub regressions: Regressions,
}
impl Candidate {
    pub fn validate(&self) -> Result<()> {
        let compiled = self.template.validate()?;
        name(&self.proposed_by, "proposed_by")?;
        text(&self.reason, "reason")?;
        text(&self.compatibility, "compatibility")?;
        if self.regressions.template_digest != self.template.content_digest()?
            || self.regressions.cases.len() != 10
        {
            return Err(invalid(
                "regressions",
                "exact template digest and five cases in each execution mode required",
            ));
        }
        let mut seen = BTreeSet::new();
        for c in &self.regressions.cases {
            if !seen.insert((c.mode, c.scenario))
                || c.bundle_digest != compiled.digest()
                || !hash(&c.run_digest)
                || !hash(&c.report_digest)
                || c.artifacts.is_empty()
                || c.artifacts.len() > 128
                || c.artifacts.iter().any(|a| !hash(a))
            {
                return Err(invalid(
                    "regressions.cases",
                    "duplicate, missing or inconsistent regression evidence",
                ));
            }
            let success = matches!(c.scenario, Scenario::Success | Scenario::Rework);
            if c.accepted != success
                || (success && c.terminal_status != "succeeded")
                || (!success && !["failed", "cancelled"].contains(&c.terminal_status.as_str()))
                || (c.scenario == Scenario::Rework && c.repair_rounds < 2)
            {
                return Err(invalid(
                    "regressions.cases",
                    "regression outcome does not cover its scenario",
                ));
            }
        }
        Ok(())
    }
    pub fn content_digest(&self) -> Result<String> {
        self.validate()?;
        digest(self)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub candidate_digest: String,
    pub actor: String,
    pub owner_role: String,
    pub decision: ReviewDecision,
    pub reason: String,
    pub reviewed_at_unix_ms: u64,
}
impl Review {
    pub fn validate(&self, candidate: &Candidate) -> Result<()> {
        name(&self.actor, "review.actor")?;
        text(&self.reason, "review.reason")?;
        if self.actor == candidate.proposed_by
            || self.owner_role != candidate.template.owner_role
            || self.candidate_digest != candidate.content_digest()?
            || self.reviewed_at_unix_ms == 0
        {
            return Err(invalid(
                "review",
                "independent owner review must bind this exact candidate",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub candidate: Candidate,
    pub review: Review,
    pub digest: String,
}
impl Publication {
    pub fn new(candidate: Candidate, review: Review) -> Result<Self> {
        review.validate(&candidate)?;
        if review.decision != ReviewDecision::Approve {
            return Err(invalid("review.decision", "owner rejected this candidate"));
        }
        let digest = digest(&(&candidate, &review))?;
        Ok(Self {
            candidate,
            review,
            digest,
        })
    }
    pub fn verify(&self) -> Result<()> {
        let checked = Self::new(self.candidate.clone(), self.review.clone())?;
        if self.digest != checked.digest {
            return Err(invalid("digest", "publication integrity mismatch"));
        }
        Ok(())
    }
}
