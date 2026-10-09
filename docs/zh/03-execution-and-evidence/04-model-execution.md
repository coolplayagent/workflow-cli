# 有界模型策略执行

任务可以把局部选择交给模型，流程仍负责顺序、类型化交接、租约和强制门禁。业务定义包含能力引用与
模型策略引用；运行 bundle 冻结策略、完整任务/工具契约、目标和预算。提供方、端点、模型和凭据查找
属于独立宿主绑定。

## 组件与权限

| 组件 | 职责 |
| --- | --- |
| `workflow-models` | 可移植 `ModelAdapter`、策略校验、有界工具循环、明确记录与确定性验证 |
| `workflow-model-http` | OpenAI Responses / Anthropic Messages HTTP 格式、凭据查找、deadline 与响应解码 |
| `workflow-worker` | 准确能力/策略选择，请求/grant/输出校验与调用 deadline |
| `workflow-kernel` | 解析冻结策略和工具契约，不调用模型/网络地计算状态变更 |
| `workflow-runstore-sqlite` | 不可变策略版本锁，租约约束下提交结果，恢复时验证记录 |
| `workflow-service` / PostgreSQL | 作用域模型策略 grant、HTTPS 任务投递及相同记录的事务验证 |
| `workflow-cli` | 组合工具与宿主绑定，驱动运行，检查策略和回放记录 |

每个组件都有明确 Bazel `rust_library`，策略执行器和内核不依赖提供方库。CLI 当前注册编译器能力作为
工具；库宿主可注册其他只读适配器。工具契约缺失或改变时，在第一次模型调用前拒绝。

每次模型响应只能提出一个 `call` 或 `complete` 动作。Call 必须指定准确允许的只读能力与有效类型化输入；
complete 必须给出有效任务输出和简短可见摘要。未知字段/动作、未声明工具、写入及对同一任务能力递归
调用均拒绝。工具收到独立请求，通过 session 记录关联外层请求，不继承虚构的流程 attempt，也无权提交
运行状态。

类型正确的输出仍是模型声明，任务成功不能证明与观察一致。客观验收应使用独立校验任务与冻结后置条件。
模型不能改策略、跳过门禁、提交变更或把 UNKNOWN 变为 PASS。本版不把嵌套工具观察提升为独立提交的
流程证据，也不在模型结果附加产物；因此缺失所需证据的门禁，即使模型任务类型正确地成功，仍等待在 UNKNOWN。

## 更换提供方运行同一 bundle

`examples/models/start.json` 使用 `sop.inspect@1.0.0`，冻结策略允许 `workflow.validate@1.0.0`。
目标要求调用校验器并返回观察，后续 decision 读取 `valid`。这是提供方替换演示，不含独立验收门禁。

```sh
workflow model check-policy examples/models/policy.json > /tmp/checked-policy.json
workflow schema model-http-binding
workflow model describe-binding examples/models/openai.json
```

复制 `openai.json` 或 `anthropic.json` 到宿主管理的文件，把 `HOST_SELECTED_MODEL` 改为宿主可用模型，
本地使用时通过宿主秘密机制配置指定环境变量。共享生产 CLI 要求在 `credential` 中使用
[短期 broker 租约](../06-acceptance-and-maintenance/06-security-acceptance.md)，省略 `api_key_env`。密钥值不能写入 JSON、流程、产物或
命令行参数。`describe-binding` 只校验配置，不读密钥或联系提供方；示例不保证特定模型可用。

使用 CLI 产生的策略摘要构造绑定，不手写摘要：

```sh
python3 - /tmp/checked-policy.json /path/to/host-model.json /tmp/model-bindings.json <<'PY'
import json, sys
policy = json.load(open(sys.argv[1]))['result']['binding']
http = json.load(open(sys.argv[2]))
with open(sys.argv[3], 'w') as out:
    json.dump([{'policy': policy, 'http': http}], out)
PY
workflow run init /tmp/model-runs.db
workflow run start /tmp/model-runs.db examples/models/start.json
workflow run drive-models /tmp/model-runs.db model-inspect host-1 10 /tmp/model-bindings.json
workflow run execution-history /tmp/model-runs.db model-inspect 0 100
workflow run verify /tmp/model-runs.db model-inspect
```

