# The Workflow CLI Book

[中文](zh/README.md) · [Project overview](../README.md)

This book follows a runnable local approval workflow from its definition through durable execution, evidence gates, effect recovery and shared deployment. Read the first three chapters in order on your first visit. Before operating a shared deployment, work through local execution and recovery. Every chapter provides previous/next navigation and a link to the same chapter in Chinese.

JSON fields, identifiers and command arguments stay identical in both editions. Examples state their runtime prerequisites; acceptance chapters distinguish tested behavior from unfinished objectives.

## 1. Getting started

1. [How to read this book](preface.md)
2. [Install the skill](skill-distribution.md)
3. [Your first durable workflow](getting-started.md)

## 2. Define the process

4. [Definitions and types](definition-semantics.md)
5. [Author and publish](definition-registry.md)
6. [Capabilities and workers](worker-protocol.md)
7. [Control flow and replay](kernel-semantics.md)
8. [Reviewed SOP templates](reviewed-templates.md)

## 3. Execute and verify

9. [Durable state](run-store.md)
10. [Local execution](local-execution.md)
11. [Local daemon](local-daemon.md)
12. [Bounded model execution](model-execution.md)
13. [Artifacts and provenance](artifacts.md)
14. [Attempt workspaces](workspaces.md)
15. [Evidence gates](evidence-gates.md)
16. [Runtime postconditions](runtime-postconditions.md)
17. [Protected delivery](release-acceptance.md)

## 4. Effects and recovery

18. [Events and human decisions](event-inbox.md)
19. [Durable external effects](durable-effects.md)
20. [Ordered compensation](ordered-compensation.md)
21. [Local backup and recovery](backup-recovery.md)
22. [Version and storage migration](version-migration.md)

## 5. Shared execution

23. [PostgreSQL authority](postgres-authority.md)
24. [Authentication and roles](authenticated-authority.md)
25. [HTTPS service and workers](remote-service.md)
26. [Shared artifacts](shared-artifacts.md)
27. [Remote external effects](remote-effects.md)
28. [Cluster scheduling](cluster-scheduling.md)
29. [Shared backup and recovery](shared-recovery.md)

## 6. Acceptance and maintenance

30. [Definition acceptance](definition-acceptance.md)
31. [Model boundary acceptance](model-boundaries-acceptance.md)
32. [Approval acceptance](approval-acceptance.md)
33. [Artifact and workspace acceptance](artifact-acceptance.md)
34. [Local platform acceptance](local-acceptance.md)
35. [Security acceptance](security-acceptance.md)
36. [Troubleshooting and contributing](troubleshooting.md)
37. [Architecture and delivery map](roadmap.md)
