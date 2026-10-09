# 确定性工作流内核

`workflow-kernel` 是 Rust 库，有明确的 Bazel `rust_library`，负责合法流程变更。`start` 与 `apply`
计算新状态及有序命令意图，不使用文件、网络、系统时钟或提供方 SDK。CLI 用同一 reducer 检查 bundle
并执行可复现模拟。

## 体验场景

```sh
cargo run --locked -- kernel replay examples/kernel/review-approved.json
cargo run --locked -- kernel replay examples/kernel/review-rejected.json
cargo run --locked -- kernel replay examples/kernel/parallel-all.json
cargo run --locked -- kernel replay examples/kernel/parallel-any-cancel.json
cargo run --locked -- kernel replay examples/kernel/repair-third-round.json
```

五个场景提供的都是**模拟契约和模拟任务结果**，不会调用编码/测试适配器。批准场景成功，拒绝场景
取消；parallel all 等待两个结果；parallel any 等待未获选分支取消后的核对；repair 前两轮失败，第三轮
成功。变化后的 parallel/repair 定义使用版本 `2.0.0`。

Agent 操作循环见[回放参考](../../../skills/workflow-cli/references/replay.md)。`workflow schema kernel-bundle`、
`kernel-event`、`kernel-scenario` 和 `kernel-checkpoint` 导出输入格式。CLI 求值成功退出 0，即使
`snapshot.status` 是 `failed` 或 `cancelled`；拒绝输入/变更退出 1，用法与 I/O 错误退出 2。

逐步操作时，从场景提取 `bundle`，把副本的 `events` 改为空数组后回放，将返回的 `checkpoint` 对象
保存到独立文件。`kernel restore <bundle.json> <checkpoint.json>` 验证并回放，不返回历史命令。
`kernel apply <bundle.json> <checkpoint.json> <event.json>` 返回下一快照、变更和检查点。事件需要
快照中真实的 `run_id`、`run_digest`、`revision`，以及新的稳定事件 ID 和目标时间戳。始终输出到新路径，
重定向到输入文件会在 CLI 读取前截断它。CLI 不保存运行，也不分发计算出的命令。

## Bundle 编译与身份

Bundle 包含根引用、流程定义与能力描述符。编译检查所有定义、准确 ID/version 引用、任务契约、子流程
输入/返回契约和外部操作的查询/补偿引用。重复发布、缺失绑定和递归子流程/循环引用被拒绝。
模型策略任务从 bundle 解析冻结策略及准确任务/工具契约，直接调用不能绕过策略。提供方执行在 reducer
之外，见[模型执行](../03-execution-and-evidence/04-model-execution.md)。

一个流程的成功终点必须声明相同的**输入**契约，其解析后的输入就是流程返回值。子流程或循环节点的
输入等于子流程输入，输出等于返回契约。Task/wait 接收类型化结果；其他控制节点输出契约为空。
只有成功的生产者能供给节点输出绑定；真实值缺失使消费节点失败，包括尚未获得结果的 any 未获选分支。

流程/节点顺序规范化，边顺序仍有意义。Bundle 身份包括所有提供的流程和描述符。Frame 记录准确的定义
摘要，任务命令携带准确能力版本和契约摘要。`run_digest` 绑定 bundle、运行 ID、初始输入、起始时间与
限制，拒绝不同运行种子间意外投递事件。宿主仍需保证运行 ID 唯一、检查适配器可用性；提供契约和摘要
不等于授权或适配器真实性证明。

## 控制流

| 结构 | 变更语义 |
| --- | --- |
| Sequence/task | 解析类型化输入和前置条件，产生 `execute_task`；接纳类型化成功或声明的失败代码。条件为假则跳过，表达式错误则失败 |
| Decision | 共用求值器按边顺序求值；exclusive 多重匹配或缺失值比较失败；first-match 选择首个真值，否则默认出口；未选边跳过 |
| Fork | 选择所有出边，按稳定 frame/节点顺序激活 |
| All join | 等待全部 token；失败优先于取消，两者都阻止成功；其他分支成功时跳过为中性，全部跳过则跳过 |
| Any join | 按单调提交序号选首个成功 token；接纳事件顺序确定外部结果竞争，边顺序和规范遍历处理内部同序情况；无成功分支则不能成功 |
| Wait | 产生带一个绝对逻辑 deadline 的 `await_signal`；匹配接受信号提供类型化输出，拒绝要求空输出；只消费一次，走接受/拒绝/超时路由 |
| Subworkflow | 用类型化输入和新实例创建 frame，传播完成后的子状态与返回值 |
| Loop | 只捕获一次输入，固定循环体与全循环 deadline，每轮新建 frame/实例；成功退出 completed；失败在次数限制内重试，耗尽后等待剩余子工作结束再走 exhausted |
| Terminal | 记录声明结果；frame 等待包括未获选分支的所有节点。已激活失败终点优先于取消和成功；成功返回值冲突则失败；所有终点均跳过时失败 |

