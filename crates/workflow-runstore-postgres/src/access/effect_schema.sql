CREATE SCHEMA workflow_effect_dispatch;
CREATE TABLE workflow_effect_dispatch.schema_version (
 singleton boolean PRIMARY KEY CHECK(singleton), version integer NOT NULL
);
INSERT INTO workflow_effect_dispatch.schema_version VALUES (true,1);
CREATE TABLE workflow_effect_dispatch.assignments (
 id text PRIMARY KEY, tenant text NOT NULL, project text NOT NULL,
 worker_id text NOT NULL, run_id text NOT NULL,
 lease text NOT NULL, attempt text NOT NULL, expires_at bigint NOT NULL, deliver_before bigint NOT NULL,
 delivered boolean NOT NULL DEFAULT false, settled boolean NOT NULL DEFAULT false,
 FOREIGN KEY(tenant,project,worker_id) REFERENCES workflow_access.credentials(tenant,project,id),
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE INDEX effect_assignment_worker ON workflow_effect_dispatch.assignments(tenant,project,worker_id,id);
