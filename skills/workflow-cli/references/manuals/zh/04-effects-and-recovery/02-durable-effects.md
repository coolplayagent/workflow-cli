# 持久化外部写操作

`workflow-effects` 定义确定性操作账本与宿主适配器端口；`workflow-runstore-sqlite` 把操作记录在运行
共用、受所有权约束的执行日志；`workflow-effect-http` 调用明确配置的 JSON 网关。普通 Worker 调用仍
只接纳只读能力。

## 冻结契约与标识

写任务通过 `BundleSpec.effect_bindings` 显式启用，指定准确流程版本、节点 ID 和 `EffectPolicy`。
策略冻结自身版本、逻辑目标、服务主体绑定及重试预算，版本在 store 内锁定不可变内容。空绑定列表不
序列化，使旧 bundle 摘要不变。绑定要求直接写任务，声明的 query 引用必须解析为 bundle 中只读能力。

主 operation key 对不可变运行摘要、具体节点实例和 primary slot 求摘要；attempt ID 与 epoch 独立。
重试复用 key，不同循环轮次/运行有不同实例/运行身份。持久化意图绑定命令摘要、准确描述符、输入及其
摘要、目标、调用身份和首次接纳时间；prepared 记录还保留准确请求摘要。它们是完整性绑定，不是签名
或认证凭据。

## 事务边界

1. 取得有效运行租约，在事务下选择首条 pending 命令。
2. 调用任何适配器前提交意图与 prepared call。
3. 在运行数据库事务外调用提供方。
4. 在当前租约下校验观察，将操作记录、任务事件、后继命令和回执一起提交。

恢复只回放日志，不调用提供方；复查冻结意图、请求摘要、租约、历史 pause/节点接纳状态，以及受管理
任务事件、回执和证明的一一对应。原始任务结果或手工回执不能绕过受管理操作。相同观察/手工解决重试
返回已提交结果，改内容冲突。

租约隔离保护权威状态提交，网关/提供方必须自行实现 key 唯一与保留。暂停或过期 worker 仍可能影响
不执行隔离的远端系统，因此不保证全局恰好一次。取消流程不删除已创建资源，也不自动补偿。

## 未知结果、重试与取消

| 观察 | 接纳决策 |
| --- | --- |
| 首次合格任务 | 持久化意图，再发一次写 |
| 原所有者消失或写响应未知 | 声明 lookup 时先查询；没有 lookup 时只在声明的幂等保证内重试，否则停在 uncertain |
| 查询找到效果 | 校验原意图/目标/输出回执并提交 |
| 查询暂未找到 | 只有目标端幂等时才重试；否则“没找到”不能隔离旧 writer，必须手工核对 |
| 输入、权限、业务或永久拒绝 | 提交声明的确定未产生效果错误，不重试；若早期写仍未知，新拒绝不能消除它 |
| 确定未产生效果的暂时拒绝 | 仅在幂等、有界次数和原写窗口内重试 |
| 查询失败或未知 | 有界查询重试，耗尽后保持 uncertain |
| 首个意图前取消 | 不写入，确认取消 |
| 写接纳后取消 | 可查询，保留真实回执并完成取消，不接纳新写 |
| 暂停 | 停止新写/查询接纳，已 prepared 结果仍可提交 |

重试策略约束包含查询在内的**全部调用**，次数 1–32。指数退避带确定性 50–100% 抖动，以
`max_backoff_ms` 封顶；退避和次数跨重启/换所有者保留。写接纳 deadline 为 `created_at + total_write_ms`
与幂等保留窗口中较早者。写 deadline 后仍可在剩余次数内查询恢复。提供方的“无效果”必须权威，不能把
超时、不明确 HTTP 状态或网络错误当作无效果。

调用 deadline 后到达的真实回执仍可在有效续租下提交，否则会丢失真实效果证据。提交前第二次宿主时钟
检查仍拒绝租约过期或时钟回退；旧所有者不能提交，新所有者通过查询取得回执。新写还受最早等待定时器
约束，定时器到期需先推进。宿主在请求 deadline 后应停止发起 I/O；提供方需说明幂等窗口如何覆盖延迟请求。

Driver 返回 `effect_backoff` 或 `effect_uncertain`，不 sleep 或启动 daemon。通过 `run effects` 检查
下一可接纳时间，再明确调用。重启和手工改数据库都不能重置预算。

## CLI 与网关协议

`examples/runs/effect-release.json` 是类型化测试发布流程，使用历史逻辑时间。真实运行应指定自己的 ID
与起始时间，示例不提供审批或证明发布已发生。

```sh
workflow run init runs.db
workflow run start runs.db start.json
workflow run drive-effects runs.db my-run local-owner 20 effect-bindings.json
workflow run effects runs.db my-run 0 100
workflow run execution-history runs.db my-run 0 100
workflow run verify runs.db my-run
```

