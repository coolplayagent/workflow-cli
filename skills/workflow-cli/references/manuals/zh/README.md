# Workflow CLI 实践指南

[English](../en/README.md) · [Documentation Bookshelf / 文档书架](../README.md)

[项目介绍](../overview.zh-CN.md)

从第 01 卷开始安装并运行本地审批流程，再依次学习定义、执行与证据、恢复和共享部署。每卷有独立目录；每章都提供前后章导航和同章语言切换。命令、字段和示例在两个版本中一致。

## 01. [起步](01-getting-started/README.md)

安装 skill 并运行完整的本地工作流，再逐步了解运行时契约。

- [1.1 如何阅读本书](01-getting-started/01-preface.md)
- [1.2 安装与发布 skill](01-getting-started/02-skill-distribution.md)
- [1.3 第一个持久化工作流](01-getting-started/03-getting-started.md)

## 02. [定义流程](02-process-definition/README.md)

定义有类型的流程，发布不可变版本，并将能力绑定到 worker。

- [2.1 流程定义与类型](02-process-definition/01-definition-semantics.md)
- [2.2 编写与发布流程](02-process-definition/02-definition-registry.md)
- [2.3 能力与 worker 协议](02-process-definition/03-worker-protocol.md)
- [2.4 控制流与回放](02-process-definition/04-kernel-semantics.md)
- [2.5 可评审的 SOP 模板](02-process-definition/05-reviewed-templates.md)

## 03. [执行与验证](03-execution-and-evidence/README.md)

持久化执行本地任务，核验产物、工作区和交付证据。

- [3.1 持久化运行状态](03-execution-and-evidence/01-run-store.md)
- [3.2 本地执行](03-execution-and-evidence/02-local-execution.md)
- [3.3 本地守护进程](03-execution-and-evidence/03-local-daemon.md)
- [3.4 有界模型执行](03-execution-and-evidence/04-model-execution.md)
- [3.5 产物与来源](03-execution-and-evidence/05-artifacts.md)
- [3.6 尝试级工作区](03-execution-and-evidence/06-workspaces.md)
- [3.7 证据门禁](03-execution-and-evidence/07-evidence-gates.md)
- [3.8 运行时后置条件](03-execution-and-evidence/08-runtime-postconditions.md)
- [3.9 受保护的交付](03-execution-and-evidence/09-release-acceptance.md)

## 04. [外部操作与恢复](04-effects-and-recovery/README.md)

处理外部事件与写入，执行有序补偿，并从备份恢复。

- [4.1 事件与人工决策](04-effects-and-recovery/01-event-inbox.md)
- [4.2 持久化外部操作](04-effects-and-recovery/02-durable-effects.md)
- [4.3 有序补偿](04-effects-and-recovery/03-ordered-compensation.md)
- [4.4 本地备份与恢复](04-effects-and-recovery/04-backup-recovery.md)
- [4.5 版本与存储迁移](04-effects-and-recovery/05-version-migration.md)

## 05. [共享执行](05-shared-execution/README.md)

在本地执行基础上引入认证服务、共享存储和集群调度。

- [5.1 PostgreSQL 权威存储](05-shared-execution/01-postgres-authority.md)
- [5.2 认证与角色](05-shared-execution/02-authenticated-authority.md)
- [5.3 HTTPS 服务与 worker](05-shared-execution/03-remote-service.md)
- [5.4 共享产物](05-shared-execution/04-shared-artifacts.md)
- [5.5 远程外部操作](05-shared-execution/05-remote-effects.md)
- [5.6 集群调度](05-shared-execution/06-cluster-scheduling.md)
- [5.7 共享备份与恢复](05-shared-execution/07-shared-recovery.md)

## 06. [验收与维护](06-acceptance-and-maintenance/README.md)

检查验收证据、排查故障，并了解架构与交付历史。

- [6.1 流程定义验收](06-acceptance-and-maintenance/01-definition-acceptance.md)
- [6.2 模型边界验收](06-acceptance-and-maintenance/02-model-boundaries-acceptance.md)
- [6.3 审批验收](06-acceptance-and-maintenance/03-approval-acceptance.md)
- [6.4 产物与工作区验收](06-acceptance-and-maintenance/04-artifact-acceptance.md)
- [6.5 本地平台验收](06-acceptance-and-maintenance/05-local-acceptance.md)
- [6.6 安全验收](06-acceptance-and-maintenance/06-security-acceptance.md)
- [6.7 故障排查与贡献](06-acceptance-and-maintenance/07-troubleshooting.md)
- [6.8 架构与交付路线图](06-acceptance-and-maintenance/08-roadmap.md)