`await` 模式的 any join 可以在其他分支仍执行时放行后继，但 frame 保留并等待它们的结果。
未获选分支失败不会推翻已经选定的成功，除非该分支到达独立激活的失败终点。

`cancel_and_reconcile` 要求封闭 fork 区域：分支互不相交，每支一条 join 入边，无共享内部节点、外来
入口或逃逸的终点路径。含歧义区域不能编译。获选分支取消其他组；已签发任务收到 `cancel_task`，在得到
明确结果前保持未决。`uncertain` 或声明为 `unknown_effect` 的错误产生 `reconcile_task`，需要明确的
`task_reconciled`。取消后迟到的成功仍记录，但不能推进已取消分支。取消不会撤销外部写入。

全局取消停止待执行工作，取消定时器和子节点，等待已签发任务/核对结束后运行才变为 cancelled。
若兄弟节点立即获选，同一变更可能先产生 execute 再产生 cancel；宿主必须保留该顺序并核对是否已分发。

## 事件、时间与回放

事件是可信宿主事实，携带稳定 ID、运行绑定、expected revision 和明确的单调时间戳。宿主构造事件前
必须验证 worker 结果、权限、审批和产物。内存引擎不提供并发存储事务或不可信 worker 接入。

完全相同的重复事件返回 `duplicate: true` 且不产生命令，终态后也是如此。相同 ID 携带不同内容是冲突；
新事件使用过期 revision 失败；新事件不能重开终态运行。任何被拒绝的变更都保持状态、日志和命令不变。

普通事件先处理时间戳及之前到期的 deadline。恰好在循环 deadline 到来的任务结果不能使循环成功。
过期信号被原子拒绝；若需持久化超时变更，应另发 `advance_time`。显式有序全局 `cancel` 在其事件内
优先于定时器过期。内核不轮询墙上时钟。

检查点保存运行种子、接纳事件日志、bundle 摘要、回放状态摘要和完整内容校验和。恢复从种子重建并
静默回放，然后验证状态摘要；保留节点实例、获选分支、deadline、取消和去重。校验和能发现损坏，不能
认证有权改写检查点并重算摘要的主体。本版本没有检查点迁移或任意快照注入 API。

## 上限与宿主集成

严格 JSON 解析拒绝重复键、未知字段和超过 2 MiB 的输入。Bundle 最多 128 个流程、256 个描述符、
合计 4096 个节点和 16384 条边；各流程还需满足自身大小/深度限制。默认运行上限依次为 256 frame、
16384 节点实例、100000 微变更、4096 事件；可配置最高值依次为 4096、100000、1000000、10000。
历史 frame 保留。快照与检查点各最多 2 MiB，CLI 合计响应也不超过 2 MiB；历史过大明确失败。
全局取消及收尾可突破普通事件/变更预算，但仍遵守序列化硬上限。

持久化宿主必须原子提交接纳事件、新 revision/检查点与有序命令 outbox，再按稳定身份
`(run_id, revision, command_index)` 分发；还需提供有效租约、所有权隔离、认证事件接入和可恢复定时器。
回放不能分发历史命令。[SQLite RunStore](../03-execution-and-evidence/01-run-store.md)实现原子状态/事件/outbox 和验证恢复；
[本地执行器](../03-execution-and-evidence/02-local-execution.md)增加租约、受所有权约束的 attempt 和有界只读重试。模型适配器产生由
持久化宿主检查的明确记录。[持久化外部操作](../04-effects-and-recovery/02-durable-effects.md)与[有序补偿](../04-effects-and-recovery/03-ordered-compensation.md)
使用独立宿主适配器。内核回放仍是纯计算，不声称外部效果恰好一次、业务收益或恢复 SLA。

<!-- book-navigation -->

2.4 控制流与回放

[全书目录](../README.md) · [2. 定义流程](README.md) · [English](../../en/02-process-definition/04-kernel-semantics.md) · [上一章: 2.3 能力与 worker 协议](03-worker-protocol.md) · [下一章: 2.5 可评审的 SOP 模板](05-reviewed-templates.md)

<!-- /book-navigation -->
