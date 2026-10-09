# 本地只读执行

`workflow-runtime` 通过 `ExecutionStore` 与经过检查的 `Worker` 驱动内核持久化 outbox，只依赖端口，
生产依赖不包含 SQLite 或提供方 SDK。`workflow-runstore-sqlite` 实现事务执行端口，CLI 选择内置编译器
适配器。每个模块都有独立 Bazel `rust_library`。

## 执行真实契约

```sh
cargo run --locked -- run init /tmp/inspect-runs.db
cargo run --locked -- run start /tmp/inspect-runs.db examples/execution/valid-start.json
cargo run --locked -- run drive /tmp/inspect-runs.db inspect-valid local-agent 10
cargo run --locked -- run status /tmp/inspect-runs.db inspect-valid
cargo run --locked -- run execution-history /tmp/inspect-runs.db inspect-valid 0 100
cargo run --locked -- run verify /tmp/inspect-runs.db inspect-valid
cargo run --locked -- run drive /tmp/inspect-runs.db inspect-valid local-agent 10
```

示例对内嵌真实定义文本调用 `workflow.validate`，检查结果，再让 SOP 决策选择业务终态。第一次 drive
执行一个任务，第二次零个。适配器是本地只读能力，无需模型或网络。`invalid-start.json` 使用运行 ID
`inspect-invalid`，真实检查返回 `valid=false`，流程结束为 `failed`。CLI 退出 0 只表示 drive 完成，
必须检查 `result.snapshot.status`。

Start 文件绑定准确流程与能力描述符。通过 `capability describe <id> <version>` 取得描述符，名称一致
不够，契约摘要还必须匹配注册适配器。示例起始时间 1000 只因流程立即开始任务而适用；真实等待或循环
需要当前起始时间，否则 deadline 可能已到期。

`run drive <db> <id> <owner> <max-commands>` 处理 1–100 条有序命令，返回快照、任务/命令/定时器计数和
`idle` 或 `budget` 停止原因。CLI 新建 acquisition ID，使用 120 秒租约。`idle` 可能表示等待，不代表成功；
退出后没有后台进程。后续显式 drive 推进到期等待/循环并继续工作。无人值守轮询及服务状态使用可选
[本地守护进程](03-local-daemon.md)。`run execution-history` 使用从 0 开始、不含当前序号的游标、1–100
页大小及 `next_cursor`。

## 所有权与调用顺序

1. 在 SQLite immediate 事务中取得租约；每个运行一个所有者，acquisition ID 唯一，epoch 单调递增。
2. 验证全部运行/执行历史，选首条未确认命令，先持久化准确请求与 grant，再返回可调用 attempt。
3. 在数据库事务外调用适配器，请求绑定运行、节点实例、attempt、epoch、定义、能力契约与输入。
4. 新 immediate 事务内验证当前所有权、请求身份、deadline、输出/错误契约和节点资格；一起提交 worker
   结果、内核事件、状态/检查点/后继 outbox、原命令回执及执行日志，提交后才确认。
5. 正常完成或报告执行错误后释放租约；崩溃留下所有权直到过期，新所有者取得更高 epoch。

可信宿主时钟在取得事务锁并完成恢复检查后采样，提交接纳前再次采样。零值/时间回退拒绝，
`now >= expires_at` 已过期。排队调用者不能提交等待写锁前采样的时间。SQLite 串行化所有接纳修改。
续租延长有界租约并使旧 token 失效，保持 epoch，不延长已准备请求的 deadline。租约最长五分钟。
Worker 必须配合 deadline；过期限制结果接纳，但不能终止任意同步 Rust 代码或撤销效果。这是可信本地
时钟/进程/存储边界，不是分布式时钟或远程认证。

旧所有者在释放、过期或接管后不能完成未提交 attempt。成功提交后准确重试结果返回 duplicate，无新命令，
租约过期后也如此；同 attempt 改变结果则冲突。读取已提交事实不授予新所有权。

## 失败与重试

