# 类型化产物与运行证据

Workflow 把报告或文件视为版本化交接。`workflow-artifacts` 定义可移植契约及 reader/store 端口，
`workflow-artifact-local` 使用持久化本地文件与 SQLite 事务清单目录实现。两者不依赖 worker、内核或
运行数据库。RunStore 注入 `ArtifactReader`，在接纳 worker 结果前和恢复期间验证引用。每个模块都有
明确 Bazel `rust_library`。[R07 验收](../06-acceptance-and-maintenance/04-artifact-acceptance.md)说明已经完成的跨适配器工作流。

## 通过 CLI 提交真实报告

```sh
cargo build --locked
python3 examples/artifacts/record-validation.py "$PWD/target/debug/workflow" /tmp/workflow-artifact-demo
```

使用新输出目录。示例从当前 Git revision 读取已提交的校验输入，启动运行，取得持久化租约，领取任务，
调用真实内置 worker，发布类型化校验报告，再带经过验证的证据完成任务。还把报告导出、导入到第二个
本地存储，并在那里验证相同运行。无需模型、网络账号或外部写适配器。源码 revision 标识被检查的已提交
输入，不代表所有未提交开发文件。

中间 JSON 文件让宿主协议可检查。`run acquire`、`renew`、`claim`、`tick-due`、`finish`、`attempt-failed`
和 `release` 暴露既有执行端口。必须使用提交响应中的准确 lease/request/grant/attempt 身份，不能编造。
`worker dispatch` 调用能力，`run finish` 在所有权约束下原子提交结果/事件/状态/回执。Lease 文件是可信
本地宿主 token，不是远程认证凭据。

```sh
workflow artifact init /path/to/artifacts
workflow artifact prepare request.json type.json source.json input-refs.json
workflow artifact put /path/to/artifacts publish.json report.json
workflow artifact verify /path/to/artifacts <artifact-id> expected-type.json
workflow artifact lineage /path/to/artifacts <artifact-id> - 20
workflow artifact impact /path/to/artifacts <replaced-input-id> - 20
workflow run --artifacts /path/to/artifacts status runs.db <run-id>
```

多数响应为 `{"ok":true,"result":...}`，将 `result` 保存为后续输入；worker 请求/结果仍为直接协议 JSON。
`artifact prepare` 绑定提供请求的生产者和输入摘要，不证明请求已领取或执行；`run finish` 按权威
持久化 attempt 检查。源码 revision 与上游产物列表是宿主声明，不是经过认证的工作区证明。

## 引用语义

`ArtifactRef` 包含不可变清单、SHA-256 清单摘要、推导出的 `artifact-<manifest-hash>` ID 和可移植
`artifact://<id>` 位置。URI 由配置的 store 解析，不是可下载 URL 或本地路径。清单绑定：

- 准确产物类型 ID/version 和内容 schema：字节、UTF-8 或类型化 JSON。
- 原载荷字节长度和 SHA-256 摘要。
- 生产运行、节点实例、attempt、准确 worker 请求和输入摘要。
- 来源仓库标识及完整 40/64 位小写 Git revision。
- 准确输入产物 ID/清单摘要、运行访问作用域及保留策略。

Worker 的 `EvidenceRef.digest` 绑定**清单**，包含来源与内容身份，不是载荷摘要；后者是
`manifest.content_digest`。无需改变 worker 协议版本或能力描述符，清单/ref 独立版本化。

类型化 JSON 使用 IR 封闭 `ValueType`，必需成员必须存在，未知字段、错误类型及任意层重复键拒绝。
消费者通过 `artifact verify` 或 `verify_expected` 提供完整预期类型，文件名或媒体类型标签匹配不够。
同一 store 中产物类型 ID/version 不能改绑 schema。相同规范和字节发布幂等；不同 attempt、输入、源码
revision、类型或载荷产生新清单身份，不能覆盖旧交接。相同载荷可共享一个对象。

