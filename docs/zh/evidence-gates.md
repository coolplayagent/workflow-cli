# 证据检查与目标绑定

`workflow-gates` 是可注入 `EvidenceSource` 的可移植 checker，不依赖 SQLite、文件系统、worker SDK 或
内核。本地 CLI 通过恢复的 RunStore 执行记录和经过验证的产物实现端口，对准确的拟执行动作与目标返回
**PASS**、**FAIL** 或 **UNKNOWN**。独立 CLI 只产生只读观察。[强制后置条件](runtime-postconditions.md)
在受所有权约束的运行变更内消费该结果；[受保护交付](release-acceptance.md)增加外部操作边界授权。

## 检查真实工作

```sh
cargo build --locked --bin workflow
python3 examples/gates/check-validation.py "$PWD/target/debug/workflow" /tmp/new-gate-example
```

使用新的输出目录。示例调用真实校验能力，发布类型化报告，在持久化租约下提交结果，再执行证据检查。
待检查定义读取自固定 Git 提交。示例保留请求、策略、目标、产物引用与决策；改变目标 revision 会得到
UNKNOWN，旧 PASS 不能复用于新目标。示例不执行发布或其他外部操作。

```sh
workflow schema gate-request
workflow schema gate-decision
workflow gate evaluate <run-db> <artifact-store> <request.json>
workflow gate revalidate <run-db> <artifact-store> <request.json> <decision.json>
```

请求和旧决策是原始 JSON 对象，不是 CLI 包装响应；把求值响应的 `result` 保存为 `decision.json`。
只有 PASS 退出 **0**；FAIL、UNKNOWN、无效/不可用证据退出 **1**；用法与输入 I/O 错误退出 **2**。
`ok: true` 表示求值完成，可能是 FAIL 或 UNKNOWN，必须检查 `result.verdict`。运行或产物存储不可用/
损坏时返回 `ok: false` 和错误，不产生决策。这些命令不创建存储、不改 revision、不分发工作或签发 grant。

## 策略与目标

策略有准确标识和 1–64 个具名要求。每个要求固定以下内容：

- 流程节点 ID 与能力 ID/version。
- 完整能力契约摘要。
- 完整预期产物类型，包括含必需布尔字段的封闭 JSON 对象。
- 从**已接纳 worker 输出**检查的布尔字段。
- 1 毫秒至 30 天之间的最大年龄。

目标绑定运行 ID、不可变运行摘要、准确拟执行动作版本、仓库/完整 Git revision、共用输入摘要，以及
最多 128 个准确产物清单引用。本契约要求各 checker 输入摘要相同，尚未实现异构逐 checker 输入投影。
各报告必须把目标产物集合准确列为直接输入。空集合支持流程定义等内联输入检查。

为每个要求提供明确证据引用。缺失证据得到 UNKNOWN；重复或未知要求 ID 被拒绝。省略证据不能把强制
要求变为可选。删除策略要求会改变请求/策略摘要，使旧决策失效。策略权限属于宿主：CLI 不决定谁能新建
策略，也不授权 agent 为已批准运行采用更弱策略。

## 验证顺序与含义

Checker 通过 reader 检查报告清单身份、字节、类型和祖先。报告的源码 revision、生产运行、输入摘要与
直接产物依赖必须等于目标；每个目标产物也必须验证成功并属于同一运行。

单有报告不够。可信执行适配器必须找到**已提交** attempt，生产者、请求和输入身份准确匹配；运行摘要、
节点、能力及契约一致，已接纳 worker 结果明确引用该报告。本地读取恢复执行日志，验证请求/grant/结果、
epoch 隔离以及内核事件/回执。只发布未提交，或给未被引用的新报告复制生产者元数据，都不能证明执行。

结论来自已接纳 worker 输出。报告声称 `valid: true`，真实校验输出为 `valid: false` 时结果仍为 FAIL。
Checker 不解释自然语言摘要、任意载荷声明或 LLM 自称结论。类型化报告是保留附件，不是布尔结论来源。
成功调用缺少要求的布尔输出得到 UNKNOWN；已接纳的永久能力失败为 FAIL。输入无效、取消、暂时失败和
不确定效果为 UNKNOWN，因为它们不能证明质量检查已完成。

完成时间必须为正且不晚于提交时间，提交时间不晚于宿主求值时间。到期边界不包含等号：
`now >= completed_at + max_age_ms` 时为 UNKNOWN。未来、溢出或过期观察不能通过。任一已验证失败使总体
FAIL；否则任一 UNKNOWN 使总体 UNKNOWN；只有全部要求和目标产物都通过才是 PASS。

决策记录请求/策略/目标摘要、求值时间、最早过期时间、逐要求原因、证据引用、验证过的生产者与完成时间，
以及逐产物完整性发现。评审时保留完整请求，摘要不能替代策略/目标文档。纯 checker 不隐含批准。
运行时另行记录有权主体授予的例外，并把决策和人工审批纳入[最终验收清单](release-acceptance.md)。

## 重新验证与权限边界

`revalidate` 要求相同请求，先从真实证据重算旧 PASS（拒绝伪造字段），再按当前宿主时间求值。
动作、revision、输入、产物、策略或证据绑定变化需要新求值；过期观察不能再次通过。这是重算，不是
密码学签名，也不证明旧 CLI 实际运行过。本地 SQLite/文件所有权与宿主接入仍可信。

本地来源在有界分页读取前后检查运行 revision，若并发业务变更发生则拒绝本次读取，允许重试。
已接纳记录不可变，最多读 10000 条执行记录。纯 checker 还拒绝重复 JSON 键、未知字段、过大文档及
过深类型契约。

宿主必须独立观察当前工作区/目标并保护策略。清单中的仓库/revision 仍是宿主声明，元数据和摘要不能
证明 worker 确实检查了干净工作区。CLI 不认证远程 worker，不把模型 JSON 变成可信宿主事实，也不能
阻止调用者把旧目标冒充当前目标。共享认证服务先强制 worker grant 与产物来源，再提供相同 `EvidenceSource`。

PASS 本身不授予外部执行权限。受保护运行时/外部操作契约在分发前重新验证，并要求适配器执行时观察
和比较准确目标。服务做不到原子比较时，必须明确剩余竞争窗口与核对路径。读取后证据可能不可用，
可变外部目标也可能改变；单独的证据检查不消除这一竞争。

## 验收证据

测试覆盖真实编译器输出与账本提交、伪造报告声明、缺失/未提交/替换引用、损坏载荷、准确 revision/
输入/工具/类型/来源、严格布尔输出、过期/未来报告、动作/策略变化、决策篡改及 schema 一致性。
内存端口、本地适配器和经过认证的共享 reducer 使用同一求值器。受保护发布 CLI 矩阵还使用真实 TLS、
PostgreSQL、worker 凭据和作用域证据上传。

任务/终点强制门禁及有界修复通过[运行时集成](runtime-postconditions.md)使用此 checker。
[受保护交付契约](release-acceptance.md)记录动作授权、当前工作区验证、独立审批/例外权限、最终清单
以及本地/共享验收矩阵。R06 提供其中经过认证的人工响应与持久化等待。

<!-- book-navigation -->

[目录](README.md) · [English](../evidence-gates.md) · [上一章: 尝试级工作区](workspaces.md) · [下一章: 运行时后置条件](runtime-postconditions.md)

<!-- /book-navigation -->
