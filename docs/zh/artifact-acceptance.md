# R07 产物、隔离 attempt 与重新验证

产物保留精确类型、不可变清单、run/node/attempt、request/input 摘要、源码 revision、输入链接、scope 和保留策略。存储与传输不改变 `artifact://` 身份。已接受 worker 输出仍是权威依据；单独上传报告或提供模型解释不能通过 gate。

## 验收证据

| R07 标准 | 可执行证据 |
| --- | --- |
| 发布产物可追溯到要求/设计/代码/测试/attempt | 本地血缘/影响契约、工作区 capture/proposal/merge 清单，以及 `real_s3_roundtrip_lineage_download_auth_integrity_and_process_crashes`；对象 CLI 演练迁移真实已接受报告并保留生产 attempt 和源码 revision。 |
| 类型错误、文件缺失或摘要变化阻止消费/gate | 本地和 S3 reader 验证完整祖先链；已有持久 artifact/gate 契约和 `input_invalidation_revokes_prior_gate_pass_until_fresh_evidence_is_bound` 证明拒绝不确定证据。 |
| 并行修复不能静默覆盖；合并代码重新验证 | 独立进程工作区测试和 `sealed_parallel_repairs_require_explicit_conflict_resolution_and_create_a_new_verified_revision` 覆盖独立文件、冲突选择和精确新 Git 对象。`execute-isolated.py` claim 两个真实修复 attempt，拒绝未解决冲突，显式合并封存提案，对精确新 revision 执行真实 validator 与冻结 gate，并拒绝旧源码绑定。 |
| 导出到对象存储保留引用 | `object-contract.py` 将四个已接受产物导入真实 MinIO，通过 S3 reader 读取并验证两个持久 run，再导出到新本地 store，引用完全相同。 |
| 上传进程中断不能产生成功的不完整引用；孤儿可回收 | 已有本地上传/提交进程终止，以及真实 S3 子进程在上传前、8 MiB 流式 PUT 中途、PUT 后、manifest 提交前后终止。重开、清理和精确重试验证边界。 |
| 输入变化识别受影响证据/节点并保留历史 | `durable_revalidation_view_blocks_old_and_new_stale_descendants_but_preserves_history` 检查传递生产者投影、后来出现的过时后代、新证据和原历史；按选定当前输入策略撤销先前 PASS。 |

这些测量确定性契约正确性和进程中断，不估算生产吞吐、模型质量或业务时间节省。

## 在工作区执行实际任务

```sh
cargo build --locked -p workflow-cli
python3 examples/workspaces/execute-isolated.py target/debug/workflow
workflow run --artifacts /host/artifacts drive-workspaces runs.db run-id owner 20 workspace-binding.json
```

示例自行创建 Git 仓库，用五个隔离工作区执行六个持久 attempt。两次修复将有冲突的 fixture-host 编辑记录为封存提案；显式 claim 的合并生成已验证的新 Git revision。原 revision 与合并 revision 各执行内置 validator、捕获类型化报告并通过冻结 postcondition。不匹配源码在调用前被拒绝。示例检查原仓库及已完成 run 不变。提议修改来自 fixture host，不暗示存在模型或 shell 修复适配器。

`drive-workspaces` 选择含 `workspace_store`、`repository_path`、`source_revision` 和 `capabilities` 的主机绑定。每项 capability 指定精确能力版本、将 UTF-8 请求输入映射到已提交路径的 `inline_files`、可选类型化 `input_artifacts`，以及带精确路径/类型/输出字段的 `reports`。完整生成绑定见示例。运行前需显式初始化 workspace/artifact store。

runtime 在租约下 claim 实际请求，将分配绑定到该 attempt，对比请求字节和已提交源码，调用 builtin，写入精确类型化输出，捕获产物，再通过已有 fencing 完成协议结算。未声明源码变化会被拒绝。重复输出写入必须具有相同字节；部分写入/冲突输出需要新 attempt。环境记录 OS、架构、Git 版本、可执行文件 SHA-256、CLI 版本及能力身份/契约。已有 attempt 不能换用另一可执行绑定。输出/capture 失败绝不会将任务结算为成功。

