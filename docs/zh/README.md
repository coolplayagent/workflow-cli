# Workflow CLI 实践指南

[English](../README.md) · [项目介绍](../../README.zh-CN.md)

本书从一个可运行的本地审批流程开始，逐步讲解定义、持久化执行、证据门禁、外部操作恢复与共享部署。首次使用请顺序阅读前三章；需要维护生产运行时，先完成本地执行和恢复，再进入共享执行篇。每章都有上一章、下一章和同章语言切换。

命令中的 JSON 字段、标识符和参数在两个版本中保持一致。文档示例会说明所需的运行环境；验收章节区分已有测试证据与尚未完成的目标。

## 1. 起步

1. [如何阅读本书](preface.md)
2. [安装与发布 skill](skill-distribution.md)
3. [第一个持久化工作流](getting-started.md)

## 2. 定义流程

4. [流程定义与类型](definition-semantics.md)
5. [编写与发布流程](definition-registry.md)
6. [能力与 worker 协议](worker-protocol.md)
7. [控制流与回放](kernel-semantics.md)
8. [可评审的 SOP 模板](reviewed-templates.md)

## 3. 执行与验证

9. [持久化运行状态](run-store.md)
10. [本地执行](local-execution.md)
11. [本地守护进程](local-daemon.md)
12. [有界模型执行](model-execution.md)
13. [产物与来源](artifacts.md)
14. [尝试级工作区](workspaces.md)
15. [证据门禁](evidence-gates.md)
16. [运行时后置条件](runtime-postconditions.md)
17. [受保护的交付](release-acceptance.md)

## 4. 外部操作与恢复

18. [事件与人工决策](event-inbox.md)
19. [持久化外部操作](durable-effects.md)
20. [有序补偿](ordered-compensation.md)
21. [本地备份与恢复](backup-recovery.md)
22. [版本与存储迁移](version-migration.md)

## 5. 共享执行

23. [PostgreSQL 权威存储](postgres-authority.md)
24. [认证与角色](authenticated-authority.md)
25. [HTTPS 服务与 worker](remote-service.md)
26. [共享产物](shared-artifacts.md)
27. [远程外部操作](remote-effects.md)
28. [集群调度](cluster-scheduling.md)
29. [共享备份与恢复](shared-recovery.md)

## 6. 验收与维护

30. [流程定义验收](definition-acceptance.md)
31. [模型边界验收](model-boundaries-acceptance.md)
32. [审批验收](approval-acceptance.md)
33. [产物与工作区验收](artifact-acceptance.md)
34. [本地平台验收](local-acceptance.md)
35. [安全验收](security-acceptance.md)
36. [故障排查与贡献](troubleshooting.md)
37. [架构与交付路线图](roadmap.md)
