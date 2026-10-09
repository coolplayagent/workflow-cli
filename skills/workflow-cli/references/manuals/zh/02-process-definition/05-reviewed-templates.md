# 可评审的 SDLC 模板（R13）

模板是可选、带版本的业务定义，只有声明的前提与排除范围符合任务时才使用。普通定义与运行 API
仍可用于自主探索。模板提出冻结 bundle，实例提供业务参数，以及独立的宿主能力、操作目标、模型策略
和允许审批者清单。

## 可移植业务定义

| 模板 | 准备 | 成功交付 | 失败处理 |
| --- | --- | --- | --- |
| `template.defect@1.0.0` | 诊断固定源码 | 经过验证、审批，**等待合并**的 PR | 带原候选和真实测试诊断修复一次；再次失败则终止 |
| `template.feature@1.0.0` | 根据需求设计 | 经过评审的设计、实现与测试产物 | 同样有界修复，不执行外部发布 |
| `template.release@1.0.0` | 准备发布候选 | 独立预检、审批、提供方发布、部署冒烟测试 | 冒烟失败时按准确原部署回执补偿 |

三者共用 `sdlc.implementation-attempt@1.0.0`，稳定契约接收 request、源码仓库/revision、上下文产物及
反馈，返回候选产物、真实测试结果和诊断。首次失败的真实输出传给第二次调用，使用明确两次尝试的图；
用同一初始输入重复循环会丢失反馈。最终独立 checker 是无模型策略的直接只读任务。每个成功根终点有
冻结后置条件，类型化报告必须属于真实 checker attempt、当前输入与候选产物。

每条分支评审当前候选及质量策略的摘要。拒绝审批取消运行，五分钟无响应则失败。示例有意采用有时间
上限的协作评审；更长业务窗口需要新定义、新报告和 owner 评审。交付时重查质量、审批与目标。
提供方必须原子比较源码和候选，或使用经过单独评审并声明剩余竞争窗口的策略。补偿使用既有不可变
回执。不确定写入保留 R05 查询/恢复行为；报告本身不是重试写操作的授权。

`examples/templates/*-local.json` 与 `*-shared.json` 共用业务定义与参数契约，绑定包含逻辑适配器名、
准确能力摘要、允许目标标识和审批者。URL、凭据、数据库绑定和组织特定实现留在宿主。示例 `sdlc.*`
和 `delivery.*` 需要已注册的项目适配器。`acceptance.py` 提供可运行测试适配器，创建候选工作区、执行
真实 Python 测试，并调用一次性 PR/部署提供方。它们验证执行与恢复契约，不衡量 LLM 任务质量，也不
代表支持生产 Git/发布提供方。模型驱动适配器可使用已有有界模型协议；添加冻结模型策略属于需要评审
的定义修改，不能作为实例覆盖。

## 预览、计划与差异

```sh
workflow template validate examples/templates/defect.json
workflow template plan examples/templates/defect.json examples/templates/defect-local.json
workflow template diff before.json after.json
workflow schema template
workflow schema template-instance
```

校验检查元数据、业务角色和交付物、准确根参数契约、编译后的图/子契约、冻结等待、强制验收和批准的
外部操作。计划解析默认值、允许覆盖和业务事实，拒绝缺失/错误能力或模型策略摘要、未批准目标以及
不可用响应者，随后构造 `StartRun`，不调用适配器或创建存储。错误定位到参数/绑定路径。计划列出
依赖、所有可能分支/补偿操作、写标志、目标、门禁、等待与逐节点预算上限，是可能动作清单，不是每条
分支都必定执行的预测。

`proposed_run_digest` 使用预览时钟，共享启动改用权威数据库时间，因此真实摘要可能不同。
声明环境不授予凭据，也不保证 worker 可用，调度与 grant 独立限制访问。预算约束任务超时、循环次数、
等待、逐任务模型调用和逐 attempt 外部调用，不实现总金额统计或分布式全运行预算预留（R12）。

Diff 包含 JSON-pointer 路径、完整前后值、变化章节、强制策略变化和是否需重新评审。实例参数不能修改
门禁。不同候选必须独立进行 owner 评审；用不同内容复用已发布模板、组件或策略身份会被原子拒绝。

## Owner 评审与发布

```sh
workflow template init templates.db examples/templates/owners.json
workflow template propose templates.db candidate.json author
workflow template candidate templates.db sha256:CANDIDATE
workflow template review templates.db sha256:CANDIDATE process-owner approve 'Reviewed exact reports'
workflow template publish templates.db sha256:CANDIDATE
workflow template get templates.db template.defect 1.0.0
workflow template instantiate templates.db template.defect 1.0.0 instance.json
```

