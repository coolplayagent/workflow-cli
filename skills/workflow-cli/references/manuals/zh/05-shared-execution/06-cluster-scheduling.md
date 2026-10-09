# 共享调度与 R09 验收

集群模式将 TLS 控制 API、按 project 限定的 scheduler 和执行 worker 分开。PostgreSQL 持有全部 run 租约、node attempt、assignment 交付、准入许可、worker 存活状态和死信。所有 API 副本观察同一 authority。scheduler 的范围是其认证 tenant/project；分区内的 ownership 由 run 租约决定，不依赖进程本地的领导者标志。同一分区内可有多个 scheduler 竞争。每份取得的租约都有 owner、获取身份、按数据库时间计算的到期时间，以及递增 epoch。

## 启用与运行

先升级 API 副本，排空现有执行授权，再安装可信主机配置。`examples/cluster/policy.json` 是有界示例，不是生产容量建议。将其包装为：

```json
{
  "tenant": "team",
  "expected_revision": null,
  "policy": { "...": "contents of examples/cluster/policy.json" }
}
```

```sh
workflow service configure-scheduling server.json cluster-configuration.json
workflow remote managed-work worker-client.json examples/cluster/worker.json 100000 40
workflow remote schedule scheduler-client.json scheduler.json 100000 40
```

配置操作只在可信数据库主机本地执行，不暴露为 RPC。首次激活会拒绝仍未结算且未到期的 task/effect grant，因此必须先排空。之后更新需提交此前返回的精确 `expected_revision`。部署变更记录应保留返回的 revision/policy digest。激活创建 scheduling schema 1，并将 access schema 升到 3。旧 API 二进制在请求时拒绝此 schema，不能提供绕过配额的流量。`migrate-access` 保留 schema 3。未配置策略的 tenant 在兼容 API 下保持旧调度行为。数据库 owner 直接使用的 SDK 属于可信主机代码，必须随 API 一起升级。

调度器配置示例：

```json
{
  "instance_id": "scheduler-a",
  "cluster": true,
  "effects": false,
  "worker_ids": ["issued-worker-credential-id"],
  "lease_ms": 120000,
  "scan_limit": 100
}
```

每个需要持续推进的 project 都应运行 scheduler 池；凭证不会静默跨越 project 边界。worker 池规模与 tenant/project 限额应一起配置。此实现不分配主机，也不承诺不同 project 部署间的最低 CPU 份额。避免多个进程共用一个 worker 凭证，因为存活状态、排空及每 worker 限额都属于该身份。

## 准入、公平性与传输

tenant、project、精确能力 ID/版本、model pool 和 worker 各有显式并发 grant 上限及滑动 60 秒准入限额。project/capability/model 覆盖项均有界；model pool 将冻结策略 ID/版本映射到运维选择的提供方池。未映射策略使用自身精确身份。限额统计**执行 assignment**，包括 model-task assignment，不是单次模型 HTTP 调用、token 或金额。R02 冻结模型策略约束每个已准入任务内的调用、工具和 token。提供方计费预算需另行管理。effect 写入与对账查询都会消耗许可。

所有 API 副本通过 tenant 策略行串行化配额决策。执行 claim、持久 assignment 和准入许可一起提交。完成会释放并发槽，但保留该分钟的速率 token。到期释放 grant 槽；撤销/释放的工作可能保守地占槽直到原截止时间。调低策略会阻止新 grant，直到用量符合限额；不会追溯取消已经授权的外部 I/O。旧式单 worker dispatch 也受已安装配额约束。

有界活动 run 队列以事务方式拒绝超额启动/恢复。启用策略时发现的已有活动 run 可继续保留。在 project 内，候选排序依据为上次选中时间加 `(9 - priority) * priority_step_ms`，其次是 run ID。priority 范围 0–9；等待更久的低优先级候选最终会超过刚被选中的高优先级候选。选择只是提示，精确当前租约仍决定 ownership。独立 tenant 配额锁防止一个 tenant 消耗另一个 tenant 的限额。这是带优先级老化的有界轮转，不是加权全局调度器，也不承诺生产延迟。

持久 assignment 表本身就是传输通道。worker 反复扫描其范围内的 pending 记录，不依赖可能丢失的外部队列通知。state/Outbox 变更与 assignment 准入处于同一事务；结果与持久 Inbox/回执去重也一起提交。断连 poller 通过扫描恢复；提交后丢失 dispatch 响应不允许重复外部写入。effect 交付是一次性的，后继执行者在考虑写入前先查询 R05 账本/提供方。

## 心跳、续租与滚动排空

托管 worker 注册固定 runtime 版本，心跳独立于同步能力执行。每个 assignment 仍由精确能力、模型策略和 effect 契约授权。runtime 版本是主机声明，不是二进制证明。心跳缺失/过期、身份正在排空、或版本已移出 allowlist 时，新工作会延后。心跳失败会停止托管 worker 接受更多工作。成功心跳不能复活过期执行 grant。

集群 scheduler 在有效租约剩余时间小于配置寿命的一半时续租。续租保留 epoch，并原子更新存储的 assignment 租约身份。冻结的 task/call 截止时间和准入到期时间绝不增加。过期 node attempt 在更晚的 attempt/lease epoch 下回收，迟到完成不能改变 run。每次持有租约的调度扫描仍处理持久 timer 和 Inbox 对账，包括进程重启后。

`managed-work` 组合内置只读工作、可选 `models`（bundle 和主机绑定路径）以及可选 `effects`（主机绑定路径）。所有共享提供方绑定必须使用相同的认证执行主体。绑定格式见模型和 effect CLI 示例。

