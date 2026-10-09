# 经过认证的远程 effect 与 R05 验收

远程写入任务与本地执行使用相同的冻结策略、operation key、重试预算、effect 账本和补偿规则。HTTPS 服务在 PostgreSQL 中原子提交 intent 与 worker assignment。worker 在事务外调用显式配置的 gateway，然后提交观察结果。提供方凭证只保存在 worker 主机绑定中，从不成为 assignment 数据。只允许写入能力而未提供精确 effect 策略的 allowlist 不会授予写入权限。

## 配置与执行

新作用域在可信 bootstrap 时初始化附加的 `workflow_effect_dispatch` schema。已有部署需显式执行：

```sh
workflow service init-effects server-binding.json
```

初始化具备事务性和幂等性，拒绝未知 schema 版本。已有只读调度器不需要新 schema。这些表保存 assignment、交付状态和回执；run image 继续使用已有 schema 与 effect journal。

对每个允许的写入 descriptor，worker 凭证的 `CapabilityRule` 包含精确 `id`、`version`、`contract_digest`，以及以下附加字段：

```json
{
  "effect": {
    "policy": {
      "identity": {"id":"sandbox-release-policy","version":"1.0.0"},
      "target": {"id":"sandbox-releases","version":"1.0.0"},
      "call_identity": {"id":"sandbox-publisher","version":"1.0.0"},
      "retry": {"max_calls":6,"initial_backoff_ms":10,"max_backoff_ms":100,"total_write_ms":60000}
    }
  }
}
```

这只是策略片段，不是完整配置请求。descriptor digest 来自 `workflow_worker::Capability::new(descriptor)?.digest()`；策略必须等于 bundle 中对应的冻结绑定，包括重试预算。worker 凭证必须有效至调用截止时间。错误目标、主体、版本或预算会同时回滚 intent 和 assignment。每个声明的 compensator 都需单独配置规则。所配置的 gateway 必须执行自身的主体、资源和 operation key 权限检查。

在包含 `instance_id`、`worker_ids`、`lease_ms` 和 `scan_limit` 的调度器 JSON 中启用 `"effects": true`，默认值为 false。使用与本地 `run drive-effects` 相同格式的 HTTP gateway 绑定启动 effect worker：

```sh
workflow remote schedule scheduler-client.json scheduler.json 1000 100
workflow remote work-effects worker-client.json effect-bindings.json 1000 100
```

此 worker 也处理内置只读 assignment。其他适配器可通过库组合 `work_once` 和 `work_effects_once`。`observed_effect_calls` 统计已持久接受的观察，包括 UNKNOWN，并不是业务操作成功次数；结果应查看 run 和 effect 账本。调度报告区分等待重试和人工对账。

## 交付、完成与恢复

| 操作 | 授权角色与行为 |
| --- | --- |
| `dispatch_effect` | Scheduler 持有精确租约；验证同 tenant/project 内有效 worker 及其精确能力/effect 策略，然后原子准备 assignment。 |
| `pending_effects` | Worker 在交付截止时间前分页查询自己的未交付 assignment；通知丢失时可由此重建。 |
| `effect_assignment` | Worker 原子消费一次交付；返回的 attempt 必须与持久 journal 一致，且租约有效、当前节点允许执行。 |
| `observe_effect` | 被分配的 worker 提交观察；scope、attempt、lease 来自服务端记录。相同重试幂等，冲突回执失败。 |
| `effects` | 可读取 run 的角色分页读取已验证账本，包括稳定 operation key、调用及真实回执。 |
| `outstanding_effects` | Administrator/recovery 角色查看未解决 assignment，包括已交付工作和被撤销 worker。 |
| `resolve_effect` | Recovery 身份持有自己的有效租约并提交实际对账证据；认证 actor 替换调用方注释。 |
| `control` | Runner/recovery 发送类型化暂停、恢复或取消，附 event ID 和预期 revision；不存在任意事件/状态端点。 |

所有操作使用已有 protocol-1 请求/响应 envelope。新增操作名是增量扩展：旧服务会拒绝，旧 worker 命令继续使用普通 assignment。gateway 请求沿用 effect-attempt schema。响应 request ID 不能替代持久 operation key 或 assignment ID。

交付是**一次性**的，同一凭证下并发进程也遵循此规则。服务在返回写入 attempt 前提交 `delivered`。如果响应丢失，或 worker 在提交观察前退出，不要再次获取或执行同一 assignment。ownership 到期或显式释放后，后继租约先查看现有账本：支持查询则先查；只有声明了幂等保证才可重试；否则要求人工对账。intent、策略、input digest 和 operation key 保持不变，attempt ID 和 epoch 改变。缺少本地响应不能推断提供方写入成功，失败查询也不能抹除此前未知的写入。

