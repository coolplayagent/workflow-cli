# 能力调用与 worker 协议

本章介绍可工作的只读能力边界，以及本地、远程 worker 宿主共用的 JSON 契约，包括两个真实的编译器
能力。Worker 产生经过检查的观察；RunStore 与运行时负责提交运行、仲裁变更、限制并发所有者。
Worker 命令不会启动运行；独立的[运行存储 CLI](run-store.md)持久化状态与命令意图。

## 独立组件

- `workflow-worker` 负责能力描述符、严格 JSON 编解码、请求与结果校验、`CapabilityAdapter` 端口和
  进程内分发。它只依赖可移植 IR 与校验器，不依赖数据库、文件、网络或提供方 SDK；时钟可注入。
- `workflow-builtin-capabilities` 通过已有编译器实现端口。适配器接受不可变请求，返回结构化结果，
  不获得运行状态 writer 或状态变更回调。
- `workflow-cli` 组合宿主与适配器。CLI 文件传输使用远程传输实现也能调用的 `dispatch_json` 路径。

每个 crate 有独立的 Bazel `rust_library` 和 `rust_test`。本章关注 JSON 协议边界与本地文件传输；
[共享 HTTPS 执行](remote-service.md)增加经过认证的传输、调度与结果接纳。

## 发现与调用

在仓库根目录执行 `cargo build --locked` 后使用以下命令，也可调用已安装的 `workflow`。
Bazel 的二进制位于 `bazel-bin/crates/workflow-cli/workflow`。

```sh
target/debug/workflow capability list
target/debug/workflow capability describe workflow.validate 1.0.0
target/debug/workflow capability invoke workflow.validate 1.0.0 examples/worker/inputs.json
target/debug/workflow capability invoke workflow.canonicalize 1.0.0 examples/worker/inputs.json
```

两个能力都接收 `document`（字符串，不是文件路径）与 `format`（`json` 或 `yaml`），不读文件，
不访问模型，也不进行外部写入。

`workflow.validate` 返回包含 `valid`、`diagnostics` 和可选定义 `digest` 的检查结果。检查执行正常时
可以同时有 `status: succeeded` 和 `valid: false`，调用者必须先检查 `valid` 才能使用定义。
无效定义没有摘要。由于 IR v1 契约排除 null，缺失的诊断 `node`、`edge` 用空字符串表示。
`workflow.canonicalize` 要求定义有效，成功返回 `document` 中的规范 JSON 及摘要，否则返回声明的失败。

退出 0 表示通过检查的能力成功结果；退出 1 表示请求/结果被拒绝或能力声明失败；退出 2 表示用法或
文件 I/O 错误。命令输出紧凑 JSON，保持在线路字节上限内。

## 请求、授权、分发与接纳

使用新临时目录，在示例的 60 秒截止时间内一起执行：

```sh
worker_demo_dir=$(mktemp -d)
target/debug/workflow worker prepare workflow.validate 1.0.0 examples/worker/standalone-job.json > "$worker_demo_dir/request.json"
target/debug/workflow worker grant "$worker_demo_dir/request.json" > "$worker_demo_dir/grant.json"
target/debug/workflow worker dispatch "$worker_demo_dir/request.json" "$worker_demo_dir/grant.json" > "$worker_demo_dir/result.json"
target/debug/workflow worker check-result "$worker_demo_dir/request.json" "$worker_demo_dir/grant.json" "$worker_demo_dir/result.json"
```

`prepare` 解析准确的本地注册能力。Job 包含 `request_id`、`trace_id`、`timeout_ms`、`inputs` 和可选
`attempt`，独立 job 不能声明 attempt。CLI 根据宿主时钟填写签发时间和截止时间；timeout 必须为正且
不超过描述符上限。`prepare` 只提出请求，不调用适配器。

`grant` 是对该准确只读请求的显式本地授权。它检查形状、当前时间、能力标识/摘要和真实输入，再输出
绑定请求的 grant。Grant 通道和文件代表宿主权限，需要保护。摘要不是签名、认证凭据或有效租约证明；
能够签发或替换 grant 的主体可以授权请求。远程服务必须认证 controller，独立于 worker/模型数据取得
当前宿主 grant，并在提交状态前重查。请求拒绝内嵌 grant、未声明状态字段和状态变更命令。
Rust 的 `ExecutionGrant::bind` 只绑定字节，本身不执行授权。

`dispatch` 在调用适配器前验证请求与 grant，按已注册描述符检查结果，输出绑定完整请求摘要的结果。
`check-result` 使用当前 grant 和时钟复查独立收到的结果。`AcceptedResult` 是有效观察，不代表权威的
运行完成或独立验证的业务成功。宿主仍需验证产物可用性、门禁与持久化所有权。
[本地执行器](local-execution.md)提供持久化所有权与结果提交；配置在 RunStore 上的
[产物读取器](artifacts.md)检查清单、内容、类型与准确生产者/输入绑定。

输入、能力契约、截止时间、trace、请求 ID、节点实例、attempt 或租约 epoch 改变后，grant 不可复用。
相同只读请求可在过期前重复调用，因为此 worker 没有持久化去重账本。接纳结果不等于恰好执行一次；
被拒绝或过期的结果不能转换为成功。