收到 SIGINT/SIGTERM 后，独立监控器会请求服务端持久排空，同时允许当前调用完成。管理员 `control_worker` 也可排空身份。数据库确认排空后拒绝新 assignment；已接受的 assignment 继续结算，直到有效 grant 为零或报告排空错误。只有这时 CLI 才返回 `drained: true`。重启或普通心跳不能清除排空；必须由管理员显式恢复。新版本使用新 worker 身份安装，加入 scheduler 路由，再在排空后停用旧版本。尝试其他兼容 worker 时，精确契约不匹配不会留下推测性 attempt。

排空超时从当前同步适配器返回之后开始计算。此库不能强行抢占 Rust 回调。内置/model/HTTP 绑定执行自身已有截止时间；不配合的主机适配器需要进程隔离和服务管理器终止。进程被杀后仍需等待租约过期，不确定写入还需 effect 对账。scheduler 退出后，其持久 grant 可在到期后恢复，不会被报告为已排空。

## 死信与恢复

永久不兼容路由或 worker 执行错误会停放 run，记录稳定原因、run revision 和 snapshot digest。配额繁忙或 worker 暂时失活则延后处理。`dead_letters` 列出活动记录；`dead_letter` 获取单条记录及其不可变 resolution receipt。只有作用域内 recovery 凭证能调用 `resolve_dead_letter`，且需提供经过审核的精确当前 revision/digest、retry/archive 选择和有界原因。快照变化后必须重新审核。服务将回执绑定到认证 actor 和时间；相同重试幂等。不能归档活动 run，也不能重试终态 run。重放恢复普通权威 dispatch（包括 R05 对账），不会执行过时的序列化任务或绕过 effect 检查。

| 故障模型 | 恢复边界与 RTO | RPO / 外部副作用 |
| --- | --- | --- |
| PostgreSQL 健康时 scheduler/worker 进程丢失 | 等待剩余 ownership/attempt/permit 到期，再成功扫描、claim 和执行；扫描/I/O 延迟叠加于租约时间 | 已确认数据库事务零丢失；只读执行可能重复 |
| Worker/API 网络分区 | 没有成功心跳/claim/commit 就没有新确认的 authority；连通后遵循扫描和租约规则 | 已提交 assignment 持久保留；未知写入需要 R05 查询/人工对账 |
| 数据库暂时不可用或只读 | 不确认状态转换；恢复可写服务后，由扫描/租约对账决定恢复时间 | 已提交状态、assignment、timer 一起保留；不承诺固定数据库故障 RTO |
| 整库丢失/恢复 | 验证完整归档、隔离恢复、凭证/ownership fencing、运维/提供方对账，再显式恢复 | RPO 为备份/WAL 边界，不是零；备份后的缺失 effect 需独立对账 |

`fence-restored` 在同一恢复事务中也会将 scheduling permit 标记为 finished，并令全部保留 worker 进入 draining。完成[数据库恢复流程](07-shared-recovery.md)后，才能重新配置新身份并恢复被 hold 的 run。提交 fencing 无法证明旧进程已停止外部 I/O；该独立边界由 [effect 协议](05-remote-effects.md)定义。

## 可执行证据

```sh
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo test -p workflow-service --locked -- --ignored --nocapture --test-threads=1
cargo build -p workflow-cli --locked
python3 examples/cluster/acceptance.py target/debug/workflow
```

这些命令要求可丢弃的 `WORKFLOW_TEST_POSTGRES`，由必跑 PostgreSQL CI 任务执行。常规 Cargo/Bazel 检查也会编译全部契约。R09 进程 fixture 启动两个活动 scheduler 和三个 worker，终止 owner，对旧 worker 执行 SIGSTOP/恢复，检查迟到完成被拒绝，对比本地/集群最终状态，加载两个 tenant，并验证版本 2 worker 排空。它记录总吞吐、nearest-rank p95 准入延迟、较小 tenant 的 p95、租约/扫描设置和机器配置。60/90 秒测试上限是观察边界，不是生产 SLO。

2026-09-30 的一次本地运行使用 Linux x86_64、Intel i7-1260P、16 个逻辑 CPU，报告内存 16,061,032 KiB。实测：租约 8,000 ms、扫描 40 ms、owner 丢失恢复 10,262 ms；15 个排队任务耗时 28,355 ms（0.529 jobs/s）；总体 p95 准入延迟 18,791 ms，较小 tenant 为 7,081 ms。这个小型持久 image fixture 在共享开发机上执行真实 TLS/数据库操作，不是容量基准。每次 CI 都输出自己的测量值。

CLI fixture 在真实模型提供方请求进行中发送 SIGTERM，观察执行期间排空、确认完成并保留模型速率 token，然后从 runtime 1.0.0 滚动到 2.0.0。可选 `--legacy-binary` 检查确认 R09 之前的二进制拒绝 access schema 3。PostgreSQL 契约覆盖并发配额竞争、所有准入范围、续租/截止时间保持、过期心跳、只读回滚、审核后的死信重试，以及 worker 替换后 effect 查询。已有多进程和数据库恢复 fixture 继续覆盖 dispatcher 丢失、断连持久扫描、timer 和恢复后的 effect。

<!-- book-navigation -->

5.6 集群调度

[全书目录](../README.md) · [5. 共享执行](README.md) · [English](../../en/05-shared-execution/06-cluster-scheduling.md) · [上一章: 5.5 远程外部操作](05-remote-effects.md) · [下一章: 5.7 共享备份与恢复](07-shared-recovery.md)

<!-- /book-navigation -->