| 观察 | 行为 |
| --- | --- |
| 适配器缺失或 worker 协议拒绝 | 把错误保留为 attempt 观察，任务/命令仍 pending；drive 退出 1，尽可能释放租约 |
| 请求持久化后、结果提交前崩溃 | 保留 prepared attempt；接管后可新建只读尝试，旧 epoch 不能提交 |
| 结果事务中崩溃 | 恢复旧完整状态或新完整结果/事件/回执，不会部分成功 |
| 结果提交后、回复前崩溃 | 重开并检查历史，已完成命令不再调用 |
| 提交前过期或时钟回退 | 结果、事件、回执与执行日志一起回滚 |
| 调用前取消 | 不调用适配器，结束未启动只读任务并确认取消意图 |
| 调用中取消 | 保留经过验证的迟到结果，业务状态仍由内核取消决定 |
| 不确定操作/核对命令 | 停止，等待经过验证的宿主核对结果，不能虚构完成 |
| 非空产物证据 | 要求配置 ArtifactReader，检查清单、字节、类型、祖先及准确生产者/请求/输入；缺失或损坏拒绝 |

每条命令最多三次 prepared attempt，包括遗留未决尝试和失败调用。Worker/协议错误使 drive 停止；
后续显式 drive 可用新 acquisition 重试只读工作，不重置预算，不自动退避，也不重试写操作。
每个运行执行日志最多 10000 条，取得/续租/释放和观察都计数，还受内核预算约束。耗尽报错，不丢弃历史。

CLI 错误包装分别保留 storage/worker/release 错误；release 错误可能发生在结果已提交后。重试前检查状态
和历史，不能仅凭退出 1 或回复丢失推断回滚。Acquisition ID 是单次身份，不是幂等请求 ID。取得租约的
回复不明确时，检查历史并等待过期，再获取新租约。调查任务时保留原请求/结果身份。

此执行器只接纳声明 `read_only` 的能力，并信任适配器遵守声明。遗留未决尝试后可能至少一次调用；
去重的是结果提交，不实现外部效果恰好一次、租户隔离或授权。

## 持久化控制意图与恢复

等待/deadline 注册体现为持久化内核快照；本地 driver 确认意图并在每次 drive 扫描它，不表示安装了 OS
定时器或外部服务。取消保留意图顺序，execute/cancel 不能重排成新调用。

每次正常读取/修改都回放执行日志，验证独立计数/摘要链，重建租约/attempt 权限，从不可变命令重建请求，
按已提交事件和回执检查完成结果。尾部缺失、旧所有权与绑定冲突都失败。能重写所有事实及摘要的数据库
所有者不在完整性边界内。恢复不调用适配器。

`run event`、`run acknowledge` 是可信管理宿主接口，不取得执行租约或认证外部观察。不能向不可信 worker
开放，也不能手工确认受管理任务来绕过结果检查。外部服务必须通过受所有权约束的端口接纳结果并认证
调用者。人工信号/取消与执行并发时依靠既有内核 revision 检查。

## 持久化暂停与继续

使用 `run status` 得到的当前 revision 和稳定控制身份：

```sh
workflow run pause runs.db run-id pause-1 7 1790559000000 'maintenance'
workflow run status runs.db run-id
workflow run resume runs.db run-id resume-1 8 1790559060000 'maintenance finished'
```

`pause`、`resume` 在同一事务提交内核事件与快照。Reason 非空，最多 1024 字节，不应包含秘密。
暂停期间 `status`/`list` 暴露 `pause.reason` 和 `pause.at_unix_ms`，业务 `status` 仍为 `running`。
相同重复事件返回当前快照，不重复控制。身份载荷变化、revision 过期、重复暂停、未暂停却继续、取消/
终态后控制都拒绝。回复丢失时重试准确 ID、revision、时间和 reason。

暂停阻止新任务/门禁领取、时间推进和自动后继激活。`drive` 释放租约并返回 `stop_reason: paused`，
不新调用能力或模型。已有 prepared 工作仍可按原租约/deadline 执行与提交，因此暂停是接纳边界，不是
进程挂起或回滚。结果与不确定观察仍保留，但继续前不能触发新下游工作。Pending 命令和实例 ID 保留，
继续不会重发已完成任务。

