# 共享数据库恢复与 R04 验收

PostgreSQL 始终是共享 authority。run image 包含冻结定义、状态/事件/checkpoint、Outbox/回执、Inbox/wait、执行 attempt、模型记录和 effect 历史。同一数据库还保存共享产物及字节、不可变版本锁、发布记录、有作用域的凭证、assignment 和审计。数据库恢复必须一起保留这些内容。

只恢复数据库还不够：它可能复活旧凭证、租约和已派发工作，同时丢失更晚的外部回执。在目标开放给客户端前，可信离线操作 `service fence-restored` 会验证每个 run 和产物依赖，并创建处于 hold 状态的恢复 generation。

## 一致归档与隔离恢复

验收示例使用完整的 PostgreSQL custom-format 归档，恢复到新的隔离数据库。`pg_dump` 为单个数据库生成一致快照，不包含集群级角色或 tablespace。custom 归档由 `pg_restore` 读取。参见官方 [pg_dump 文档](https://www.postgresql.org/docs/current/app-pgdump.html)。

应使用可信源、兼容的 PostgreSQL 工具、主机管理的连接密钥和新的目标。以下示例假设 libpq service 已配置：

```sh
umask 077
pg_dump --format=custom --file=workflow.dump --dbname='service=workflow-source'
sha256sum workflow.dump
pg_restore --single-transaction --exit-on-error --no-owner --no-privileges \
  --dbname='service=workflow-isolated-restore' workflow.dump
```

目标数据库及 owner 必须事先存在。这些恢复选项将应用归档放在一个事务内加载，并遇错停止；它们不会重建部署用户/权限。参见 [pg_restore](https://www.postgresql.org/docs/current/app-pgrestore.html)。主机持久备份存储应保留归档、验证后的校验和、快照/捕获时间及兼容应用版本。此示例是有界逻辑恢复演练，不能替代部署的物理备份、WAL/PITR、复制或归档保留配置。

此时不要启动目标服务。主机持有的 server binding 必须指向隔离数据库。准备包含精确目标数据库名、归档实际 SHA-256 摘要、操作者和原因的请求：

```json
{
  "database": "workflow_isolated_restore",
  "backup_digest": "sha256:ACTUAL_ARCHIVE_DIGEST",
  "actor": "recovery-operator",
  "reason": "Restore isolated destination after source storage loss"
}
```

```sh
workflow service fence-restored restored-server.json restore-request.json new-administrators.json
```

这是可信数据库主机操作，不在公共 RPC 协议中。提供的名称必须等于 `current_database()`。归档摘要是主机来源注释；完整回放验证的是已恢复的应用状态，而不是任意 SQL 归档的身份或真实性。调用者必须自行选对可信归档与隔离目标。

## 原子恢复事务

此操作锁定 authority、credential、assignment 和 artifact 表，使用同步提交，并在单个数据库事务中执行：

1. 验证支持的 schema 版本及有界清单（最多 1000 个 run、1000 个 tenant/project scope）。验证每份 image digest、完整回放、checkpoint/历史一致性、不可变绑定、产物字节与血缘。
2. 撤销全部保留凭证，包括管理员。对保留的未完成 task/effect assignment 执行 fencing，但不删除历史。
3. 为每个 run 追加新的随机 ownership generation 和绑定备份的 recovery barrier，释放旧 ownership。运行中的 run 变为暂停；已有暂停、wait 截止时间、循环轮次、重试预算和终态保持不变。
4. 为每个保留 scope 签发新管理员并追加恢复审计。确认事务后才返回报告。

依赖损坏或写入失败会回滚整个操作，包括较早 run 的变更和凭证替换。成功完成的 run 保留业务状态与证据，不重新执行工具。恢复 image 保留原历史，并追加显式恢复 journal 条目。

CLI 将新管理员凭证写入一个独占创建的 0600 文件；stdout 仅输出计数、目标、备份和 generation 元数据。每个管理员保留现有的一小时 bootstrap 有效期。通过已有作用域配置机制签发替代的 runner、viewer、recovery、scheduler 和 worker 凭证，再启动目标服务。绝不复用源 worker 凭证。普通报告及 Debug 输出均不含 token。

如果事务提交后私有文件交付失败，命令返回错误，数据库可能已经处于 hold。应继续隔离。使用新输出路径重复操作会创建另一个 hold generation 和新管理员，并撤销前次恢复凭证；不会重置历史或执行预算。这是有意执行的新恢复操作，不是结果不明时的自动重放。

## 外部 effect 与继续运行

使用 `remote call` 和 `{"type":"recovery_barrier","run_id":"..."}` 查看精确 barrier。恢复 run 只允许对账和只读推进，不授权新的外部写入。旧凭证和旧租约不能在恢复目标中完成工作。

| 保留证据 | 恢复动作 |
| --- | --- |
| 保留原 intent，但结果不确定 | 在后继租约下使用现有 effect 协议查询提供方。缺少观察不能代表成功，也不能允许盲目重复。 |
| 源在备份之后准入了一次写入 | 获取真实源 intent 和提供方回执。recovery 凭证取得自己的有效租约，调用 `import_restored_effect`，提交 `lease` 与 `request: {intent, resolution}`。 |
| 原始源/提供方历史不可用 | 保留 barrier。数据库没有回执不等于提供方没有执行。 |

导入使用已有 `run-restored-effect` schema。服务用认证 recovery actor 替换请求中的 resolution actor。reducer 检查冻结 task/input/policy/key 和精确提供方回执；改变后的 intent 被拒绝且不改变状态。重复导入在重新检查当前凭证/租约后返回原完成结果。导入不调用提供方，也不虚构缺失的原始 attempt 历史。

实际停用源并完成提供方审计后，`acknowledge_recovery` 接受绑定精确 backup/generation 的已有 `run-recovery-acknowledgement` 文档。其中 `no_missing_effect_intents` 是恢复操作者的断言，不由模型或服务推断。已知未解决 effect 仍会阻止确认。清除 barrier 不会恢复暂停中的 run。

新 generation 保护恢复数据库，但无法停止仍存活的源服务，也不能撤销源的提供方凭证。源停用和提供方 fencing 仍是部署责任；受控迁移与 ownership 交接属于 R10 工作。

## 可执行证据

```sh
# WORKFLOW_TEST_POSTGRES 必须指向容器使用的可丢弃数据库。
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo build -p workflow-cli --locked
python3 examples/remote/database-recovery.py target/debug/workflow --container POSTGRES_CONTAINER_ID
# Podman 主机还需传入 --container-engine podman。
```

CLI 演练创建两个唯一数据库，以及独立于二者的提供方 gateway。它发布由 assignment 派生的产物，创建完整数据库快照，在快照后执行一次真实 fixture-provider 写入，停止源，恢复并 fence 目标，验证原已完成状态与产物字节。旧凭证失效。导入真实备份后 intent/receipt 前，新写入准入持续被阻止。错误角色和篡改导入失败；重复导入保留一次完成及**一次提供方写入**。认证 actor 替换 payload actor 声明。只有 fixture 源已停止、唯一写入已完成对账后才最终确认。两个数据库和所有子服务均会清理。

Rust 恢复契约还会损坏较晚产物，证明较早 run 变更和全部凭证撤销都被回滚。它检查旧 lease/result 被拒绝、跨 tenant 凭证替换、完成状态、产物获取，以及重复恢复不改变截止时间/暂停。

| R04 标准 | 证据 |
| --- | --- |
| 提交前/中/后终止；保留后继 Outbox | SQLite `killed_processes_never_leave_partial_state_or_lose_committed_outbox`；PostgreSQL `real_postgres_parity_fencing_atomicity_and_outage_contract`；HTTPS scheduler/worker 终止契约 |
| CAS、重复事件/完成、取消竞态 | SQLite 独立进程及最终完成/取消测试；PostgreSQL assignment 结算与 fencing 测试 |
| wait、timer 和有界循环跨重启保留 | SQLite 场景/checkpoint 测试；R06 PostgreSQL/HTTPS 重启契约保留 wait 身份、审核对象和原截止时间 |
| 不重做已提交工作；对账未知 effect | R02 模型记录重启矩阵；R05 query-first HTTP 崩溃 fixture；本演练的备份后提供方回执导入 |
| checkpoint/tail 等于完整回放；损坏/缺失记录失败 | SQLite 周期 checkpoint、journal 损坏和完整回放测试；PostgreSQL run-image/绑定检查；恢复产物损坏回滚 |
| 满存储/不可写存储不能确认成功 | SQLite 真实磁盘满/只读测试；PostgreSQL 事务失败/断连契约；同步 authority 写入 |
| 备份验证活动 run、历史和产物；明确 RPO/RTO | [本地备份恢复](backup-recovery.md)、完整 PostgreSQL 归档演练，以及上述原子恢复契约 |

这些契约由仓库 Rust、Bazel 和可丢弃 PostgreSQL CI 任务执行。进程崩溃/重启不同于所有存储丢失。备份快照决定 RPO：之后提交的数据库状态可能缺失，之后的外部 effect 需要对账。恢复归档不承诺 RPO=0。演练报告归档字节数、dump 时间、到达已验证 hold 状态的时间，以及二进制摘要。人工/源/提供方对账时间属于额外业务 RTO，小 fixture 的耗时不是生产保证。

已提交产物和活动恢复依赖都会保留。现有传输清理仅移除符合条件的未完成传输，不删除已提交产物或 run 历史。对象存储生命周期策略和受控在线迁移分别属于 R07/R10/R14 的范围。

<!-- book-navigation -->

[目录](README.md) · [English](../shared-recovery.md) · [上一章: 集群调度](cluster-scheduling.md) · [下一章: 流程定义验收](definition-acceptance.md)

<!-- /book-navigation -->