`TaskExecutor` runtime port 支持主机执行包装器，不会把 claim、lease 或结果 authority 移出存储。提供的包装器只对精确内联输入运行只读 builtin，不调用 shell 命令、模型选择的程序或共享外部写入。无状态、仅内联输入的任务仍可使用普通 `drive`。进程沙箱及命令工具边界见 [R14](security-acceptance.md)；已有 effect 适配器仍保留独立声明目标与 effect authority。目录分离不是操作系统安全边界。

## 封存、审核与合并提案

```sh
workflow workspace seal workspaces workspace-id artifacts summary.json
workflow workspace merge-plan artifacts repository-id source-repo source.json proposals.json resolutions.json
workflow workspace merge-apply artifacts repository-id source-repo plan.json merge-request.json summary.json new-merged.git
```

`summary.json` 是解释决定的有界 JSON 字符串。`proposals.json` 是精确 proposal 产物链接的已排序数组。所有提案必须具有相同 run、源码 revision 和完整 baseline。最多接受 16 个提案，每个提案最多 64 个声明的文件修改/删除。封存验证全部输入产物、捕获变化后的类型化字节并记录整个观察到的树。后续编辑不能改变已封存提案。未声明变化、不安全文件系统条目或捕获时文件变化会停止封存。

plan 合并互不相交或相同的编辑。同一路径上不同编辑/删除产生冲突；`resolutions.json` 将精确路径映射到某个提案产物 ID。未使用或虚构的 resolution 会被拒绝。路径/大小写/目录碰撞也拒绝 plan。应用时重新验证源码、保留提案、选中字节和完整 plan，从不读取后来可变的工作区编辑。

apply 命令要求该 run 中 merge producer 的 workflow request。它写入新 bare Git 仓库，验证真实 SHA-256 commit/tree/blob 对象，并发布含提案血缘的不可变 merge record。原仓库/index/refs 和 proposal 工作区不变。新 root commit 记录已审核 plan digest 和原 revision；逻辑祖先关系留在 manifest 中，不虚称存在未导入的 Git parent。

plan 与 merge record 都要求重新验证。后续任务输入和冻结 gate 目标应使用产生的源码 revision。已有 gate 将报告绑定到精确 revision/input/attempt，提案或旧 revision 的报告不能验证合并目标。主机在调用独立 CLI 基础命令前必须验证 merge request 的权限；集成 runtime 通过持久 task claim 完成此检查。

创建仓库后失败可能留下未确认目录。命令拒绝复用已有目标；显式重试到另一个新目录，会从同一审核 plan 产生相同 Git 身份。

## 输入变化后的当前证据

```sh
workflow artifact invalidate artifacts request.json source.json replacements.json summary.json
workflow run --revalidated-artifacts artifacts plan-artifact-id status runs.db run-id
workflow run --revalidated-object-artifacts object-binding.json plan-artifact-id verify runs.db run-id
```

每项替换为 `{ "old": ArtifactLink, "new": ArtifactLink }`。替代输入必须具有相同精确类型/run scope，独立验证通过，且不能继续依赖被替代输入。保留的 plan 记录 inventory digest、变化输入、传递影响的 artifact/producer/attempt 身份、`require_fresh_evidence` 策略和显式决策说明。发布 plan 保留新旧输入祖先链，不改变任何历史 manifest 或 run event。

当前 reader 拒绝旧输入及全部依赖产物，包括投影后才发布的产物。报告若只是新增 attempt ID，仍保留旧依赖，依然无效。基于替代输入重新计算的证据，只有通过正常 result/gate 验证后才具备资格。历史审计使用原 artifact reader。采用当前视图的恢复读取，可能正确地拒绝包含已失效证据的历史 run。

这是主机显式选择的策略，不是修改运行中的不可变输入，也不会自动重放业务写入。新 revision/run 或已声明返工路径提供新任务输入。策略要求新证据，不复用或静默改写先前成功决定。

## S3 对象与共享清单 authority

`workflow-artifact-s3` 实现相同 reader/store/inventory port。S3 保存 payload，PostgreSQL namespace 保存只追加 manifest 链。使用同一数据库 namespace 的不同主机通过 catalog 行串行化发布与清理。namespace 绑定 endpoint、bucket、region 和 prefix。只有 `s3-init` 创建 catalog；bucket 由其 owner 配置。

