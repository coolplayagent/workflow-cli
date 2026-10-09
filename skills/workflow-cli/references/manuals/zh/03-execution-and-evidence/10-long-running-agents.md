# 长时间运行的 Agent 会话

0.2.0 将任务截止时间与所有权租约分开，增加可恢复的模型/工具会话、类型化循环反馈和显式 run 接续。本地与共享存储继续校验审批、制品、副作用和执行结果；模型不能直接改变流程控制状态。

## 升级与恢复

SQLite 存储和 PostgreSQL run image 使用 **schema 12**。停止旧写入者，保留旧可执行文件，按照现有存储升级与备份流程操作。本地 schema 1–11、共享 image 10–11 必须显式升级，普通打开不会自动迁移。旧版本不能打开 schema 12。共享服务与 worker 应一起升级，并在调度策略中更新允许的确切 worker 版本。

新 run 在 revision 1 及每 16 个 revision 保存状态检查点，包含快照、事件/Outbox 前缀摘要及前一检查点摘要。普通恢复校验记录身份、摘要链、回执与执行证据，再重放最新检查点之后最多 15 个事件；它仍需扫描保留历史，并非常数时间。`run verify` 独立完整重放一次并比较检查点和命令。升级保留旧日志检查点，在已验证的当前 revision 增加状态检查点。

```sh
workflow run storage-plan runs.db
workflow run migrate runs.db retained-before-v12.db
workflow run history-usage runs.db segment-1
workflow run verify runs.db segment-1
```

`history-usage` 显示事件数、frame 数、检查点字节数、配置上限、检查点 revision 和尾部重放数。事件、frame 或字节达到上限的 80% 时建议接续。原有上限不变。PostgreSQL 仍写入有 64 MiB 上限的完整事务 image；本次减少重放工作，不减少 image 写入量。应把流程设计成有界阶段。

## 长任务与调度

只读任务的截止时间由冻结的 capability timeout 决定；所有权租约可以更早到期，由执行器在任务运行时续租。续租保留 epoch，不改变任务的绝对截止时间。进度写入 execution history；旧持有者不能写入进度或结果。旧 prepared 记录仍按原来受租约限制的截止时间校验。

CLI 的 builtin、workspace 和模型任务在私有子进程内运行。主进程保持续租并检查取消。任务取消、截止时间耗尽或所有权丢失时，停止并回收子进程组。Linux 的父进程死亡信号会在驱动进程崩溃后终止直接活动子进程。这不会撤销外部副作用。自定义同步 Rust adapter 仍须合作遵守截止时间，库不能强行终止任意 Rust 代码。

本地 daemon 最多同时处理八个独立 run，每个 drive 处理一个命令。慢模型调用不会挡住其他 run 的定时器。`active_runs` 列出正在处理的 run；`active_run` 保留第一个。stop 停止新增工作并等待所有已接纳的 drive 完成。daemon 配置 `lease_ms` 为 100–300000 毫秒，默认 120000；应给数据库事务留足时间。共享调度器一起续期任务分配和容量占用，受原任务截止时间及 worker 凭据有效期限制。共享 CLI worker 在子进程执行期间检查分配权限并记录进度。副作用继续采用原来的调用截止时间与对账规则。

## 冻结重试策略与会话检查点

没有 `retry` 的策略保留原有规范化摘要与失败行为。新的不可变策略版本可声明：

```json
"retry": {
  "max_retries": 2,
  "initial_backoff_ms": 500,
  "max_backoff_ms": 30000
}
```

`max_retries` 必须大于零且小于 `budget.model_calls`；任务还需声明 `model_rate_limited` 和 `model_authentication` 两种 permanent 最终错误。每次已接纳调用都计入原预算，包括崩溃后结果未知的调用。退避与下次可重试时间由持久化观察确定，重启不会重置调用预算或原始截止时间。

开启重试后，HTTP 429 表示限流，408/5xx 和传输错误属于临时故障，401/403 属于认证错误；拒绝回答和无效输出不可重试。`Retry-After` 支持秒数和 HTTP 日期，下次调用同时遵守指数退避和服务方等待时间。如果等待超出策略上限或会话截止时间，会结束会话，不会提前重试。HTTP 客户端自身不会自动重试或跟随重定向。

每次模型/工具调用前先持久化接纳记录，返回后确认观察结果，再接纳下一次调用。检查点绑定确切输入、原请求、策略摘要和 provider binding；写入需要当前持有者以及上一检查点的准确摘要。取消流程后禁止新增模型/工具调用。重启复用已确认的工具结果；没有确认结果的模型调用记录为 `uncertain` 并计入预算，工具调用则记录明确的未知观察，不会补造结果。

恢复后生成的最终模型记录使用 schema 2，带有原始 `source_request`；旧 schema-1 记录仍可验证。离线记录验证不代表授权，提交结果还必须对应已保存的会话前缀。崩溃后的 provider 用量可能未知；本版本不承诺精确计费或原子的货币预算。可查看 `schema model-checkpoint`、`schema model-record` 和 `run execution-history`。

## 循环反馈与后继交接

循环可声明 `"feedback":{"diagnostic":"diagnostic"}`：键是下一轮输入字段，值是失败 body terminal 的必填输入字段。编译 bundle 时检查类型，失败迭代将真实 terminal 值带入下一轮。缺失或冲突的诊断会使循环失败。空反馈保持旧行为与定义摘要；最大轮数和循环绝对截止时间不变。

接续计划包含版本 1 的 handoff：源 run/digest/revision、目标、计划、已验证事实、制品引用、失败与剩余事项。每个事实指明 instance、输出字段和准确观察值；制品必须已经被源执行确认且仍能验证。后继流程必须声明类型化输入并传入完全相同的 handoff。

```sh
workflow schema run-handoff
workflow schema run-continuation
workflow run continue runs.db source-lease.json reviewed-continuation.json
workflow run continuation runs.db segment-1
```

接续要求源 run 在指定 revision 成功，Outbox 和副作用已结清，并持有有效租约。先在源执行历史中保留唯一后继计划，再通过第二次幂等提交启动后继 run。两次提交之间崩溃时可重试同一命令。不同后继计划会冲突；已存在但启动内容不同的目标 run 也会冲突。应保留记录的计划和两个 run。后继历史从 revision 1 开始。未决门禁、审批和未知副作用不能跨越这一成功阶段边界；这是显式分段，不是自动截断正在运行的流程。

## 复现故障路径

```sh
python3 examples/long-running/acceptance.py target/debug/workflow
python3 examples/long-running/history-benchmark.py target/release/workflow --output history.json
```

验收脚本发送真实 503，在工具结果确认后杀死 daemon，并取消阻塞活动；校验重试等待、已确认工具只执行一次、未知调用预算、子进程终止及离线恢复。它使用确定性的本地 provider，不是生产模型质量证据。Kernel 测试证明后续修复收到上一轮诊断；共享集成测试覆盖任务续期、检查点权限和 PostgreSQL 恢复。交付里程碑由 [`long-running-p1.json`](../../../../assets/examples/maintenance/long-running-p1.json) 记录。

<!-- book-navigation -->

3.10 长时间运行的 Agent 会话

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/10-long-running-agents.md) · [上一章: 3.9 受保护的交付](09-release-acceptance.md) · [下一章: 4.1 事件与人工决策](../04-effects-and-recovery/01-event-inbox.md)

<!-- /book-navigation -->
