# 当前证据与受保护交付（R03）

节点完成、运行完成和对外发布是三个独立的决定。`postconditions` 约束前两个边界，带有 `release` 的 effect 绑定保护第三个边界。旧式写入绑定仍可执行通用外部操作，但不代表交付已经通过质量门禁。发布、部署以及任何要求当前质量证据的操作，都应使用受保护的交付契约。

必需的检查器必须是直接执行的只读任务，不能配置模型策略。即使输出契约声明了类型正确的布尔结果，bundle 编译仍会拒绝模型驱动或具有写入能力的检查器。模型生成的结构化输出不能证明自身质量；证据必须由独立注册的检查器执行后产生。

## 冻结发布契约

`examples/gates/protected-release.json` 是完整且经过验证的示例。写入任务必须提供 `delivery` 输入，其中包含：

```json
{
  "source_revision": {"repository": "repository-id", "revision": "full-git-object-id"},
  "input_digest": "sha256:checked-task-input-digest",
  "artifacts": [{"artifact_id": "exact-artifact-id", "digest": "sha256:artifact-digest"}]
}
```

`EffectBinding.release` 指定 1–16 个前置 gate 节点、上述输入字段、0–16 项普通审批要求，以及目标比较契约。通向写入的每条控制流路径都必须先经过所有 gate。其 `action` 必须等于实际写入能力的 ID/版本。所有 gate 的审核对象必须与写入输入完全一致：仓库和 revision、被检查输入的摘要，以及有序的产物链接。规范化审批对象会对产物链接排序；执行对象保留声明顺序，拒绝改变后的请求。

authority 从该 frame 已验证的历史中冻结实际 gate 上下文和已应用审批。在**每次写入之前**（包括重试），它读取已结算的检查结果，重新验证产物字节、生产者身份、能力契约/工具版本以及当前证据年龄。过去保存的 PASS 不能单独授权此次调用。生成的 `ReleaseAuthorization` 包含新评估和最早的证据/审批到期时间。effect attempt 的截止时间会缩短到该到期时间，并在提交前再次检查。

本地运行时在调用适配器前重新检查已提交的调用。共享认证交付在有作用域的 PostgreSQL 事务内做相同检查，然后才把一次性 assignment 标记为已交付。过期租约、变化的 epoch、暂停、取消、恢复屏障和过期证据都不能放行写入。恢复时，根据原始执行前缀和记录的时间重新计算每项授权，不信任调用方计算的哈希。查询不携带发布授权，因此质量证据或审批到期后，仍可用来对账之前的写入。

从启动时就包含受保护发布契约或质量门禁人工审批流的 run，以及迁移进入这些流程的 run，之后都不能再次迁移定义。改变流程图或削弱策略必须创建具有不同 run digest 的新 run，并重新取得证据与审批。这项保守限制保留原始批准的交付范围。旧式 R11 迁移继续采用已评审的重启行为。

## 独立授权的例外

postcondition 可指定 `exception: {node_id, subject_field}`。引用的前置人工等待节点必须声明摘要审核对象，以及带版本的例外策略；策略有自己的允许响应者和例外代码。R06 入口认证共享 Approver 身份；reducer 独立检查冻结的响应者列表、例外策略/代码、精确的 run/instance/input 关联、原因和到期时间。本地入口仍是可信主机操作。模型文本和 actor 字符串不能认证共享响应者。

`workflow gate review-digest <gate-request.json>` 根据完整质量策略、动作、仓库/revision、被检查输入摘要和产物计算审核范围。为避免启动输入自引用，故意不把 run ID/digest 纳入此摘要；经过认证的 wait correlation 会另外绑定实际 run 和 instance。不同循环 frame、输入、策略或动作需要各自有效的审批。普通审批不能代替例外，例外也不能满足必需的普通审批。

例外获准后，检查器的 **FAIL 或 UNKNOWN** 判定保持不变。`gate_exception` 记录 actor、原因、策略/代码、review digest、消息、关联和到期时间；状态转换原因为 `postcondition_exception`。节点 gate 和终态 gate 各自需要正确限定范围的例外许可。effect 授权也保留这一区别。过期或不匹配的例外一律拒绝。修复循环仍受原迭代次数、总截止时间和运行总预算限制，例外不会重置预算。

## 实际目标与本地工作区

可信 gateway 负责提供方特定的目标观察和操作应用：

- `atomic_compare`：在提供方的原子边界内比较完整审核对象并应用操作。过时的候选对象必须被拒绝，且不产生写入。这是适配器契约，并非 workflow-cli 自行提供的分布式事务。
- `observe_then_reconcile`：显式记录有界且非空的 `remaining_race` 说明。能力必须声明由提供方支持的查询。观察和应用是两个步骤；超时或不确定写入使用现有效果对账账本和原 operation key。

成功回执必须证明**实际观察到的**对象、比较模式、授权摘要，以及位于允许写入时间窗口内的观察时间。账本将回执匹配到保留的写入授权，之后通过查询取回的回执也遵循此规则。错误目标或格式错误的回复不能建立成功状态，HTTP 适配器返回 UNKNOWN。管理员可以在显式备份恢复中保留实际提供方回执并留下独立恢复审计，但不能把质量判定改写为 PASS。