替换演示使用新数据库，只改 HTTP 绑定，复用同一 start 文档。流程与 bundle 摘要相同，每次运行记录各自
配置/解析模型与绑定指纹，不保证真实模型输出一致。同一策略 ID/version 改内容会被拒绝，需发布新版本。

绑定授权向指定端点发送任务输入、冻结策略和明确的先前工具观察。宿主应按任务授权选择端点和数据访问。
只读适配器契约不是 OS 沙箱或通用提示注入防护。

## 远程模型 worker

相同准备请求、执行 grant 和模型记录通过既有认证 HTTPS 任务协议传输。Worker 凭据必须允许准确任务
能力 ID/version/摘要，并包含冻结 `model_policy`：

```json
{
  "model_policy": {
    "policy": {"id":"sop.inspect-policy","version":"1.0.0"},
    "digest":"COPY_THE_DIGEST_FROM_CHECK_POLICY"
  }
}
```

这是 `CapabilityRule` 片段，真实摘要从 `check-policy` 取得，其 `task_contract_digest` 对应规则的
`contract_digest`。仅有能力凭据不能执行模型。目标、工具或预算变化会改变摘要，需要新 grant。
一个凭据对同一任务 ID/version 授予一个策略；相同任务契约的不同策略需不同身份。

向 worker 提供已发布 bundle 与宿主提供方绑定。Bundle 只含策略与契约，凭据/端点留在宿主绑定。
示例先从 start 提取 `bundle`，再运行：

```sh
workflow remote work-models worker-client.json bundle.json model-bindings.json 1000 100
```

普通远程 scheduler 分发这些任务。`work-models` 注册内置工具和冻结模型适配器，轮询前验证绑定，
与确定性 worker 共用 `work_once`。本地注册与服务器分发都要求完整策略摘要。服务器接纳结果前重建
权威 assignment，缺失/不一致记录、错误 worker 或过期租约不能提交。`settled_tasks` 包含模型失败，
业务成功要看运行状态。

HTTPS envelope 仍为 v1，其中模型请求/结果使用 worker 协议 2。旧配置命令拒绝新 grant 字段，旧的
内置能力 worker 不能执行模型策略；分配模型任务前先升级服务器和具备作用域权限的 worker。
替换提供方只改 worker 的 HTTP 绑定，回放不再次调用提供方。

`https_model_bindings_records_failures_and_policy_authority_contract` 使用真实 HTTPS/PostgreSQL、独立
worker 进程、确定性适配器和两种 HTTP 格式，比较进程内业务输出，检查策略拒绝/回滚、伪造结果、
提供方不可用、非法变更提议，以及重开后不再调用提供方。这是协议测试，不是在线模型质量或费用测量。

## 记录、协议与恢复

直接调用保留协议 1，省略新字段；模型调用使用协议 2，`WorkRequest` 带准确 `model_policy`，grant
版本匹配，`WorkResult` 必须有 `model_record`。模型策略/提议/记录 schema 各自为 1。通过
`workflow schema model-policy`、`model-proposal`、`model-record` 和 `model-http-binding` 导出。

宿主记录完整准备请求、策略摘要、适配器/配置身份、请求/解析模型、可见响应 ID、报告的 token 用量、
各明确提议、工具请求/结果或拒绝、单调事件时间及推导结果。缺失用量保持未知。记录不含凭据值或隐藏
推理，但可能含敏感业务输入和工具输出，应据此管理数据库和导出历史。

`workflow model dispatch <policy> <http-binding> <request> <grant>` 返回原始 worker 结果，不提交状态。
可信宿主需用当前 lease/attempt 经 `run finish` 提交。`model check-record <policy> <request> <result>`
不联网、不调用工具地确定性回放，拒绝上下文/请求/策略摘要不匹配、工具输入变化、错误输出契约、事件
缺失/多余以及完成后时间戳；它不认证提供方、grant 或当前租约。

