CREATE SCHEMA workflow_access;
CREATE TABLE workflow_access.schema_version (
 singleton boolean PRIMARY KEY CHECK(singleton), version integer NOT NULL
);
INSERT INTO workflow_access.schema_version VALUES (true,1);
CREATE TABLE workflow_access.credentials (
 id text PRIMARY KEY, token_digest text UNIQUE NOT NULL,
 tenant text NOT NULL, project text NOT NULL, actor text NOT NULL,
 role text NOT NULL CHECK(role IN ('administrator','definition_maintainer','viewer','runner','approver','scheduler','worker','recovery')),
 capabilities text NOT NULL,
 issued_at bigint NOT NULL, expires_at bigint NOT NULL CHECK(expires_at>issued_at),
 revoked boolean NOT NULL DEFAULT false,
 UNIQUE(tenant,project,id)
);
CREATE TABLE workflow_access.assignments (
 id text PRIMARY KEY, tenant text NOT NULL, project text NOT NULL,
 worker_id text NOT NULL, run_id text NOT NULL,
 lease text NOT NULL, task text NOT NULL, expires_at bigint NOT NULL,
 settled boolean NOT NULL DEFAULT false,
 FOREIGN KEY(tenant,project,worker_id) REFERENCES workflow_access.credentials(tenant,project,id),
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE INDEX assignment_worker ON workflow_access.assignments(tenant,project,worker_id,id);
CREATE TABLE workflow_access.audit (
 sequence bigserial PRIMARY KEY, tenant text NOT NULL, project text NOT NULL,
 actor text NOT NULL, credential_id text NOT NULL, operation text NOT NULL,
 resource text NOT NULL, outcome text NOT NULL,
 at_unix_ms bigint NOT NULL DEFAULT floor(extract(epoch FROM clock_timestamp())*1000)::bigint
);
CREATE INDEX audit_scope ON workflow_access.audit(tenant,project,sequence);
CREATE TABLE workflow_access.published_bundles (
 tenant text NOT NULL, project text NOT NULL, digest text NOT NULL,
 published_by text NOT NULL,
 PRIMARY KEY(tenant,project,digest)
);
