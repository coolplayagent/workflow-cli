# 编写与发布流程定义

注册表实现 R01 的编辑与发布部分，保存可移植定义数据及历史。它不执行节点，也不解析外部能力、
子流程 bundle、模型策略或租户权限。

## 体验编写循环

从仓库根目录执行，数据库的父目录必须已经存在：

```sh
cargo run --locked -- draft create /tmp/workflow-demo.sqlite review examples/review.yaml
cargo run --locked -- draft publish /tmp/workflow-demo.sqlite review 1
cargo run --locked -- draft edit /tmp/workflow-demo.sqlite review examples/registry/review.patch.json
cargo run --locked -- draft diff /tmp/workflow-demo.sqlite review 1 2
cargo run --locked -- draft publish /tmp/workflow-demo.sqlite review 2
cargo run --locked -- release get /tmp/workflow-demo.sqlite requirements-review 1.0.0
cargo run --locked -- release export /tmp/workflow-demo.sqlite requirements-review 2.0.0 yaml
```

重新演示时使用新数据库或草稿 ID。补丁把版本改为 `2.0.0` 并缩短审批截止时间，版本 `1.0.0`
仍保留原定义。`workflow help` 列出全部命令；通过 `bazel run` 调用时，文件和数据库使用绝对路径。

## 独立模块

`workflow-definitions` 负责编辑、评审差异、错误、revision 与 `DefinitionRegistry` 端口，只依赖 IR
和校验器，不依赖 SQLite、文件系统、命令行、传输或提供方。`workflow-registry-sqlite` 为本地 SQLite
实现端口，`workflow-cli` 组合应用命令。每个 crate 有明确的 Bazel `rust_library`，Cargo 与 Bazel
执行相同测试。其他服务可以复用端口与领域操作，并提供自己的适配器。注册表本身不承担独立的
持久化 RunStore 或集群调度职责。

## 三种不同标识

| 标识 | 含义 |
| --- | --- |
| `draft_id` | 永久的编写标识，与流程业务 ID 独立 |
| `revision` | 草稿递增正整数；编辑、替换、删除、发布前都必须比较 |
| 流程 `id` + `version` + `digest` | 不可变发布内容；同一版本名不能指向不同内容 |

草稿从 revision 1 开始。有变化的编辑在一个事务中追加完整历史并移动当前指针；无变化保留 revision，
但仍要求正确的 expected revision。整体替换相当于导入，遵循相同并发规则。草稿内流程 ID 不能改变；
复制或重命名时创建新草稿。

删除追加墓碑 revision，当前查询返回 `not_found`，历史 revision 和已发布定义仍可读取。
删除的草稿 ID 不可复用，避免旧 revision token 错配同名新草稿。存储 schema 1 不自动裁剪历史，
也没有取消发布操作。

## 增量编辑与诊断

`workflow schema patch` 导出编辑请求的 JSON Schema。补丁包含 `expected_revision` 与 1–256 个操作，
可以修改版本、入口或输入，添加/替换/移除节点和边，或明确重排全部边。添加边可指定插在某条已有边前；
替换保留稳定 ID 与位置。删除节点不会自动删除关联边或绑定，需要同批处理或修复产生的诊断。

操作按顺序作用于私有副本。未知 ID、重复插入、非法边排列或格式错误会拒绝整个批次，不追加 revision。
冲突响应包含 expected 和 actual revision。读取新草稿、检查差异，再重做需要的编辑；盲改 token
可能覆盖其他作者的工作。

草稿允许不完整的图、错误绑定和其他语义诊断，但必须使用 schema 1，节点/边稳定 ID 唯一，满足图规模
与 1 MiB 规范文档限制，并能通过有界 JSON 解析器往返。草稿即使存在诊断也有内容摘要；这与只有
静态校验成功才输出摘要的 `workflow validate` 不同。创建和编辑返回草稿与诊断。返回及存储快照统一
按节点 ID 规范化顺序，保留边顺序。