SQLite 提交先回放并检查结果，再原子提交结果、事件、状态和回执；恢复复查相同历史。模型策略运行
拒绝原始成功任务事件。摘要绑定内容，不是签名，本地适配器与数据库完整性属于可信宿主边界。
已完成运行重启不再次采样模型。

Schema 5 开始保护这些语义，当前存储是 schema 11。旧版本遵循显式备份[迁移](../04-effects-and-recovery/05-version-migration.md)，
已有运行需要产物时配置 `--artifacts`。旧协议 1 字节、请求/bundle 摘要及已完成运行历史保留。

## HTTP 与预算边界

适配器发送非流式 JSON 提议指令，不使用提供方内置工具或函数执行。OpenAI 使用 `instructions`、文本
`input`、`max_output_tokens` 和 `store: false`；Anthropic 使用 `system`、用户消息和 `max_tokens`。
字段对应官方 [Responses API](https://developers.openai.com/api/reference/cli/resources/responses/methods/create)
与 [Messages API](https://github.com/anthropics/anthropic-sdk-python/blob/main/api.md#messages)。
提供方输出不可信，只接纳完成的 assistant 文本中准确的提议 JSON，丢弃隐藏推理块。拒答、截断、
格式错误/重复键 JSON 和意外工具块都使调用失败。

必须 HTTPS；测试只有显式选项和字面 loopback 地址才允许 HTTP。禁用重定向、继承代理和自动 HTTP 重试。
每个请求时限取剩余 deadline 与 60 秒的较小值，原始提供方响应最多 256 KiB，不持久化错误响应体。
实现使用 reqwest 的[客户端控制](https://docs.rs/reqwest/latest/reqwest/blocking/struct.ClientBuilder.html)
和[禁止重试策略](https://docs.rs/reqwest/latest/reqwest/retry/fn.never.html)。

策略约束调用数 1–16、工具数 0–16、上下文 1–256 KiB、响应 128 B–64 KiB，以及每次请求输出 token
1–32768。完整 worker 结果仍受 2 MiB 限制，单项预算符合不保证全部记录能装下。声明失败包括
`model_unavailable`、`model_invalid_response`、`model_refused`、`model_budget`、`model_deadline`，均为
permanent。Driver 退出 0 表示持久化操作成功，业务结果仍检查快照。

上限按 attempt 计算。超时或崩溃可能留下未知账单，后续 attempt 可能再次计费。不声称全运行原子金额
预算、流式输出、session 中途恢复或恰好一次模型计费。同步自定义适配器需配合 deadline，租约只限制
迟到结果，不能终止任意 Rust 代码。已提交记录回放，未提交 session 可能在既有重试上限内重做。

## 验证与后续范围

测试覆盖确定性假模型、真实内置工具、loopback 上两种真实 HTTP 格式、同一业务 bundle 更换绑定、
持久化拒绝/恢复、强制 UNKNOWN 保留以及无提供方访问的重启。未使用生产凭据，证明线路与权限行为，
不证明在线服务兼容性或模型质量。存储迁移也对真实旧 v4 二进制/数据库执行，逐字节保留快照和执行记录。

[R02 验收](../06-acceptance-and-maintenance/02-model-boundaries-acceptance.md)映射公共接口、本地/远程矩阵和可执行替换示例。动态模型工具
保持只读，声明的写节点使用持久化外部操作执行器及独立权限。全运行成本、模型质量、工具隔离、模型
工作区/门禁绑定和自动产物发布仍为独立路线图工作。

<!-- book-navigation -->

3.4 有界模型执行

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/04-model-execution.md) · [上一章: 3.3 本地守护进程](03-local-daemon.md) · [下一章: 3.5 产物与来源](05-artifacts.md)

<!-- /book-navigation -->
