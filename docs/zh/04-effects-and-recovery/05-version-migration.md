# 版本锁定与显式迁移（R11）

发布不会改变已有运行。原 StartRun、规范 bundle、流程/子流程版本、能力契约、模型策略与模板、后置
条件和审批策略都保留。新版获得新不可变绑定，同版本换内容被拒绝，包括共享 tenant/project 中其他
运行的冲突。宿主为每个新运行明确选择已发布 bundle。

Worker 分发要求准确能力版本、描述符摘要和模型策略绑定，不兼容 worker 不能消费任务。依赖运行完成前
保留兼容 worker、产物、凭据和环境，或退役前排空运行；旧实现不可用时暂停并明确处理。版本标签是
宿主契约，不是任意程序真实性证明。[工作区执行器](../03-execution-and-evidence/06-workspaces.md)还为既有 attempt 记录和检查实际
可执行文件摘要。

历史回放只化简记录结果，不调用模型/worker/提供方；重新运行工具需新实例/attempt 和对应操作策略。
整个运行生命周期保留原及全部迁移目标 bundle、产物依赖、注册表发布和兼容解释器。当前存储不会自动
删除这些依赖。

## 定义迁移

`MigrationStore` 提供预览与受保护管理提交。唯一实现策略为 `restart_with_fresh_evidence`：用明确目标
输入重启目标根，分配单调递增的新节点实例，重算每个任务、门禁和审批。已完成结果**没有自动复用条件**。
节点映射用于影响评审，不表示从映射节点恢复；未知策略/字段被拒绝。

来源必须 paused 且仍 running。评审计划包含：

- 来源 revision、完整状态摘要与原运行身份。
- 来源/目标 bundle 摘要及完整规范目标 bundle。
- 带明确覆盖的建议映射、增删节点、定义/门禁/审批策略变化。
- 目标输入、输入变化标志、每个旧实例的输入/输出和门禁决策摘要。
- 失效回调 ID、旧 timer deadline 到目标持续时间的映射。
- 明确执行/定时器策略、稳定 migration ID 和有界理由。

映射只能引用已有节点，不能把两个旧节点合并到同一目标。每次提交都根据当前来源重算完整计划，编辑
影响列表或使用过期计划均拒绝。即使输入/定义恰好相同，也使全部旧结果失效；门禁/审批绑定身份变化
保守地报告为变化。

`cancel_and_rearm_on_resume` 取消旧定时器意图。目标保持暂停，明确 resume 后，到达的 wait/loop 才从
该时点开始声明的时长。这是经过评审的 deadline 重置；普通不含迁移的 pause/resume 仍保留原 deadline。

Pending 旧 Inbox 同一事件中变为 `definition_mismatch` 拒绝，既有接受/拒绝记录保留为历史。旧实例不
复用，旧审批/证据不能授权目标。共享回调端点拒绝没有当前认证 wait 策略的目标；本地持久化接入可以
把旧 target 保存为拒绝消息。

迁移事务保留目标定义/绑定，提交内核事件，以迁移专用回执退役全部旧 pending outbox，追加带 actor 与
计划摘要的执行证明，改变 generation 并释放租约。新所有者必须新取得租约。当前活动 attempt 必须排空，
过期只读 attempt 被隔离；即使目标删除旧节点/能力，历史证明仍按原定义和状态验证。`history-at` 重建
锁定的历史状态。

任何业务效果账本条目或未解除恢复屏障都会阻止此重启策略，包括已核对/失败操作；不能清空账本强行迁移。
应完成旧运行，或明确核对独立新运行及外部操作策略。改定义或恢复数据库不逆转发布、支付等外部写。

相同 `(migration_id, complete plan, actor)` 返回原逻辑提交，改内容/actor 冲突。本地或共享最终接纳时
租约过期回滚。回放迁移事件必须恰有一个匹配执行证明，否则损坏。原始 `run event` 不能提交定义迁移。

启动时带有、或迁移进入受保护发布契约或质量门禁人工审批流程的运行，不能再次迁移定义。改变策略或图
需新运行、新证据与审批，额外接纳边界见[受保护交付](../03-execution-and-evidence/09-release-acceptance.md)。

## 本地命令

按当前 revision 暂停，编写符合 `workflow schema run-migration-request` 的请求，使用不可变目标 bundle
和明确输入。CLI 返回值在 result：

```sh
workflow run migration-plan runs.sqlite run-id request.json
# 把 result 保存为 plan.json 并评审，再取得当前管理租约：
workflow run acquire runs.sqlite lease-request.json
# 把 result 保存为 lease.json；文件系统/宿主访问是本地信任边界。
workflow run migration-apply runs.sqlite lease.json plan.json operator
workflow run history-at runs.sqlite run-id 7
workflow run verify runs.sqlite run-id
```

提交后仍 paused。检查目标，用新 revision 明确 resume，并取得新执行租约。不确定回复可重试同一
不可变计划，不能在用过的 migration ID 下重生成另一计划。保留历史有产物时始终配置 `--artifacts`
或相应对象 reader。

## 共享管理操作

HTTPS 经 `workflow remote call` 暴露 `plan_migration`、`migrate_definition`、`historical_snapshot`。
Definition maintainer 先发布目标 bundle；只有同作用域管理员可计划/应用迁移，可取得/释放自己的租约，
但不因此获得 task 分发或 start 权限。服务根据认证凭据检查 lease owner，写入认证 actor，使用 PostgreSQL
时间，不接受自称 actor 或客户端时钟。运行镜像、全局绑定、旧 assignment 退役和有界审计一起提交。
普通读者可请求历史快照，管理员也可用来评审。

