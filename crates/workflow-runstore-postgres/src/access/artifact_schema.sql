CREATE SCHEMA workflow_artifacts;
CREATE TABLE workflow_artifacts.schema_version (
 singleton boolean PRIMARY KEY CHECK(singleton), version integer NOT NULL
);
INSERT INTO workflow_artifacts.schema_version VALUES (true,1);
CREATE TABLE workflow_artifacts.artifacts (
 tenant text NOT NULL, project text NOT NULL, run_id text NOT NULL,
 id text NOT NULL, reference text NOT NULL CHECK(octet_length(reference)<=65536),
 content bytea NOT NULL CHECK(octet_length(content)<=67108864),
 created_at bigint NOT NULL,
 PRIMARY KEY(tenant,project,id),
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE INDEX artifact_run ON workflow_artifacts.artifacts(tenant,project,run_id,id);
CREATE TABLE workflow_artifacts.uploads (
 id text PRIMARY KEY, tenant text NOT NULL, project text NOT NULL, run_id text NOT NULL,
 worker_id text NOT NULL, assignment_id text NOT NULL, request_id text NOT NULL,
 reference text NOT NULL CHECK(octet_length(reference)<=65536),
 expected_bytes bigint NOT NULL CHECK(expected_bytes>=0 AND expected_bytes<=67108864),
 received_bytes bigint NOT NULL DEFAULT 0 CHECK(received_bytes>=0 AND received_bytes<=expected_bytes),
 expires_at bigint NOT NULL, completed_id text,
 UNIQUE(worker_id,assignment_id,request_id),
 FOREIGN KEY(assignment_id) REFERENCES workflow_access.assignments(id),
 FOREIGN KEY(tenant,project,completed_id) REFERENCES workflow_artifacts.artifacts(tenant,project,id),
 FOREIGN KEY(tenant,project,worker_id) REFERENCES workflow_access.credentials(tenant,project,id),
 FOREIGN KEY(tenant,project,run_id) REFERENCES workflow_authority.runs(tenant,project,run_id)
);
CREATE INDEX upload_scope ON workflow_artifacts.uploads(tenant,project,run_id,expires_at);
CREATE TABLE workflow_artifacts.chunks (
 upload_id text NOT NULL REFERENCES workflow_artifacts.uploads(id) ON DELETE CASCADE,
 byte_offset bigint NOT NULL CHECK(byte_offset>=0),
 content bytea NOT NULL CHECK(octet_length(content)>0 AND octet_length(content)<=65536),
 PRIMARY KEY(upload_id,byte_offset)
);
CREATE TABLE workflow_artifacts.downloads (
 id text PRIMARY KEY, tenant text NOT NULL, project text NOT NULL,
 credential_id text NOT NULL, artifact_id text NOT NULL, assignment_id text,
 expires_at bigint NOT NULL,
 FOREIGN KEY(tenant,project,credential_id) REFERENCES workflow_access.credentials(tenant,project,id),
 FOREIGN KEY(tenant,project,artifact_id) REFERENCES workflow_artifacts.artifacts(tenant,project,id),
 FOREIGN KEY(assignment_id) REFERENCES workflow_access.assignments(id)
);
CREATE INDEX download_scope ON workflow_artifacts.downloads(tenant,project,expires_at);
