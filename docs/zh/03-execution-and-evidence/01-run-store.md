# 持久化运行存储

`workflow-runstore` 定义存储端口，`workflow-runstore-sqlite` 通过 SQLite 实现，并有明确 Bazel 库。
内核仍无 I/O 地计算控制流，存储提交状态、接纳事件和命令意图，提供持久化进度与可检查 outbox。
[本地执行器](02-local-execution.md)通过持久化租约和 attempt 增加只读分发；
[外部操作宿主](../04-effects-and-recovery/02-durable-effects.md)单独执行受管理写入及[声明式补偿](../04-effects-and-recovery/03-ordered-compensation.md)。
存储端口自身不启动后台进程；无人值守执行使用独立配置的[本地守护进程](03-local-daemon.md)。

## 本地 CLI 示例

使用新的明确数据库路径。只有 `run init` 创建数据库，其他命令要求已有兼容存储。CLI 查询以写权限
打开已有文件，让 SQLite 恢复中断的回滚日志；其 SQL 事务仅查询逻辑运行数据。Rust 适配器还提供
`open_readonly`，用于已完成恢复的只读存储。定义注册表数据库使用不同 application ID，不能传给这些命令。

```sh
cargo run --locked -- run init /tmp/review-runs.db
cargo run --locked -- run start /tmp/review-runs.db examples/runs/review-start.json
cargo run --locked -- run status /tmp/review-runs.db demo-review-approved
cargo run --locked -- run outbox /tmp/review-runs.db demo-review-approved 0 100 pending
cargo run --locked -- run acknowledge /tmp/review-runs.db examples/runs/timer-delivered.json
cargo run --locked -- run event /tmp/review-runs.db examples/runs/review-approved.json
cargo run --locked -- run event /tmp/review-runs.db examples/runs/implementation-completed.json
cargo run --locked -- run history /tmp/review-runs.db demo-review-approved 0 100
cargo run --locked -- run verify /tmp/review-runs.db demo-review-approved
```

这些契约、审批、任务结果与投递回执都是**模拟数据**，不注册定时器服务，也不调用编码适配器。
每次 CLI 打开新连接；进程退出保留已提交状态，但没有后台进程推进它。所有 dispatcher 缺席时，
`run status` 仍可能显示运行/等待。真实宿主观察到 deadline 到期后，可通过 `run event` 提交经过验证的
`advance_time` 事件。

`run cancel <db> <run-id> <event-id> <expected-revision> <at-unix-ms>` 使用真实运行指纹记录全局取消，
仍受 CAS 约束；已签发工作可能使运行停留在 cancelling，直到明确任务结果到达。取消不撤销外部写入。
事件形状见 `workflow schema kernel-event`，启动与回执输入分别为 `run-start`、`run-receipt`。
JSON 严格解码，上限 2 MiB。

[运行参考](../../../skills/workflow-cli/references/run.md)介绍持久化操作，
[回放参考](../../../skills/workflow-cli/references/replay.md)介绍模拟与文件检查点，
[内核语义](../02-process-definition/04-kernel-semantics.md)定义业务状态、分支、取消、循环与 deadline 行为。

## 事务与标识契约

| 操作 | 持久化行为 |
| --- | --- |
| 启动 | 在一个 immediate 事务内校验/规范化 bundle，登记不可变流程/能力版本摘要，写入种子、revision 1 状态、初始检查点、投递头和初始命令 |
| 应用事件 | 写事务内读取并验证权威运行，检查 expected revision，追加事件、更新状态、按需创建检查点并追加命令，再提交 |
| 相同启动重试 | 相同运行 ID 与语义相同输入返回当前状态及空的 duplicate 变更；bundle/输入/时间/限制变化返回 `start_conflict` |
| 相同事件重试 | 相同事件 ID/内容返回当前状态，无新命令，终态后同样如此；内容改变或 revision 过期不会产生部分写入 |
| 投递确认 | 原子追加不可变回执并推进独立有序摘要链；不改变运行 revision，也不标记任务完成 |
| 读取/verify | 使用一致读取事务，恢复数据缺失/损坏则失败；不推进时间或调用适配器 |

同一流程或能力 ID/version 在该存储中不能得到不同摘要，即使通过另一个运行写入。内容改变必须发新版。
本地目录记录提供且经过检查的内容，不认证外部注册表，也不授权声明的能力。所有历史运行保留 bundle/
定义锁，没有删除或保留期命令。

SQLite writer 使用 `BEGIN IMMEDIATE`、外键和 `synchronous=FULL`，锁等待最多五秒。只有 `commit()`
成功才返回修改成功。Busy、只读、磁盘满和其他存储错误均返回错误。运行 revision 通过显式条件更新逐次
增加；SQL 触发器禁止普通 SQL 修改/删除种子、bundle、绑定锁、事件、检查点、命令与回执。

提交后进程死亡或输出丢失时，调用者可能不知道是否已提交。先读取运行，再按**准确原身份**重试启动、
事件或回执；不能虚构新 ID 掩盖不确定回复。CLI 退出 0 只表示存储操作成功；修改响应检查
`result.snapshot.status`，查询检查 `result.status`。请求/事务被拒绝退出 1，用法、输入 I/O 或输出失败
退出 2。超过 2 MiB 的响应报错，但可能已提交；使用分页历史/outbox，并在不确定响应后重读状态。

