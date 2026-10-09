# Reviewed reusable SOPs

Read `workflow schema template` and `schema template-instance`. Choose a template
only when its prerequisites and exclusions match the requested work. The bundled
defect, feature and release templates under `assets/examples/templates/` require
project-specific `sdlc.*` and `delivery.*` adapters; they are not builtin integrations.

Use `template validate <template>` and `template plan <template> <instance>` to
check the exact bundle, parameters and declared bindings. Plans are pure: they
list possible actions, gates, waits and budgets and emit `plan.request`, without
creating a run or invoking an adapter. Missing bindings must be resolved from the
actual host inventory. Instance parameters cannot override frozen mandatory gates.

For catalog publication, use `template init` with the intended owner policy, then
`propose`, independent `review` and `publish` for the exact candidate digest.
Inspect the actual regression evidence required by the candidate; neither an LLM
assertion nor a synthetic digest proves those executions occurred. A changed
candidate requires its own review and new immutable identities as applicable.

`template instantiate <catalog> <id> <version> <instance>` returns publication
provenance and `plan.request`. Save that request and pass it to `run start`, then
use the appropriate driver described in the [run guide](run.md). An instantiated
plan does not grant execution credentials. Existing runs retain their old versions.

Shared owner configuration uses `service configure-template-owners`; authenticated
publication and review use scoped remote operations. See the complete
[template manual](../../../docs/reviewed-templates.md) for candidate evidence, owner roles,
local/remote publication and fixture limitations.
