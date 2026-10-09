# 持久化事件 Inbox

Inbox 在等待激活前或等待期间保留可信宿主回调。收到消息与应用决策是两个事实；提交回执可以是
`pending`、`applied` 或 `rejected`，只有 applied 推进等待。可移植内核负责匹配与回放，`InboxStore`
负责事务接入/查询契约，SQLite 原子提交回执状态、事件、检查点和新 outbox。等待期间不需要保持 worker
或模型 session 打开。

## 命令与绑定

```sh
workflow run waits runs.db run-id 0 100
workflow schema run-signal
workflow run receive runs.db verified-signal.json
workflow run inbox runs.db run-id 0 100
workflow run history runs.db run-id 0 100
workflow run verify runs.db run-id
```

`waits` 返回活动等待、原 deadline 和运行是否暂停。应从真实等待登记复制 target 和 correlation ID。
Target 绑定节点实例、不可变定义摘要、事件名及真实类型化输入摘要。Correlation 对不可变运行摘要与
target 求摘要；克隆运行或改变输入不能复用。Bundle 为每个经过认证的等待冻结 `wait_policies`，必需
类型化输入标识评审对象：SHA-256 内容摘要或准确产物 ID/digest 对象。`waits` 返回对象、响应者允许列表、
有效期、不可变策略版本及三条路由目标。人工审批至少一个对象；接纳事务及恢复时都验证产物对象字节与
运行所有权。任意输入变化需新 target/correlation，新循环或返工等待使用新实例。

提交包含运行 ID/digest 和 schema-1 消息：稳定提供方 message ID、复制的 target/correlation、宿主 source
标签、`approve`/`reject`/`request_changes`、非空 reason、类型化 outputs 和绝对 expiry。Reason 最多
1024 字节。Reject/request_changes 要求空 outputs，都走声明的 `rejected` 边，但保留不同决策和理由。
该边应声明为目标返工或终止路径；approve 校验输出后走 accepted。

`receive` 在写事务内采样可信宿主时钟，调用者不指定运行 revision 或回执时间。提交前再次采样，检查
消息 expiry 与等待 deadline 的较小值。时间回退或刚过期使整个事务回滚，重试可持久化过期回执。
受所有权约束的 task/gate/timer 在激活早到回调时同样检查。原始管理 `run event`/pause/resume 仍是可信
逻辑时间观察，不是认证外部入口。回复丢失后准确重试，返回当前回执及 `duplicate: true`，不再次应用，
即使已经过期或运行终止；相同 ID 改任何字段都冲突。退出 0 表示回执持久化，不代表批准，应检查
`result.entry.status.status` 和拒绝理由。Inbox 分页用上次接收 revision，waits 用上次实例 ID，limit
1–100；通过 `next_cursor` 读全历史。

## 顺序、早到与恢复

早到回调可指向已分配、尚待激活的等待实例，并携带准确预期输入摘要；激活提供真实输入前保留 pending。
未分配的未来循环/子流程实例产生保留的 `unknown_instance` 拒绝回执，宿主应先取得新身份。多条消息按
提交接收顺序竞争，不按 message ID 排序。首个合格决策结束等待，后续保留 `already_settled`。
被拒绝或格式错误消息不能授予路由。

暂停时合格回调保持 pending，继续后重查实际输入、消息 expiry 和原等待 deadline；过期不能授权工作。
相同逻辑时间先处理定时器过期，再处理回调。取消提交后优先，后续回执为 `run_cancelled`。审批先提交时，
使用旧 revision 的取消被拒绝，需要按当前状态重考虑。审批应用后的取消不改写历史审批。

Pending expiry 在下一次提交事件或 timer drive 被观察。本地 daemon 与远程 scheduler 驱动这些处理；
两者都不运行时，需要宿主继续执行，查询不会暗中推进逻辑时间。匹配与恢复不采样模型。检查点加尾部
与全事件回放分别重建 Inbox；编辑或日志缺失导致与存储头不一致。原子提交保证崩溃后看到完整回调变更
或完全没发生。每运行最多 256 条消息，还受既有事件/消息/检查点大小限制。容量拒绝不算持久化接纳，
不自动删除回执和去重身份。

## 宿主边界与兼容性

`source` 是宿主声明，不是认证证明。本地 CLI 信任有数据库权限的管理员。认证 HTTPS API 写入凭据 actor、
检查 tenant/project 与角色，并要求匹配冻结 wait 策略。`approve` 只接受 approver 凭据和人工审批策略，
`signal` 只接受 signal_source 凭据和外部事件策略。Worker 与模型输出不能取得这些权限，原始内核
`Signal` 不能绕过受保护等待的 Inbox。

可选 exception 策略拥有独立不可变版本、响应者列表和允许代码。例外是带
`exception: {policy: {id, version}, code}` 的 approve 决策，reason、actor、claim 保留在 Inbox 和日志。
不能覆盖输入身份、expiry 或产物完整性；普通审批不自动获得例外权限。

外部 webhook 网关必须认证上游、持久化稳定 provider event ID，并以相同 signal 重试直到得到持久化
回执。HTTPS 服务停止时不接收消息，重启后通过 API 重试提供与本地 receive 相同的 Inbox 契约。
上游签名验证和持久化队列属于网关。完整策略、迁移、本地/HTTPS 示例和故障证据见
[R06 验收](approval-acceptance.md)。

当前运行存储为 schema 11，旧版按[版本迁移](version-migration.md)使用
`run [--artifacts store] migrate db new-backup-file`。既有种子、事件和空 Inbox 快照保留准确摘要。
确认丢失时使用原提交重试，正常恢复不调用 webhook 或模型。

测试覆盖激活前/检查点恢复后早到回调、暂停过期、输入/定义/事件/输出不匹配、重复身份改内容、终态/
迟到投递、独立进程重复与取消竞争、五个 writer 终止阶段、提交前过期/时钟回退、任务提交期间回调
过期、Inbox 头损坏及 CLI 回执/查询行为。

<!-- book-navigation -->

[目录](README.md) · [English](../event-inbox.md) · [上一章: 受保护的交付](release-acceptance.md) · [下一章: 持久化外部操作](durable-effects.md)

<!-- /book-navigation -->