把摘要占位符替换为实际 proposal 摘要。实例输出发布来源和 `plan.request`，后者传给普通 `run start`
或共享 `Start` API。Proposal 保存模板、提出者、变化原因、兼容性说明和十个回归引用：本地与共享两种
模式下的成功、返工、拒绝、超时、终态失败。每个引用绑定模板/bundle、运行、验收报告、产物摘要、
终态和修复轮数。缺失、重复或不一致覆盖都不能发布。摘要保证完整性，**不能证明测试实际执行过**；
owner 必须检查真实报告和产物。单元测试明确标记合成引用，端到端矩阵产生实际报告及候选文件用于评审。

本地 actor 名是可信宿主声明，目录文件需要像其他本地运行存储一样保护。`init` 不能替换已有 owner
策略或打开无关 SQLite 数据库。候选、评审、发布和组件绑定仅追加；并发评审只提交一个不可变胜者。
被拒绝候选需要新 proposal，批准不能覆盖拒绝。

经过认证的操作先由可信宿主配置项目 owner：

```sh
workflow service configure-template-owners server.json owners-binding.json
workflow remote template-plan runner-client.json template.defect 1.0.0 instance.json
```

`owners-binding.json` 包含 `tenant`、`project`、`expected_revision`（创建时 null）和与 `owners.json`
形状相同的 `policy`。更新使用 revision CAS 并保留策略历史。管理操作不暴露在公共 RPC。目录 schema
与访问/运行时 schema 独立，配置 owner 时安装；未知未来版本拒绝执行。

共享 `TemplatePropose`、`TemplatePublish` 要求 `DefinitionMaintainer`；提出者由作用域凭据确定，不信任
传入 actor 字符串。`TemplateReview` 同时要求 `Approver` 和配置的业务 owner 角色，禁止 proposal 作者
本人评审。记录使用数据库时间，绑定准确候选和当前 owner-policy revision。修改 owner 使未发布旧评审
失效，需按当前策略重新提交候选；已发布版本保留历史审批。`TemplateCandidate`、`TemplateGet` 使用
既有项目读取角色。全部记录按 tenant/project 隔离，全部修改审计。发布原子冻结组件绑定，并把底层
bundle 提供给有权 runner。

子流程升级使用新固定版本，既有运行镜像、旧发布和证据不变，不会被追溯指定到最新模板。
PostgreSQL 契约测试启动真实运行、发布新子流程/根版本，再验证原快照与 bundle。R16 工具可向同一路径
提交候选，没有绕过 owner 评审的通道。

## 可复现验收

```sh
cargo test -p workflow-templates -p workflow-registry-sqlite --locked
cargo build -p workflow-cli --locked
# WORKFLOW_TEST_POSTGRES 必须指向一次性 PostgreSQL 数据库。
python3 examples/templates/acceptance.py target/debug/workflow --output template-acceptance
```

完整矩阵执行 **30 次运行**（3 模板 × 5 场景 × 2 模式），严格使用仓库中的业务 bundle。本地和 TLS/PG
worker 读取固定 Git 源码、传递类型化产物、执行候选测试、传播真实失败诊断、独立最终检查、提交绑定
审批并验证提供方回执。发布失败执行补偿且保留两份回执。六个超时用例都等待真实五分钟持久化 deadline，
不改测试定义或伪造服务器时钟；驱动程序在等待期间执行其他用例。

输出目录保留完整验收清单、产物引用与载荷、回归候选、测试 owner 发布记录和汇总。测试评审验证经过
认证的 owner 控制，不能替代生产流程 owner 的评审。CI 把相同文件保留为 `template-acceptance` artifact。
`--quick` 与 `--local-only` 明确标记为不完整的开发矩阵，从不发布候选。

Cargo/Bazel 测试覆盖确定性计划、准确执行前错误、固定项目参数、差异详情、删除门禁拒绝、缺失/错误
回归覆盖、过期候选评审、owner/作用域限制、owner 轮换、并发不可变评审、组件版本冲突、旧运行保留及
schema 导出。三个 PostgreSQL 模板测试在必需隔离数据库 CI job 执行。`generate.py` 确定性重建仓库示例，
输出变化仍须相同评审与回归。

这些测试测量执行用例数、真实修复尝试数及产物/操作数量，不证明组织级配置时间、复用率、跨项目适配
成本或生产力提升；这些比较属于 R15 验收基线。

<!-- book-navigation -->

2.5 可评审的 SOP 模板

[全书目录](../README.md) · [2. 定义流程](README.md) · [English](../../en/02-process-definition/05-reviewed-templates.md) · [上一章: 2.4 控制流与回放](04-kernel-semantics.md) · [下一章: 3.1 持久化运行状态](../03-execution-and-evidence/01-run-store.md)

<!-- /book-navigation -->