`effect-bindings.json` 是 `HttpEffectBinding` 数组，用 `workflow schema run-effect-http-binding` 导出契约。
组合有界模型和外部操作时，在 drive-effects 的操作绑定文件后添加可选模型绑定文件，共用冻结 bundle：

```json
[{
  "schema_version": 1,
  "target": {"id": "sandbox-releases", "version": "1.0.0"},
  "call_identity": {"id": "sandbox-publisher", "version": "1.0.0"},
  "capability": "REPLACE WITH THE EXACT WRITE DESCRIPTOR FROM THE BUNDLE",
  "endpoint": "https://your-authorized-gateway.example/operations",
  "api_key_env": "WORKFLOW_EFFECT_TOKEN",
  "allow_loopback_http": false
}]
```

占位符必须替换为描述符对象，不是字符串。宿主只路由准确的 descriptor/target/principal 匹配。
凭据由指定环境变量解析，或宿主 secret resolver 通过 `execute_with_secret` 提供；秘密不进入账本或
适配器错误消息。禁用 HTTP 重定向、隐式重试和代理发现；除明确允许的字面 loopback IP 外必须 HTTPS。
响应受 worker 消息大小和请求时间预算约束。

网关收到 `POST <endpoint>/write` 或 `/query`，内容为序列化 `EffectAttempt`，携带 bearer 授权、
`Idempotency-Key`、`X-Effect-Intent` 和 `X-Effect-Request`。Query 虽用 POST，语义仍是只读提供方查询。
能力的 query 引用命名操作协议 lookup，不是按描述符 task 端口直接调用 Worker。网关必须依据自身权限
验证主体、目标与能力，传入 identity 字符串不认证它们。

成功 HTTP 响应含 `EffectReply`：准确 attempt 的请求摘要与 `Observation`。Applied 回执绑定原 operation
key、意图摘要、目标、提供方资源/回执 ID 和类型化输出。缓存的 provider receipt 可跨 attempt 使用，
但外层回复必须指向当前请求。非成功状态、格式错误或不匹配回复都变成 unknown；HTTP 状态本身不能
证明无效果。`NotApplied` 必须使用冻结写能力声明的代码/类别。

通过 `workflow schema run-effect-attempt`、`run-effect-reply`、`run-effect-observation`、
`run-effect-resolution` 导出契约，和其他 run schema 一样，JSON Schema 位于正常 `ok/result` 响应中的 result。
底层组合用 `run effect-claim <db> <lease.json>`、
`effect-observe <db> <lease.json> <attempt-id> <observation.json>` 及
`effect-resolve <db> <lease.json> <operation-key> <resolution.json>`，都是可信本地管理接口，不是远程认证 API。

## 手工核对

不确定效果阻塞核对命令。手工解决保留稳定 resolution ID、actor 注释、reason 和 evidence 引用。
确认已应用要使用真实提供方回执；`confirmed_not_applied` 只能在检查目标且静默所有未决 writer 后提交。
确认未应用使任务 cancelled，不授权隐藏重试。重新开始业务需要明确流程决策/新运行，不能伪造回执、
审批或认证 actor。

## 存储与验证边界

当前 schema 12 保护操作日志、补偿、恢复导入和迁移语义。[显式迁移](05-version-migration.md)要求验证
备份，升级 schema 1–11 并复查保留运行；普通打开拒绝不同版本。

测试使用真实 loopback HTTP 网关及独立持久化提供方 SQLite 数据库，在 provider 已提交但运行回执未提交
时杀进程，再以新租约查询；并发重复网关投递保留一个发布。还覆盖双进程领取、十个意图/回执事务终止
阶段、循环实例隔离、预算/退避、旧 writer 不确定性、暂停/取消竞争、过期提交、迟到真实回执及日志证明
缺失。真实 v7→v8 迁移保留暂停 Inbox，旧 reader 拒绝 v8。它们是有界测试，不是生产可靠性测量。

逆序依赖、原回执绑定、不可逆操作及失败后手工接管见[有序补偿](03-ordered-compensation.md)。
[认证远程操作](../05-shared-execution/05-remote-effects.md)连接共享 PostgreSQL 账本、HTTPS scheduler/worker 与审计恢复。
动作专属审批和当前工作区验证见[受保护交付](../03-execution-and-evidence/09-release-acceptance.md)；外部操作绑定的远程产物及来源到
目标的所有权仍有独立宿主/传输边界。[本地恢复](04-backup-recovery.md)保留已知意图，并在持久化恢复屏障下
审计导入真实的备份后 provider 回执。远程操作章节映射完整 R05 验收证据。

<!-- book-navigation -->

4.2 持久化外部操作

[全书目录](../README.md) · [4. 外部操作与恢复](README.md) · [English](../../en/04-effects-and-recovery/02-durable-effects.md) · [上一章: 4.1 事件与人工决策](01-event-inbox.md) · [下一章: 4.3 有序补偿](03-ordered-compensation.md)

<!-- /book-navigation -->