发布在写事务内检查当前 revision 并执行完整静态校验，保存规范内容、摘要、源草稿 ID 与 revision。
同一 ID/version 重发相同内容返回最初发布记录及来源；不同内容返回 `publication_conflict`，应分配新版本。
后续编辑或删除不能改变已发布内容。读取时验证内容摘要、定义身份、静态有效性及源 revision 摘要。
这些是完整性检查，不是针对数据库所有者的密码学认证。

## 评审差异与查询

`workflow diff <before-file> <after-file>` 和 `workflow draft diff <db> <id> <before-revision>
<after-revision>` 返回摘要与确定性变化。评审路径是基于 ID 键表示的 RFC 6901 指针，例如
`/nodes/review/kind/timeout_ms`。变化类型为 `added`、`removed` 或 `modified`，包含前后值。
嵌套对象逐字段比较，数组整体比较；忽略节点顺序。边优先级变化在 `/edge_order` 中体现，因为它会
影响 first-match。草稿 revision 差异还返回 `before_deleted` 和 `after_deleted`，墓碑保留原内容时
也能看见删除。

草稿与发布列表采用显式 keyset 分页：`after-id` 或 `after-version`，随后是 1–100 的 limit；第一页用 `-`。
存在 `next_cursor` 表示还有下一页。按字节词典序排序，不按语义版本排序，也没有隐式 `latest`。
每页有独立读取快照，不同请求间可能看到并发新增或删除。`draft revision` 读取准确历史，
`release digest` 通过 SHA-256 读取不可变发布内容。

JSON 响应包含 `ok`，以及 `draft`、`publication`、`page`、`diff` 或 `error`。退出 1 表示定义无效、
记录缺失、身份重复、乐观并发或发布冲突；退出 2 表示请求非法、数据库格式不匹配、损坏、锁等待耗尽
或 I/O 失败。原始导出只向 stdout 输出定义 JSON/YAML，失败写 stderr。可以导出未完成草稿；
普通文件 `workflow export` 与发布导出要求定义有效。不要把导出重定向到输入文件或数据库本身。

## SQLite 行为与限制

写入使用 immediate 事务、外键和 synchronous FULL。revision 检查、历史插入与当前指针更新为同一事务；
发布校验与插入也为同一事务。不可变历史和发布表还有触发器拒绝 SQL 更新/删除。application ID
和独立 schema 版本拒绝外来或未来版本数据库；注册表不会自动迁移存储。

只有 `draft create` 可以初始化空数据库。其他命令不带 CREATE 打开已有文件，使 SQLite 恢复中断日志，
因此普通查询也可能需要可写数据库。库还提供用于稳定数据库的 `open_readonly`，但不能恢复 hot journal。
持久化适配器禁用 SQLite URI 解释，拒绝空路径和 `:memory:` 文件名。路径必须明确，不存在隐式用户数据库。

锁争用最多等待五秒后报告 `busy`，没有绕过 revision 检查的强制选项。修改在事务内重查存储版本。
提交成功但确认丢失时，应读取当前/历史或不可变发布来核实。重试旧 token 不会创建第二个变化 revision，
但可能返回冲突。

测试使用独立进程争抢相同 revision、成功子进程退出后重开数据库，以及终止带未提交日志的 writer，
验证唯一胜者、原发布完整和未提交修改回滚。它们覆盖本地文件系统的进程故障，不代表断电、磁盘损毁
或网络文件系统。数据库仍需常规备份与容量规划；这是本地注册表，不是共享集群数据库。

SQLite 事务语义遵循 [SQLite 事务契约](https://www.sqlite.org/lang_transaction.html)与
[rusqlite 事务 API](https://docs.rs/rusqlite/0.40.2/rusqlite/struct.Transaction.html)。

<!-- book-navigation -->

2.2 编写与发布流程

[全书目录](../README.md) · [2. 定义流程](README.md) · [English](../../en/02-process-definition/02-definition-registry.md) · [上一章: 2.1 流程定义与类型](01-definition-semantics.md) · [下一章: 2.3 能力与 worker 协议](03-worker-protocol.md)

<!-- /book-navigation -->