时间保持绝对：暂停不延长 wait、loop、lease、task 或门禁证据 deadline。继续先处理过期等待/循环再
驱动后继。最后任务完成时仍可能因暂停保持 running，直到继续化简剩余控制流。暂停时仍可取消，清除
暂停并走普通取消/核对。原始 signal、gate、retry-gate 和 time 事件在暂停时被拒绝；
[持久化 Inbox](../04-effects-and-recovery/01-event-inbox.md)缓冲可信回调，认证审批接入见 [R06 验收](../06-acceptance-and-maintenance/03-approval-acceptance.md)。

控制使用可信管理宿主边界，调用者 reason 只是审计上下文，不是经过认证的 actor。测试覆盖检查点回放、
结果排空、暂停期间无新调用、过期等待、旧继续冲突，以及五个提交阶段杀死 pause writer。

## 显式存储迁移

新数据库使用 schema 11，旧 schema 1–10 需要验证备份后显式升级，普通 create/open 不迁移：

```sh
cargo run --locked -- run --artifacts /path/to/artifacts storage-plan /path/to/existing-runs.db
cargo run --locked -- run --artifacts /path/to/artifacts migrate /path/to/existing-runs.db /path/to/new-before-upgrade.sqlite
```

升级事务内比较来源摘要，所有保留历史、执行证明和产物依赖都必须通过验证后才能提交版本/日志。
失败保持旧 schema。[版本迁移](../04-effects-and-recovery/05-version-migration.md)说明来源绑定计划、存储日志、回滚至保留旧二进制、
共享镜像转换以及独立的定义迁移。逻辑 StartRun 仍为 schema 1，直接 worker 为协议 1，模型策略为协议 2。
存储升级不重启定义或撤销外部业务效果。

## 证据与范围

测试涵盖两个独立进程抢租约、接管隔离、续租/释放/时间边界、伪造结果、有界重试、真实内置业务决策、
存储拒绝后不调用，以及提交后重复 drive。在结果事务前、事件/状态/执行写入后、提交前后强制终止，
重开恢复完整事务，只重试遗留未决尝试。迁移保留 v1 运行，损坏时回滚。Cargo 与 Bazel 执行相同测试。

[远程服务](../05-shared-execution/03-remote-service.md)、[工作区验收](../06-acceptance-and-maintenance/04-artifact-acceptance.md)、[集群调度](../05-shared-execution/06-cluster-scheduling.md)
和[安全验收](../06-acceptance-and-maintenance/06-security-acceptance.md)分别说明共享、工作区和权限契约。本地 Linux 进程崩溃实验不证明
生产吞吐量、断电持久性、共享网络文件系统安全、业务收益或 RPO/RTO。

宿主管理 worker/报告时使用 `run acquire/claim/finish/release`。带证据运行的后续读取/修改都需要
`run --artifacts <store>`。产物位置属于宿主配置，缺少依赖不能返回健康恢复结果。内置 drive 不虚构报告，
而是通过存储验证注册适配器提供的证据。

## 强制后置条件

`claim` 与 `drive` 在当前租约下消费冻结 gate 命令，任务提交后仍可能等待门禁，需要检查业务状态和
决策。PASS 原子提交变更、回执与证明，UNKNOWN 消费一次意图并等待显式 `retry-gate`。绑定、证据选择、
修复上限和受保护事件接入见[运行时后置条件](08-runtime-postconditions.md)。

## 受管理写操作

独立的 `EffectStore` 与 `drive_with_effects` 在相同租约下持久化写意图并提交提供方观察。CLI 使用
`run drive-effects`，普通 drive 继续使用只读 worker。稳定键、查询恢复、有界重试、手工核对及提供方
隔离限制见[持久化外部操作](../04-effects-and-recovery/02-durable-effects.md)。Schema 8 增加策略与操作日志，schema 9 增加
[有序补偿](../04-effects-and-recovery/03-ordered-compensation.md)。显式升级到 11 接受 1–10；恢复存储在再次接纳写入前必须通过
[恢复屏障](../04-effects-and-recovery/04-backup-recovery.md)。

<!-- book-navigation -->

3.2 本地执行

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/02-local-execution.md) · [上一章: 3.1 持久化运行状态](01-run-store.md) · [下一章: 3.3 本地守护进程](03-local-daemon.md)

<!-- /book-navigation -->
