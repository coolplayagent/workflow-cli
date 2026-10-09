# Authenticated shared execution

Read `workflow help` and the packaged [remote manual](manuals/remote-service.md)
before configuring a shared service. The same executable provides `service serve`,
`remote schedule`, `remote managed-work` and client commands. These are separate
process roles; a skill installation does not provision PostgreSQL or start them.

Use the task's HTTPS endpoint, trusted CA and scoped bearer secret reference in a
client binding. Keep credentials out of workflow definitions and command arguments.
`remote validate <client-binding> <definition>` uses server-side validation.
`remote call <client-binding> <request>` exposes the typed application operations.
The request ID correlates a call; each mutation has its own documented retry identity.

Use `remote artifact-upload` / `artifact-download` with actual assignment authority.
For workers, match granted capability versions/digests and frozen model/effect
policies. Configure model or effect adapters only within the task's intended scope.
Keep uncertain effect outcomes in the ledger and reconcile provider observations.

Bootstrap, credential issuance, owner-policy configuration, storage restoration and
recovery fencing are trusted administration. Use the authority already established
by the user; possession of an input document does not grant a remote role.
Successful HTTP transport alone does not prove a business run succeeded. Inspect
the typed response and current run status before reporting progress or retrying.

For deployment details read [authentication](manuals/authenticated-authority.md),
[cluster scheduling](manuals/cluster-scheduling.md),
[remote effects](manuals/remote-effects.md) or
[shared recovery](manuals/shared-recovery.md) as needed.