调用截止时间阻止新的 I/O。只要 worker 凭证和持有租约仍有效，真实的迟到回执仍可结算。过期/替换租约不能提交观察，相同重复观察也不行。撤销操作通过凭证锁与其他操作排序，并持续检查到提交。已取消或暂停节点不能收到尚未交付的写入；已交付调用仍可返回真实回执。取消不会自动撤销外部资源。

人工对账前须释放调度 ownership 或等待其到期，用 recovery 凭证取得租约，然后提交 `resolve_effect`，字段为 `lease`、`operation_key` 和已有 `ManualResolution`。应提供真实提供方回执，或在检查提供方并停止旧写入者后确认操作未应用。在有效租约下，完全相同的 resolution 重试是幂等的。任意异常文本会被固定的提供方错误描述替换；结构化回执/输出仍是受作用域保护的应用数据，不是已经脱敏的任意遥测文本。

## 验收证据

测试使用隔离的提供方系统和可丢弃 PostgreSQL，不访问生产发布、PR 或部署凭证。

| R05 验收项 | 本地与共享证据 |
| --- | --- |
| 提供方写入后、回执提交前终止；仅一个逻辑 effect | SQLite 的 `real_http_write_then_killed_worker_is_queried_and_duplicate_delivery_creates_one_release`；HTTPS 的 `https_effect_worker_crash_recovers_provider_receipt_without_duplicate_write` 在独立 PostgreSQL 提供方提交后终止真实 worker/scheduler，验证恰好一次写入、一次查询、一个提供方资源。 |
| 重交付/竞态保留 key；不同循环 instance 使用不同 key | 本地 `two_process_claim_race_dispatches_only_one_durable_attempt`、`loop_instances_use_distinct_effect_keys_and_restart_preserves_each_receipt`；共享 `effect_authority_policy_rollback_and_single_delivery_race` 使用独立连接竞争，另有撤销 worker 与 HTTPS 接管检查。 |
| 不支持查询/去重时停止并进行可审计对账 | 本地 `unknown_without_guarantees_and_query_absence_require_audited_manual_resolution`；共享 `effect_unknown_without_lookup_requires_authenticated_manual_resolution` 拒绝错误角色/owner，记录真实 actor，验证持久幂等对账。 |
| 永久错误停止；瞬态重试保留边界 | 本地 `permanent_failures_settle_once_and_transient_calls_keep_backoff_and_budget`；共享 `effect_shared_retry_budget_and_permanent_errors_survive_reopen` 在调用之间重开服务，检查精确调用次数和最终状态。 |
| 有序补偿可从失败中恢复，已完成撤销不重复 | 本地 `killed_compensator_after_provider_commit_recovers_by_query_without_repeating_earlier_undo`；共享 `effect_shared_compensation_order_and_manual_takeover` 验证资源逆序、调用间重开、保留失败撤销供认证恢复，并确认每项已完成补偿只调用一次。 |
| 取消竞态保留真实回执 | 本地 `pause_drains_real_receipts_and_cancel_recovers_without_reissuing_write`；共享 `effect_control_cancel_and_late_receipt_preserve_truth` 检查交付前后、CAS/幂等性，以及有效租约下真实迟到回执。 |

执行常规仓库检查及必跑数据库套件：

```sh
cargo test --workspace --locked
# WORKFLOW_TEST_POSTGRES 必须指向可丢弃的数据库。
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo test -p workflow-service --locked -- --ignored --nocapture --test-threads=1
bazel test //...
```

独立数据库故障实验串行运行，防止无关测试消耗彼此真实租约/截止时间窗口。各竞态或接管测试仍保留竞争连接/进程及原有时间断言。

HTTPS 故障测试使用八秒租约并输出实测恢复时间。60 秒观察上限只是测试边界，不是生产 RTO。故障模型是进程丢失和旧 worker 迟到，不覆盖整个数据库丢失或提供方网络分区。仍要求提供方保留 key 并提供权威查询语义；状态 fencing 无法物理阻止已运行的旧 worker 联系外部系统。本项目不承诺全局 exactly-once。[R03](../03-execution-and-evidence/09-release-acceptance.md) 定义动作专属 gate/审批消费和工作区测量；[R14 验收](../06-acceptance-and-maintenance/06-security-acceptance.md) 定义提供方租约引用、绑定主体的交付、注册执行器边界和保留策略。

<!-- book-navigation -->

5.5 远程外部操作

[全书目录](../README.md) · [5. 共享执行](README.md) · [English](../../en/05-shared-execution/05-remote-effects.md) · [上一章: 5.4 共享产物](04-shared-artifacts.md) · [下一章: 5.6 集群调度](06-cluster-scheduling.md)

<!-- /book-navigation -->