## 通过流程节点调用

[检查流程](../../examples/worker/inspect-definition.json)与独立调用使用完全相同的输入/输出契约及
准确能力版本，其 decision 节点明确检查 `valid`。新临时目录中可准备等效节点请求：

```sh
target/debug/workflow worker prepare-node examples/worker/inspect-definition.json inspect examples/worker/node-job.json
```

再把输出传给上面的 `grant`、`dispatch`、`check-result`。示例 attempt 字段只是演示关联信息，不代表
实际创建的运行或已取得租约。集成到运行时时，宿主提供真实持久化的运行/实例/attempt 标识与 epoch。
请求准备器检查完整定义、准确能力引用、节点与能力契约一致性、真实值和前置条件，把 workflow-v1
定义摘要锁进请求。这里拒绝带模型策略的任务节点，避免原始能力调用绕过策略。

宿主负责从真实流程输入和已提交前驱输出中解析绑定，再传入节点值。这些命令不遍历图、不计算后续
decision、不建立数据来源，也不推进终态。前置条件为假的错误同样不会替运行时决定如何记录跳过。

## 描述符与线路契约

直接内置能力的 `capability list` 声明 `protocol_versions: [1]`；模型策略执行的 `model check-policy`
声明协议 2。请求、grant 和结果必须版本一致；协议 2 要求策略绑定与执行记录，直接调用省略这些字段。
未知字段与版本被拒绝，详见[模型执行](model-execution.md)。导出的 worker 请求/结果 schema 保留历史
文件名，但描述两种变体；运行时另外检查版本/字段依赖。描述符 schema 独立，当前为 1。
同一直接能力 ID/version 或同一 ID/version/策略摘要不能重复注册；不同策略可共享同一准确任务契约。
改变契约必须发布新能力版本，即使 ID/version 拼写相同，摘要不匹配也会被拒绝。

描述符包含输入/输出 IR 契约、超时、具名错误类别、操作效果声明、用法语义及可选的准确 skill 引用。
Skill 引用是用法元数据，不是可加载代码或安全策略；所有字段都参与描述符摘要。写描述符声明幂等键
作用域/保留期、查询和补偿能力。此只读分发与结果接纳路径拒绝写操作；独立的
[持久化外部操作执行器](remote-effects.md)通过意图/观察协议授权并记录写操作。

每个请求包括协议版本、request/trace ID、输入值与摘要、能力 ID/version/契约摘要、签发和截止时间。
流程作用域还带有定义摘要、运行 ID、节点 ID、节点实例、attempt 和正 epoch。结果包含协议版本、请求
摘要、完成时间、类型化输出或声明的错误代码/类别，以及产物 ID/摘要引用。Worker 只检查证据引用语法，
不获取或证实产物，不能仅凭结果视为证据已验证。产物引用摘要标识清单及其来源/内容，载荷摘要在清单内。

使用 `workflow schema capability`、`schema request`、`schema grant` 和 `schema result` 获取 JSON Schema。
运行时另检查标识、摘要、时间、形状和契约语义。所有嵌套层级（包括输入对象）的未知/重复 JSON 键都被
拒绝，尾随文档、超过 2 MiB 的消息或过深 JSON 也被拒绝。描述符最大 128 KiB；契约最多 128 个顶层字段、
4096 个类型成员、深度 16。内置流程文档仍遵守编译器 1 MiB 上限。

协议摘要对固定 `serde_json` 实现输出的紧凑 UTF-8 JSON 计算 SHA-256，对象键排序，数组顺序与数值表示
保留，并非 RFC 8785。定义摘要继续使用会排序节点的 workflow-v1 规范化。跨语言互通应先对照 Rust
编解码器和参考样例。

## 时间与失败边界

Worker 在调用前拒绝未来签发、已过期、grant 有效期不足、描述符/输入摘要改变、能力缺失和真实输入
无效的请求；执行后复查结果。有效执行截止时间取请求截止时间与从 dispatch 开始计算的适配器超时
中较早者。墙上时钟回退或超时返回都是错误；单调时间还限制调用期间墙上时钟变化时的结果接纳。

进程内 Rust 调用使用协作式截止时间：适配器收到有效 deadline，但任意阻塞 Rust 函数无法安全强杀，
只能在返回后拒绝迟到结果。硬抢占需要进程/远程适配器。Unwind 模式的 panic 被捕获；进程 abort 或外部
worker 崩溃由宿主恢复。只读声明是可信适配器契约，不是防御恶意代码的操作系统沙箱。

失败必须使用描述符声明的代码及匹配类别；transient 不自动授权重试。缺少输出、类型错误、未声明输出、
无效证据、伪造请求身份及企图携带状态变更字段都会使结果校验失败。持久化租约和产物验证由上述宿主
适配器实现。模型适配器、提供方绑定和本地/远程共用传输契约见 [R02 验收](model-boundaries-acceptance.md)。

<!-- book-navigation -->

[目录](README.md) · [English](../worker-protocol.md) · [上一章: 编写与发布流程](definition-registry.md) · [下一章: 控制流与回放](kernel-semantics.md)

<!-- /book-navigation -->
