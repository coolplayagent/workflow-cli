# 持久审批与事件验收（R06）

R06 在本地 SQLite 和经过认证的 PostgreSQL authority 中使用相同的确定性 wait/Inbox reducer。已提交 bundle 包含等待类型、订阅事件、必需输入、响应者、策略版本、有效期上限及接受/拒绝/超时路由。等待不会调用模型，也不创建 worker assignment。主机可退出，之后恢复同一个 instance。

## 策略与身份

`examples/approval/protected-start.json` 使用已有离线编译器工作流，对提供的 `review_digest` 进行人工审核。应用必须根据实际被审核内容计算摘要；示例使用确定性 fixture 摘要。`wait_policies` 将固定版本策略绑定到 workflow 版本和节点。人工策略至少要求一个精确的必需输入，类型为 `digest` 或 `artifact`。产物输入是单个 `{artifact_id, digest}` 链接，且必须属于本 run。摘要审核对象绑定内容身份，但不表示此服务保存了相应字节。

策略允许指定的认证 actor，限制自持久接收起的响应有效期，并可定义独立版本的例外策略。响应者列表及允许的例外代码均显式声明。例外响应保留代码、策略、actor 和原因，仍要求当前审核对象、精确 correlation 和未过期截止时间。两个存储都不允许在已有身份/版本下改变策略内容。

本地 `workflow run receive runs.db response.json` 是可信管理接口。应保护数据库，不要将模型文本直接送入该接口。远程边界要求 TLS 和不透明的作用域凭证。`approve` 接受 `approver`；`signal` 接受 `signal_source`。二者都用认证 actor 替换 payload `source`，并要求匹配的绑定 wait 类型。随后 kernel 执行冻结的 actor/subject/expiry 规则。未授权响应者得到保留的拒绝回执；错误端点角色会被拒绝并审计，不改变 run。原始 `Signal` 事件不能结算受保护 wait。

已有 access schema 1 部署需显式执行：

```sh
workflow service migrate-access server-binding.json
```

迁移具备事务性且可重复。新部署创建 access schema 2；启用集群调度后使用 schema 3。无策略的旧 bundle 仍可读取，并由可信主机在本地操作，但认证响应要求受保护 bundle。已发布版本和运行中定义不会被重写。

## 本地与远程操作

```sh
cargo build -p workflow-cli --locked
python3 examples/approval/local-demo.py target/debug/workflow
```

可执行示例写入当前启动时间、推进工作流、检查新会话在等待期间不执行工作、提交三种决定，并验证重复回执及完整回放。自己的 run 可用 `workflow schema run-signal` 构造 `response.json`。从真实查询复制 run digest 和 target/correlation。提供唯一提供方 message ID、`approve`/`reject`/`request_changes`、非空原因、匹配 wait 契约的 outputs 及绝对到期时间。完全相同响应重试返回当前回执及 `duplicate: true`；同 ID 改变内容会冲突。证明消费的是已应用回执，而非单独的退出状态。本地 source 必须是允许的主机证明 actor，例如 `reviewer`。

远程客户端在操作协议中使用相同提交：

```json
{
  "protocol_version": 1,
  "request_id": "review-delivery-1",
  "operation": {
    "type": "approve",
    "request": {
      "schema_version": 1,
      "run_id": "approval-demo",
      "run_digest": "COPY_FROM_RUN",
      "message": "REPLACE_WITH_RUN_SIGNAL_MESSAGE"
    }
  }
}
```

占位符只是描述 envelope；执行 `workflow remote call client-binding.json request.json` 前，先按 schema 生成合法 message。CI/webhook gateway 使用 `type: signal`、`signal_source` 凭证和 `external_event` wait 策略。webhook gateway 认证上游签名并维护持久重试队列。服务停机时消息留在 gateway，或通过提供方查询恢复；停止的进程无法接收回调。一旦服务确认回执，共享 Inbox 就负责保存它。

reject/request-changes 走定义的 `rejected` 边，审计仍区分两种决定。返工可以定义修订任务加新 wait，或有界子流程循环。新 input digest 和 instance 身份使旧决定失效。`timed_out` 可转向升级处理、另一个 wait 或终态。暂停/恢复保留原截止时间。取消和 timer/response 事务串行化；同一观察时间先归约 timeout 再处理响应。回执准入在提交时重新检查到期时间。

## 验收矩阵

| R06 标准 | 可执行证据 |
| --- | --- |
| 会话/服务重启后保留同一 wait | SQLite `paused_inbox_persists_across_sessions_and_cancelled_or_expired_receipts_never_advance`；真实 HTTPS `tls_wait_survives_service_restart_and_resumes_only_from_the_authorized_current_response` 终止服务、创建新会话、比较 instance/deadline/subject，恢复人工和 CI wait |
| 提前、重复、乱序及迟到事件 | Kernel Inbox 套件及 PostgreSQL `early_events_survive_restart_and_lost_acknowledgements_in_receive_order`；即使 message ID 排序不同也由首个回执胜出；精确重试保留原回执 |
| 未授权 actor、错误对象或过期审批不能放行 | Kernel `protected_wait_rejects_raw_signals_and_wrong_actor_subject_validity_or_exception`；PostgreSQL actor/channel/expiry 测试；SQLite 产物损坏回滚/恢复；HTTPS 错误角色测试 |
| 拒绝/修改请求进入返工；旧决定不能复用 | Kernel `rejection_and_request_changes_rework_to_a_new_subject_and_reject_old_approval` 测试两种决定、修订任务和新 wait，再拒绝旧 instance/input digest |
| 超时、取消和响应只允许一次合法转换 | PostgreSQL `timeout_cancel_and_response_serialize_to_one_transition_after_scheduler_restart`；SQLite 独立进程取消/重复竞态、终止写入者和提交到期回滚 |
| 等待期间无模型成本、不占持久 worker 槽；完整审计 | HTTPS 等待测试观察无 pending worker assignment；PostgreSQL 提前事件测试结算/释放 assignment；已有 runtime wait/lease 测试；Inbox 保留 response/actor/reason/decision，访问审计保留 endpoint/credential/outcome |

运行本地与共享套件：

```sh
cargo test -p workflow-kernel -p workflow-runstore-sqlite --locked
cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture --test-threads=1
cargo test -p workflow-service --locked -- --ignored --nocapture --test-threads=1
cargo test --workspace --locked
bazel test //...
```

ignored 套件要求 `WORKFLOW_TEST_POSTGRES` 指向可丢弃数据库，由必跑 PostgreSQL CI 任务提供。每个竞争事务使用独立连接，HTTPS 重启测试使用真实操作系统进程。这些是正确性 fixture，不是生产吞吐或人工响应时间基准，也不能从 fixture 计时推断提供方账单减少。

<!-- book-navigation -->

6.3 审批验收

[全书目录](../README.md) · [6. 验收与维护](README.md) · [English](../../en/06-acceptance-and-maintenance/03-approval-acceptance.md) · [上一章: 6.2 模型边界验收](02-model-boundaries-acceptance.md) · [下一章: 6.4 产物与工作区验收](04-artifact-acceptance.md)

<!-- /book-navigation -->
