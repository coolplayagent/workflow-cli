# The Workflow CLI Book

[中文](../zh/README.md) · [Documentation Bookshelf / 文档书架](../README.md)

[Project overview](../overview.md)

Start with volume 01 to install and run a local approval workflow, then continue through definition, execution and evidence, recovery and shared deployment. Every volume has an index; every chapter links its neighbors and language counterpart. Commands, fields and examples match across editions.

## 01. [Getting started](01-getting-started/README.md)

Install the skill and run a complete local workflow before exploring the runtime contracts.

- [1.1 How to read this book](01-getting-started/01-preface.md)
- [1.2 Install the skill](01-getting-started/02-skill-distribution.md)
- [1.3 Your first durable workflow](01-getting-started/03-getting-started.md)

## 02. [Define the process](02-process-definition/README.md)

Define typed processes, publish immutable versions and connect capabilities to workers.

- [2.1 Definitions and types](02-process-definition/01-definition-semantics.md)
- [2.2 Author and publish](02-process-definition/02-definition-registry.md)
- [2.3 Capabilities and workers](02-process-definition/03-worker-protocol.md)
- [2.4 Control flow and replay](02-process-definition/04-kernel-semantics.md)
- [2.5 Reviewed SOP templates](02-process-definition/05-reviewed-templates.md)

## 03. [Execute and verify](03-execution-and-evidence/README.md)

Execute local work durably and verify artifacts, workspaces and delivery evidence.

- [3.1 Durable state](03-execution-and-evidence/01-run-store.md)
- [3.2 Local execution](03-execution-and-evidence/02-local-execution.md)
- [3.3 Local daemon](03-execution-and-evidence/03-local-daemon.md)
- [3.4 Bounded model execution](03-execution-and-evidence/04-model-execution.md)
- [3.5 Artifacts and provenance](03-execution-and-evidence/05-artifacts.md)
- [3.6 Attempt workspaces](03-execution-and-evidence/06-workspaces.md)
- [3.7 Evidence gates](03-execution-and-evidence/07-evidence-gates.md)
- [3.8 Runtime postconditions](03-execution-and-evidence/08-runtime-postconditions.md)
- [3.9 Protected delivery](03-execution-and-evidence/09-release-acceptance.md)

## 04. [Effects and recovery](04-effects-and-recovery/README.md)

Handle external events and writes, compensate in order and recover safely from backups.

- [4.1 Events and human decisions](04-effects-and-recovery/01-event-inbox.md)
- [4.2 Durable external effects](04-effects-and-recovery/02-durable-effects.md)
- [4.3 Ordered compensation](04-effects-and-recovery/03-ordered-compensation.md)
- [4.4 Local backup and recovery](04-effects-and-recovery/04-backup-recovery.md)
- [4.5 Version and storage migration](04-effects-and-recovery/05-version-migration.md)

## 05. [Shared execution](05-shared-execution/README.md)

Build on local execution with authenticated services, shared storage and cluster scheduling.

- [5.1 PostgreSQL authority](05-shared-execution/01-postgres-authority.md)
- [5.2 Authentication and roles](05-shared-execution/02-authenticated-authority.md)
- [5.3 HTTPS service and workers](05-shared-execution/03-remote-service.md)
- [5.4 Shared artifacts](05-shared-execution/04-shared-artifacts.md)
- [5.5 Remote external effects](05-shared-execution/05-remote-effects.md)
- [5.6 Cluster scheduling](05-shared-execution/06-cluster-scheduling.md)
- [5.7 Shared backup and recovery](05-shared-execution/07-shared-recovery.md)

## 06. [Acceptance and maintenance](06-acceptance-and-maintenance/README.md)

Check acceptance evidence, diagnose failures and follow the architecture and delivery history.

- [6.1 Definition acceptance](06-acceptance-and-maintenance/01-definition-acceptance.md)
- [6.2 Model boundary acceptance](06-acceptance-and-maintenance/02-model-boundaries-acceptance.md)
- [6.3 Approval acceptance](06-acceptance-and-maintenance/03-approval-acceptance.md)
- [6.4 Artifact and workspace acceptance](06-acceptance-and-maintenance/04-artifact-acceptance.md)
- [6.5 Local platform acceptance](06-acceptance-and-maintenance/05-local-acceptance.md)
- [6.6 Security acceptance](06-acceptance-and-maintenance/06-security-acceptance.md)
- [6.7 Troubleshooting and contributing](06-acceptance-and-maintenance/07-troubleshooting.md)
- [6.8 Architecture and delivery map](06-acceptance-and-maintenance/08-roadmap.md)