```json
{
  "namespace": "project-artifacts",
  "database": {
    "connection": {"type": "environment", "name": "WORKFLOW_OBJECT_DATABASE"},
    "transport": {"type": "tls", "ca_file": "/host/postgres-ca.pem"}
  },
  "object": {
    "endpoint": "https://s3.example.invalid",
    "bucket": "workflow-artifacts",
    "region": "us-east-1",
    "prefix": "project"
  },
  "access_key": {"type": "environment", "name": "WORKFLOW_S3_ACCESS"},
  "secret_key": {"type": "file", "path": "/host/private/s3-secret"},
  "ca_file": "/host/s3-ca.pem"
}
```

可选 `session_token` 使用相同 secret-reference 格式。对象端点要求 HTTPS；仅字面 loopback HTTP 可通过 `allow_http_loopback: true` 启用，供可执行 fixture 使用。请求 body/timeout 有界，不重定向、不发现代理、不自动重试，错误中隐藏提供方 body 和凭证。[S3 Signature V4](https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-authenticating-requests.html) 认证请求；[条件写入](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) 防止覆盖发布 key。

发送字节前，发布先持久预留唯一 key，然后执行一次条件 PUT，回读并验证字节，再于同步 PostgreSQL 事务中提交 manifest 与 integrity head。提交前不返回引用。提交后响应丢失可由精确产物身份恢复；更早失败留下未提交预留，后续 attempt 不会接管其 key。

清理使超过五分钟的未提交预留到期，验证所有保留对象，只删除废弃 key，绝不删除已提交 manifest 或对象。tombstone 保留，因此重复清理也能发现死亡进程迟到的 PUT。清理分页执行，报告删除请求，不声称删除了未经测量的字节数。PostgreSQL/catalog 故障或保留内容损坏会停止清理。未完成上传不是成功产物。

```sh
workflow artifact s3-init object-binding.json
workflow artifact s3-import object-binding.json local-artifacts artifact-id
workflow run --object-artifacts object-binding.json verify runs.db run-id
workflow artifact s3-export object-binding.json artifact-id new-local-artifacts
workflow artifact s3-download-grant object-binding.json artifact-id 60 new-private-ticket.json
workflow artifact s3-cleanup-orphans object-binding.json - 100
```

导入/导出验证完整血缘并保留全部引用。本地路径、bucket key、数据库端点和凭证是主机绑定，从不成为产物身份。`drive-workspaces` 也可通过 `--object-artifacts` 直接发布。已有认证共享服务传输继续约束 assignment 派生生产者、精确类型、run scope 和短期分块 grant。

S3 下载操作是可信主机授权边界：请求 ticket 前先授权 worker/run/assignment。它将 bearer URL 写入新的私有 0600 文件，stdout 仅报告产物和到期时间。URL 在 1–300 秒内允许对精确单一资源执行 GET，不提供 list/PUT/catalog 凭证；祖先产物需要单独 ticket。worker 必须通过认证通道获取预期引用并据此验证字节。签名 URL 在有效期内可复用，不是一次性凭证。参见 [S3 预签名请求语义](https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sigv4-query-string-auth.html)。

保留产物使用 `run_dependency`。每个对象 catalog 上限：512 个产物、512 MiB 保留 payload、单对象 64 MiB、单 manifest 64 KiB、512 个祖先、10,000 条废弃预留记录。容量耗尽会显式报错。应轮换至新 namespace，不删除保留历史或 tombstone。备份必须同时保留 PostgreSQL 对象 catalog 和全部被引用 S3 对象；本地 run archive 不能单独备份此适配器。恢复后的 catalog 不得对仍独立活动的源 namespace 执行清理。

在可丢弃 PostgreSQL 上执行真实互操作验收：

```sh
WORKFLOW_TEST_POSTGRES='host=127.0.0.1 port=5432 user=postgres password=fixture' \
  python3 examples/artifacts/object-contract.py target/debug/workflow
```

脚本验证固定 MinIO 可执行文件摘要，仅在 loopback 启动，运行进程故障与权限测试，迁移真实已接受任务证据，再停止服务并清理临时文件。PostgreSQL 必须显式提供。fixture 版本用于可复现，不是生产服务推荐。此演练与自动工作区示例均为 CI 必跑步骤。

<!-- book-navigation -->

[目录](README.md) · [English](../artifact-acceptance.md) · [上一章: 审批验收](approval-acceptance.md) · [下一章: 本地平台验收](local-acceptance.md)

<!-- /book-navigation -->