HTTP 主机绑定还可设置 `workspace: {repository, path}`，其中 path 是本地 Git checkout 的绝对路径。写入时必须采用 `observe_then_reconcile`，并在 HTTP 发送前立即核对完整 HEAD，以及实际文件字节/模式是否匹配已检查的 commit。扫描包含未跟踪和被忽略文件，拒绝符号链接和不支持的条目，并使用现有的有界对象/树读取器。Git stat 缓存、`assume-unchanged` 和 `skip-worktree` 无法隐藏字节扫描中的修改。只排除根目录的 `.git` 管理条目。观察完成后会重新采样主机时钟和截止时间。查询调用不要求工作树干净。

观察与外部请求之间不会锁定 checkout。主机应使用不可变发布输入，或在可用时使用提供方原子比较；否则必须保留所声明的剩余竞态。恶意特权主机/gateway 可以伪造观察结果；这些机制是经过认证的适配器边界，不是硬件证明或操作系统沙箱。

## 最终验收清单

```sh
workflow run --artifacts <store> acceptance <db> <run-id>
workflow remote acceptance <client.json> <run-id> <new-private-output>
```

共享协议也支持 `acceptance {run_id}`。两个后端使用相同 reducer 和一致的已验证快照。报告包含冻结的要求、绑定源码的产物清单、每次 gate 决策（含失败修复轮次）、已应用的 inbox 审批、外部 effect intent/call/receipt、run/bundle/revision 身份、快照与执行历史摘要，以及内容摘要。导出前重新验证产物载荷；已提交字节丢失或损坏会中止报告，不会输出部分验收结果。

超过 8 MiB 的报告会被拒绝；本地 JSON stdout envelope 还受 CLI 的 2 MiB 消息上限约束。共享读取使用已有的作用域读取角色；CLI 独占创建 0600 文件，绝不覆盖已有归档。

`accepted` 要求 run 成功且根终态 postcondition 获准。`accepted_with_exceptions` 还会标记历史上使用过例外。没有 gate 或尚未完成的 run 返回 `incomplete`，不会虚构验收。这是一份历史交付记录，**不是将来再次发布的新许可**。每次后续外部写入仍需重新授权。摘要可以发现意外修改，但不是防御特权数据库管理员或不可信报告作者的数字签名。

## 可复现验收

```sh
cargo test -p workflow-kernel postconditions --locked
cargo test -p workflow-runstore-sqlite gates --locked
cargo build -p workflow-cli --locked
python3 examples/gates/release-acceptance.py target/debug/workflow
WORKFLOW_TEST_POSTGRES='...' python3 examples/gates/release-acceptance.py target/debug/workflow --https
```

CLI 矩阵使用临时 Git 仓库、真实定义验证器和持久保存回执的 gateway；共享模式还使用真实 TLS、PostgreSQL、经过认证的 worker 授权和有作用域的产物上传。它不会发布真实版本。两种执行方式都是 CI 必跑项。

| R03 验收项 | 证据 |
| --- | --- |
| 旧 commit 的 PASS 不能发布新目标；字节变化无法通过完整性检查 | `old-target`、`provider-change`、`dirty-workspace`；SQLite 载荷损坏测试及已有共享产物损坏契约 |
| 模型文本、空/缺失结果或伪造 PASS 不能绕过检查 | `missing`、`fake-report`；模型/写入检查器的编译拒绝回归；证据检查器缺少布尔值、类型或来源测试；R14 恶意模型响应准入 |
| 确认失败使用有界修复和总预算 | Kernel/SQLite 三轮修复、总截止时间、UNKNOWN 空闲/重试测试 |
| 并行、变化和延迟观察仍绑定版本 | 已有 instance/input/并发 claim/迟到结算测试；发布到期、暂停、租约和查询测试 |
| 最终要求/产物/检查/审批清单 | `pass`、`approved`、`workspace-pass`；摘要篡改和回放一致性测试 |
| 本地/共享一致，人工例外可追溯 | 相同 CLI 矩阵；`exception` 在两个 gate 和清单中保留 FAIL 及已认证的事件响应者 |

这个矩阵的确定性目标是零次未授权写入。检查不测量生产业务收益，也不保证第三方 gateway 实现了它宣称的原子性；适配器需要单独验证。

## 兼容性

存储表仍为 schema 11，产物目录仍为 schema 1。旧文档省略可选的 release/exception/workspace 字段，因此规范化哈希保持稳定。生成的 v1 schema 家族包含新的类型化字段。采用封闭 schema 的旧二进制会拒绝新受保护契约，不能用来执行这些 run。R11 显式备份/升级以及保留兼容二进制的恢复流程继续适用。

<!-- book-navigation -->

3.9 受保护的交付

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/09-release-acceptance.md) · [上一章: 3.8 运行时后置条件](08-runtime-postconditions.md) · [下一章: 4.1 事件与人工决策](../04-effects-and-recovery/01-event-inbox.md)

<!-- /book-navigation -->
