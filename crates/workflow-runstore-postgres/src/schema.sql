CREATE SCHEMA workflow_authority;
CREATE TABLE workflow_authority.schema_version (
 singleton boolean PRIMARY KEY CHECK(singleton), version integer NOT NULL
);
INSERT INTO workflow_authority.schema_version VALUES (true,1);
CREATE TABLE workflow_authority.runs (
 tenant text NOT NULL, project text NOT NULL, run_id text NOT NULL,
 generation bigint NOT NULL DEFAULT 0 CHECK(generation >= 0),
 image bytea CHECK(octet_length(image) <= 67108864), image_digest text,
 PRIMARY KEY(tenant,project,run_id)
);
CREATE TABLE workflow_authority.bindings (
 tenant text NOT NULL, project text NOT NULL, kind text NOT NULL,
 id text NOT NULL, version text NOT NULL, digest text NOT NULL,
 PRIMARY KEY(tenant,project,kind,id,version)
);