## 独立存储升级

SQLite 和共享事务镜像使用 application schema **11**，逻辑 StartRun 仍为 1，worker/service 线路版本
保持原有值。Schema 11 防止旧解释器误读定义迁移事件和存储升级日志。普通 open/create 拒绝旧/未来版本，
不随业务执行自动升级。

本地升级接受 schema 1–11。Schema 1 获得空执行权限头，较晚版本保留原权限。预检在内存构造一致快照，
验证所有运行、事件、检查点、outbox、回执、证明与产物依赖，输出来源及已验证历史摘要，不执行工具/效果。

```sh
workflow run storage-plan old.sqlite
workflow run migrate old.sqlite before-upgrade.sqlite
workflow run storage-history old.sqlite
```

Migrate 排他创建一致 SQLite 备份（Unix 0600），验证转换并 sync，然后在升级事务内比较相同来源摘要。
备份/预检后来源变化则中止，保留备份并换新路径重试。源版本、新表、迁移记录和转换原子提交。
记录含源/目标版本、来源摘要、验证运行数和历史摘要。上限为单快照 256 MiB、单次升级 10000 运行。
依赖失败/损坏保持原 schema。无备份参数的 `run migrate <db>` 只验证已经是当前版本的 store。

把返回升级记录保存为 `upgrade.json`，全部 owner 停止后恢复到新路径，使用保留的兼容二进制：

```sh
workflow run storage-restore before-upgrade.sqlite restored.sqlite upgrade.json
/path/to/retained-v10-workflow run verify restored.sqlite run-id
```

恢复按评审记录验证备份与副本，保留旧 schema，拒绝既有目标。此路径用于停机存储回滚，不创建丢失
外部工作后必需的[恢复屏障](04-backup-recovery.md)。同时保留/复制引用的产物目录与内容。如果备份后发生
执行或外部写，应使用已有隔离恢复/核对流程，不能直接重开旧所有者。

共享镜像从 schema 10–11 到 12 **逐运行**升级。同作用域管理员调用 `plan_storage_upgrade {run_id}`，再调用
`upgrade_storage {run_id, plan}`。聚合行锁下检查来源字节摘要，完整回放、全局绑定和产物验证后替换
镜像；计划不匹配则逐字节不变，相同重试幂等。转换保留逻辑定义、当前租约与记录工作，不重启定义。
新宿主拒绝未升级旧镜像，旧宿主拒绝新镜像，滚动部署需路由至兼容宿主。外层 PostgreSQL 权威/访问
schema 独立且不变。批量前做验证[共享备份](../05-shared-execution/07-shared-recovery.md)，写丢失后走凭据/所有权隔离和核对。

## 可执行验收

| 要求 | 直接证据 |
| --- | --- |
| v1 等待期间发布 v2；旧运行继续 v1，新运行用 v2 | `published_v2_does_not_move_waiting_v1_and_workers_route_by_exact_contract` 与 `examples/migrations/acceptance.py` 真实注册表/CLI |
| 拒绝或路由不兼容能力版本 | 同 PostgreSQL 契约拒绝 v2 worker 执行 v1，运行真实 v1 能力，v2 请求路由准确凭据 |
| 评审删除、输入/门禁和定时器影响 | 内核迁移、本地门禁、共享管理员测试及 CLI 计划断言 |
| 崩溃恢复与幂等 | 定义提交八个终止边界、双进程重复竞争、存储提交四个边界、租约过期回滚、过期计划拒绝 |
| 不复用证据/审批 | 旧通过门禁与任务仅作为历史，旧 WorkResult/生产者证据拒绝新 attempt，新证据通过；内核/本地/共享拒绝旧审批并使暂停 Inbox 失效 |
| 升级失败与版本解释锁定 | 完整日志/依赖验证，本地备份/恢复和损坏回滚，共享失败镜像字节不变，历史快照和保留 v10 程序验证 |

```sh
cargo test -p workflow-kernel migration --locked
cargo test -p workflow-runstore-sqlite migration --locked
WORKFLOW_TEST_POSTGRES='host=127.0.0.1 ...' \
  cargo test -p workflow-runstore-postgres migration --locked -- --ignored --test-threads=1
cargo build -p workflow-cli --locked
python3 examples/migrations/acceptance.py target/debug/workflow
WORKFLOW_TEST_POSTGRES='host=127.0.0.1 ...' \
  python3 examples/migrations/acceptance.py target/debug/workflow --https \
    --legacy-binary /path/to/retained-v10-workflow
```

CI 强制本地 CLI 与 HTTPS/PostgreSQL 演练。可选旧二进制检查也在本地对 R07 CLI 运行；CI 基线用相同旧
表布局构造 schema-10 测试库。进程故障测试在 workspace/Bazel 套件运行。这是有界测试，不是生产升级
可靠性或业务收益估算。迁移事件、证明和共享审计提供持久化结果/actor 计数；测运维迁移延迟时另记宿主耗时。

<!-- book-navigation -->

4.5 版本与存储迁移

[全书目录](../README.md) · [4. 外部操作与恢复](README.md) · [English](../../en/04-effects-and-recovery/05-version-migration.md) · [上一章: 4.4 本地备份与恢复](04-backup-recovery.md) · [下一章: 5.1 PostgreSQL 权威存储](../05-shared-execution/01-postgres-authority.md)

<!-- /book-navigation -->
