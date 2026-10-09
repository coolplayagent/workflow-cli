# R01 定义验收与基线

定义契约是一张可移植、有版本的流程图，执行前必须检查。JSON、YAML 和 Rust builder 生成相同 IR。本地 CLI 和经过认证的 HTTPS API 使用相同解析器、静态检查器、规范化摘要及诊断报告。队列、端点和凭证保存在部署绑定中，不是业务定义字段。

## 验收映射

| R01 要求 | 可执行证据 |
| --- | --- |
| 审核接受/拒绝、并行测试、两轮修复失败后成功 | 五个已提交的 `examples/kernel/*.json` 场景；`examples/validation/baseline.py` 断言每个预期节点状态、派发任务数、loop frame 结果、唯一 instance 和失败分支对账命令 |
| 稳定导入/导出与 ID；精确编辑；不可变发布 | `workflow-ir` JSON/YAML/builder 摘要测试；`workflow-definitions` 节点/边 CRUD 与 diff 测试；`workflow-registry-sqlite` revision CAS、tombstone、进程竞态与中断事务；基线断言规范往返、精确 patch 路径、过时 revision 拒绝及发布 v2 后 v1 不变 |
| 执行前拒绝无效流程图 | Validator 测试及真实 HTTPS 矩阵拒绝不可达节点、悬空引用、隐式循环、无界 loop、不兼容输入、重复 ID 和未固定能力；bundle 测试在启动前解析精确能力/子流程/策略契约 |
| 明确定义 condition 与 join 行为 | `workflow-validator` 缺失/类型/多匹配/无匹配和有序 guard 测试；kernel 决策错误、首匹配、all/any 失败、skip、await、cancel/reconcile、不安全取消区域和不可用输出测试 |
| 本地/远程错误一致、版本不可变 | `tls_definition_diagnostic_matrix_authorization_and_immutable_binding_contract` 通过真实 HTTPS/PostgreSQL 对比 18 个案例的完整报告，检查首次启动前和并发发布冲突；`https-cli.py` 对比真实 CLI 输出字节与退出码 |
| Schema、语义、三个开发示例与检查器 | `schemas/workflow-v1.schema.json`、[IR 语义](definition-semantics.md)、[kernel 语义](kernel-semantics.md)、审核/并行/有界修复示例及 `workflow validate`；生成 schema 一致性由 Cargo 和 Bazel 验证 |

这些是定义与控制流验收。持久 wait、截止时间、事件 CAS 和 checkpoint 恢复由 kernel 与 RunStore 测试；已有多进程 HTTPS fixture 还会在 scheduler 丢失后比较真实内置工作流与本地执行。外部 effect、沙箱、真实模型提供方和其他集群服务要求各有独立 issue 与验收标准。

## 本地与远程验证

```sh
workflow validate examples/review.yaml
workflow remote validate author-client.json examples/review.yaml
```

远程命令读取本地文件，将字节、格式和诊断标签发送给 `validate_definition`。服务端绝不把标签当作路径打开。definition-maintainer 和 runner 凭证可验证；验证不会发布 bundle、创建 run 或授权执行。审计使用固定资源 `definition`，不包含源码文本、文件标签或诊断。

两个命令以相同格式输出 `{valid,digest,diagnostics}`。退出码 0 表示有效，1 表示定义无效，2 表示用法、文件、认证或传输失败。传输失败只向 stderr 输出固定消息，不产生报告；不能把它统计为验证成功或检测到定义错误。静态检查成功仍不代表完整执行 bundle 已解析，也不能证明任意业务一定终止。

源码上限为 1 MiB，诊断标签为 1024 UTF-8 字节，报告最多 256 条诊断、1 MiB 紧凑 JSON。最后一个计数位置用于 `diagnostic_limit` 标记。编码后超限的报告会被替换为单条此类诊断。触发任一限制都拒绝定义并省略 digest。CLI 美化输出的空白可能超出紧凑报告上限。CLI 输入必须是普通 UTF-8 文件；过大字节输入在 UTF-8 解码前报告。

HTTP 另外将完整请求 envelope 限制为 2 MiB。JSON 转义可能让符合编译器上限的源码超出线协议上限；这是传输/准入失败，不是编译器报告。集成测试覆盖此区别和诊断编码膨胀。无法传送或认证的请求不在一致性承诺范围内。

远程发布重新编译完整 bundle，并在写入发布 allowlist 的同一事务内冻结全部 workflow、capability、gate/model/effect-policy 身份。相同内容幂等。已有 ID/版本下改变内容会返回 `binding_conflict`，首次 run 启动前也如此。所有 key 使用统一排序锁顺序；被拒绝批次会回滚部分绑定。启动时再次检查这些身份。已有部署无需改 schema：已有 run 绑定仍具权威性；早期仅保存 digest 的发布，在重新发布或首次启动时获得版本绑定。

带 revision 的草稿编辑与历史仍位于本地 DefinitionRegistry。远程 API 提供验证、不可变 bundle 发布及执行，不暴露共享草稿 CRUD、撤回或创作 UI。草稿 revision 冲突返回 `revision_conflict`；不可变发布/run 绑定冲突返回 `binding_conflict`。

## 复现测量

在仓库根目录使用 Python 3 和已构建 CLI：

```sh
cargo build -p workflow-cli --locked
python3 examples/validation/baseline.py target/debug/workflow > baseline.json
```

确定性基线报告二进制 SHA-256、Git HEAD/dirty 状态、环境、原始命令输出和计时样本。默认重复 10 次，每次创建全新 registry、发布 v1、应用已提交的双字段 patch、检查精确 diff 和过时编辑拒绝、发布 v2，再读取未改变的 v1。计时包括 CLI 启动、解析和 SQLite 提交，不包括编译、准备和人工编写。最小值与 nearest-rank p95 仅为观察，不设验收阈值，也不宣称生产效率提升。

五张植入错误的流程图给出定义期 detected/invalid 数量及 false accept。这是固定语料回归基线，不是生产缺陷检出率。五个回放场景声明 29 个必需 node-instance 步骤，单独断言预期 skip，并报告遗漏/不匹配步骤；同时检查 task 命令数及取消/对账。事件包含主机提供的事实：回放零遗漏不能证明真实世界工作零遗漏，也不能独立证明提供方质量。

远程检查要求 `WORKFLOW_TEST_POSTGRES` 指向可丢弃 PostgreSQL，且 PATH 中有 OpenSSL：

```sh
cargo test -p workflow-service --locked -- --ignored --nocapture
python3 examples/validation/https-cli.py target/debug/workflow > https-cli.json
```

Rust 矩阵输出 18 个案例、16 个无效定义、检出数、false accept、一致性差异和耗时。单次执行是正确性基线，不是吞吐测试。CLI fixture 启动带临时证书/凭证的独立 API 进程，对比两种允许角色下 JSON/YAML 有效与无效报告，并检查未授权、缺失文件和服务停止失败。它在可丢弃数据库内创建唯一 tenant。CI 必跑 Rust 矩阵和两个 Python 脚本；应将原始输出与已检查 commit 一起保留。

<!-- book-navigation -->

[目录](README.md) · [English](../definition-acceptance.md) · [上一章: 共享备份与恢复](shared-recovery.md) · [下一章: 模型边界验收](model-boundaries-acceptance.md)

<!-- /book-navigation -->
