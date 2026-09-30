CREATE SCHEMA workflow_templates;
CREATE TABLE workflow_templates.schema_version(singleton boolean PRIMARY KEY CHECK(singleton),version integer NOT NULL);
INSERT INTO workflow_templates.schema_version VALUES(true,1);
CREATE TABLE workflow_templates.owners(
 tenant text NOT NULL,project text NOT NULL,revision bigint NOT NULL CHECK(revision>0),document text NOT NULL,
 PRIMARY KEY(tenant,project)
);
CREATE TABLE workflow_templates.owner_history(
 tenant text NOT NULL,project text NOT NULL,revision bigint NOT NULL,document text NOT NULL,at_unix_ms bigint NOT NULL,
 PRIMARY KEY(tenant,project,revision)
);
CREATE TABLE workflow_templates.candidates(
 tenant text NOT NULL,project text NOT NULL,digest text NOT NULL,document text NOT NULL CHECK(octet_length(document)<=2097152),
 PRIMARY KEY(tenant,project,digest)
);
CREATE TABLE workflow_templates.reviews(
 tenant text NOT NULL,project text NOT NULL,candidate text NOT NULL,owner_revision bigint NOT NULL,document text NOT NULL,
 PRIMARY KEY(tenant,project,candidate),
 FOREIGN KEY(tenant,project,candidate) REFERENCES workflow_templates.candidates(tenant,project,digest)
);
CREATE TABLE workflow_templates.publications(
 tenant text NOT NULL,project text NOT NULL,id text NOT NULL,version text NOT NULL,digest text NOT NULL,document text NOT NULL,
 published_by text NOT NULL,published_at bigint NOT NULL,
 PRIMARY KEY(tenant,project,id,version),UNIQUE(tenant,project,digest)
);