`lineage` 返回所请求产物之前经过验证的祖先；`impact` 返回输入被替换后需要重新验证的保留下游产物，
包括生产节点/attempt。两者都是只读投影，页大小 1–100，使用不包含当前位置的 `next_cursor`；不会修改
历史决策、继续或重新计算运行。输入变化或旧 attempt 不能在提交时复用证据，当前运行输入不可变。
[R07 当前证据视图](../06-acceptance-and-maintenance/04-artifact-acceptance.md#输入变化后的当前证据)说明替换策略：重算输出被接纳前，
传递性过期证据被拒绝。

## 发布、恢复与清理

本地 store 包含 `objects/`、`uploads/` 和 `catalog.sqlite`。路径来自验证过的摘要，调用者位置不能逃出
布局。只有 `artifact init` 创建存储；打开缺失 store 不会创建，外来 application ID/schema 及符号链接
目录/文件都拒绝。新 Unix 目录/文件采用 0700/0600，已有根目录和导出文件的访问仍由宿主管理。
使用宿主管理的本地目录。能够替换根目录、改文件或重写目录摘要的恶意 OS 主体不在边界内；这不是
attempt 工作区隔离或抵御恶意本地进程的沙箱。

发布用 SQLite immediate 事务串行化清单变化与清理：写唯一临时文件、sync、不覆盖地发布内容、sync
对象/上传目录，再以 synchronous FULL 提交清单和目录计数/摘要链。提交后才返回引用。回复丢失时重试
准确规范与字节；不同字节是不同身份，不能换摘要掩盖不确定状态。

Reader 校验整个不可变目录及计数/摘要链、所有输入身份/作用域，并沿所选 lineage 验证字节和 schema。
尾部缺失、文件缺失、字节改变或类型版本冲突均拒绝。没有覆盖式修复，重发不会悄悄修复已有损坏对象。

`artifact cleanup-orphans <store>` 取得与发布相同的写锁，验证目录和全部保留对象，再删除未提交临时文件
及无清单对象。活动上传不会与清理竞争而丢失对象。已提交清单及其全部内容保留，即使当前没有运行引用。
此版本仅有 `run_dependency` 策略，未实现释放/归档清单或按时间清理，因此 cleanup 不能删除活动运行的
恢复依赖。清理错误前可能已删除部分无引用文件，重试安全。

明确上限：载荷 64 MiB、类型化 JSON 2 MiB、清单 64 KiB、直接输入 128 个、lineage/impact 单次最多
512 个不同产物、目录最多 10000 项。读取验证完整选中内容，发布在内存缓冲有界载荷，不声称生产吞吐量
或常数时间恢复。CLI 响应仍限 2 MiB，大清单需减小页；载荷导出避免把大字节放入 JSON。

## 结果接纳与持久化依赖

Rust 使用 `SqliteRunStore::with_artifacts`，或在相关 CLI 操作加 `run --artifacts <store>`。位置是宿主
配置，不作为绝对路径存入运行。结果中的每个证据引用必须解析为经过验证且准确匹配运行/节点实例/
attempt/请求/输入摘要的产物。完成还受有效租约、输出契约、节点资格和提交时 deadline 限制。

先发布产物，再完成运行。两者之间崩溃可能留下无运行结果的已保留产物，这是安全的；运行状态不能确认
缺失或未完成内容。运行完成后崩溃，回放不会重发产物或调用 worker。结果重试保留原身份。

带证据运行的读取和修改要求 reader，缺配置或依赖报错，不能报告健康。载荷损坏也阻止 status/verify/
恢复。替代适配器必须验证等价清单与字节并遵守保留策略；没有宿主配置就不自动查文件或网络。
管理 `run event` 可以接收不带证据的其他可信内核事实，不能向不可信 worker 开放。含后置条件的运行
拒绝原始成功任务事件，所有原始 gate 决策都拒绝。产物完整性不证明业务声明真实：
[证据策略](07-evidence-gates.md)和[强制后置条件](08-runtime-postconditions.md)检查已接纳 worker 输出。
[共享产物访问](../05-shared-execution/04-shared-artifacts.md)增加经过认证的上传和生产者接纳。

运行存储为 schema **11**。`run --artifacts <store> migrate <db> <new-backup-file>` 在验证备份后显式
升级 schema 1–11，检查依赖并保留执行记录；外来/未来 store 拒绝。详见[版本迁移](../04-effects-and-recovery/05-version-migration.md)。
产物目录独立为 schema 1，已有定义/worker 线路格式不变。

## 可移植性与验证边界

`artifact export <store> <id> <new-file>` 不覆盖地导出验证过的载荷，返回准确引用。保存引用后使用
`artifact import <destination-store> <reference.json> <payload>`；导入先检查预期引用，再发布。
图结构按 lineage 顺序迁移，确保准确上游先存在。两个本地 store 产生相同引用。对象存储也可实现相同
端口，[R07 验收](../06-acceptance-and-maintenance/04-artifact-acceptance.md)介绍兼容 S3 的适配器及作用域传输权限。

测试在载荷上传中途、文件 sync 后、对象发布后、清单写入后和提交后终止进程，恢复只发现孤立对象或
完整持久化清单。还测试独立发布者竞争、清理互斥、SQLite full/只读、修改/移除文件、符号链接和目录
尾部损坏。真实 worker 报告只有发布后才能接纳；错误输入来源、缺 reader 或迁移后损坏内容均拒绝。
Schema 2 迁移测试保留租约隔离。

证据覆盖 Linux 本地进程故障与注入 SQLite 故障，不证明任意断电/文件系统持久性、磁盘丢失恢复、恶意
共享目录安全、远程认证或业务收益。[R07 验收](../06-acceptance-and-maintenance/04-artifact-acceptance.md)记录隔离工作区、评审合并、共享
存储及输入替换后的当前证据；[本地备份](../04-effects-and-recovery/04-backup-recovery.md)和[共享恢复](../05-shared-execution/07-shared-recovery.md)说明独立
归档与生命周期边界。

## 工作区输出

[工作区适配器](06-workspaces.md)按声明路径和准确类型捕获产物，保留 lineage 包含原输入和捕获文件的
输出清单；保留分配的基础源码 revision，另记变化树摘要。宿主执行必须把真实输入绑定到该目录并提交
真实结果后，报告才能成为合格证据。工作区观察不授权外部操作，也不自动在门禁消费时检查当前目标。

<!-- book-navigation -->

3.5 产物与来源

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/05-artifacts.md) · [上一章: 3.4 有界模型执行](04-model-execution.md) · [下一章: 3.6 尝试级工作区](06-workspaces.md)

<!-- /book-navigation -->