## 日志、检查点与损坏检查

每个运行保留不可变种子与 bundle、当前状态/摘要、接纳事件日志和确定性 outbox。初始 revision 1
代表种子，事件从 revision 2 连续排列。在 16、32、48 等 revision 的变更事务中保存周期检查点，使用
内核 checkpoint v1，包括已接纳前缀日志与完整性校验和。

恢复验证所有记录，完整回放事件日志，恢复每个保留检查点，再从最新检查点推进尾部，比较这两种结果
与当前状态/摘要。检查点前缀或 revision 缺口均报错。重建命令只用于比较，不分发；缺失、多余或变化的
outbox 记录均失败。回执必须形成有序前缀、匹配准确命令及独立投递头/数量/摘要链，防止丢失尾部回执
把已投递工作悄悄变回待处理。

这些检查发现损坏与偏离，不能防御能重写全部记录和摘要的数据库所有者。外来或未来 schema/application
被拒绝。`run migrate <db> <new-backup-file>` 显式把 schema 1–10 原子升级为 11，见本地执行章节；
不提供覆盖式修复。读取会回放有界历史和保留检查点前缀，优先保证完整性证据，不是常数时间快照加载器。
仍受内核事件/frame/序列化预算约束；长时间运行的保留/压缩尚待实现，不声称生产吞吐量。

## Outbox 与宿主

每条意图有运行内序号、revision、命令下标、命令摘要，以及稳定的
`command_id = digest(run_digest, revision, command_index)`。回放、重启和重复事件保留这些身份。
投递回执绑定运行、命令 ID/摘要及稳定宿主 delivery ID；完全相同重试接纳，改变的回执拒绝，确认必须按序。

`run outbox ... pending` 重复展示未确认意图。真实宿主必须先持久化投递/注册，再确认。确认丢失后可能
再次投递，目标必须按稳定 command ID 去重。读取 pending **不取得租约**，多个读者不能视为有权并发
分发者。`run drive` 取得运行租约、保留 execute/cancel 顺序、持久化 attempt，并通过执行端口校验结果。
远程认证与写操作身份仍由宿主负责。投递回执不等于任务结果或外部效果证明。本地执行器支持只读任务和
持久化定时器登记；写能力使用独立的持久化外部操作驱动。

`run list` 使用词典序运行 ID 游标，history 使用不含当前 revision 的游标，outbox 使用不含当前序号的
游标，支持 `all` 或 `pending`。Limit 为 1–100，`next_cursor: null` 结束当前页流。回执会改变 pending
集合，因此这些页面是检查结果，不是持久化领取。

## 已验证故障边界与 R04 后续工作

测试通过独立进程，在事务前、事件/状态写入后、提交前以及提交成功但尚未输出后强制终止，验证恢复后
只有旧完整状态或新完整状态；竞争 writer 检查 CAS，故障注入检查只读和 SQLite 磁盘满不能确认成功。
这些实验不证明任意 VFS/文件系统断电持久性、共享网络盘多 writer 安全或磁盘丢失恢复，也不声称 RPO/RTO。
租约、结果提交和有界只读重试以及暂停/恢复见本地执行；写重试与账本见外部操作章节。
[本地备份/恢复](../04-effects-and-recovery/04-backup-recovery.md)覆盖一致快照、迁移与新所有权代际。
[本地守护进程](03-local-daemon.md)、[共享产物](../05-shared-execution/04-shared-artifacts.md)及[共享恢复](../05-shared-execution/07-shared-recovery.md)
记录后续的定时器、依赖和归档验收，保留策略与环境限制在对应章节明确说明。

Schema 3 增加执行结果的必需产物依赖验证。带证据的运行应配置 `run --artifacts <store>`；未配置或依赖
损坏会使读取和修改失败，见[类型化产物](05-artifacts.md)。

Schema 4 保护[强制后置条件](08-runtime-postconditions.md)。恢复根据历史执行前缀及保留产物重算每个
门禁决策，拒绝原始 gate 事件和手工 gate 回执；含后置条件的运行还拒绝原始任务成功事件。策略版本绑定
不可变内容。已有运行带证据时，升级也要配置产物读取器，使依赖失败能够回滚迁移。

Schema 10 增加持久化恢复所有权和外部操作核对；一致 SQLite 镜像与保留产物遵循本地备份协议，不能用
复制正在运行的文件替代。Schema 11 增加受保护定义迁移和保留的存储升级记录。
[版本迁移](../04-effects-and-recovery/05-version-migration.md)说明可评审计划、新结果/审批策略、历史快照和经过验证的存储回滚。

<!-- book-navigation -->

3.1 持久化运行状态

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/01-run-store.md) · [上一章: 2.5 可评审的 SOP 模板](../02-process-definition/05-reviewed-templates.md) · [下一章: 3.2 本地执行](02-local-execution.md)

<!-- /book-navigation -->
