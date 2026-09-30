CREATE SCHEMA workflow_scheduling;
CREATE TABLE workflow_scheduling.schema_version (
 singleton boolean PRIMARY KEY CHECK(singleton), version integer NOT NULL
);
INSERT INTO workflow_scheduling.schema_version VALUES(true,1);
CREATE TABLE workflow_scheduling.tenants (
 tenant text PRIMARY KEY, revision bigint NOT NULL CHECK(revision>0), policy text NOT NULL
);
CREATE TABLE workflow_scheduling.queue (
 tenant text NOT NULL, project text NOT NULL, run_id text NOT NULL,
 created_at bigint NOT NULL, selected_at bigint NOT NULL,
 priority integer NOT NULL DEFAULT 0 CHECK(priority BETWEEN 0 AND 9),
 revision bigint NOT NULL, active boolean NOT NULL,
 PRIMARY KEY(tenant,project,run_id),
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE TABLE workflow_scheduling.workers (
 tenant text NOT NULL, project text NOT NULL, worker_id text NOT NULL,
 runtime_version text NOT NULL, heartbeat_at bigint NOT NULL,
 draining boolean NOT NULL DEFAULT false,
 PRIMARY KEY(tenant,project,worker_id),
 FOREIGN KEY(tenant,project,worker_id) REFERENCES workflow_access.credentials(tenant,project,id)
);
CREATE TABLE workflow_scheduling.admissions (
 id text PRIMARY KEY, tenant text NOT NULL, project text NOT NULL,
 run_id text NOT NULL, worker_id text NOT NULL, capability text NOT NULL,
 model_pool text, created_at bigint NOT NULL, expires_at bigint NOT NULL,
 finished boolean NOT NULL DEFAULT false,
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE INDEX admission_recent ON workflow_scheduling.admissions(tenant,created_at);
CREATE INDEX admission_live ON workflow_scheduling.admissions(tenant,expires_at) WHERE NOT finished;
CREATE TABLE workflow_scheduling.dead_letters (
 id text PRIMARY KEY, tenant text NOT NULL, project text NOT NULL, run_id text NOT NULL,
 revision bigint NOT NULL, snapshot_digest text NOT NULL,
 reason text NOT NULL, at_unix_ms bigint NOT NULL, resolved boolean NOT NULL DEFAULT false,
 resolution text, resolution_details text,
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE UNIQUE INDEX one_active_dead_letter ON workflow_scheduling.dead_letters(tenant,project,run_id) WHERE NOT resolved;
