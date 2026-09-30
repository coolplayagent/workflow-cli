CREATE SCHEMA workflow_objects;
CREATE TABLE workflow_objects.schema_version(singleton boolean PRIMARY KEY CHECK(singleton), version integer NOT NULL);
INSERT INTO workflow_objects.schema_version VALUES(true,1);
CREATE TABLE workflow_objects.catalogs(
  namespace text PRIMARY KEY, binding text NOT NULL, sequence bigint NOT NULL CHECK(sequence>=0), chain text NOT NULL
);
CREATE TABLE workflow_objects.artifacts(
  namespace text NOT NULL REFERENCES workflow_objects.catalogs(namespace),
  sequence bigint NOT NULL CHECK(sequence>0), id text NOT NULL, reference text NOT NULL,
  object_key text NOT NULL UNIQUE, digest text NOT NULL,
  PRIMARY KEY(namespace,id), UNIQUE(namespace,sequence)
);
CREATE TABLE workflow_objects.uploads(
  namespace text NOT NULL REFERENCES workflow_objects.catalogs(namespace), object_key text PRIMARY KEY,
  expires_at bigint NOT NULL, abandoned boolean NOT NULL DEFAULT false
);
CREATE FUNCTION workflow_objects.retained() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'retained immutable artifact'; END;
$$;
CREATE TRIGGER artifact_immutable BEFORE UPDATE OR DELETE ON workflow_objects.artifacts FOR EACH ROW EXECUTE FUNCTION workflow_objects.retained();
CREATE TRIGGER catalog_retained BEFORE DELETE ON workflow_objects.catalogs FOR EACH ROW EXECUTE FUNCTION workflow_objects.retained();
