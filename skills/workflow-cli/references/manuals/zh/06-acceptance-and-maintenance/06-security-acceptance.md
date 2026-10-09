# R14 共享执行安全

受支持的共享边界是经过认证的 PostgreSQL 应用门面及其 HTTPS 传输。凭证决定 tenant、project、actor 和角色；run ID 或调用方提供的 namespace 都不授予访问权。数据库客户端、提供方绑定、可执行注册和凭证交付属于可信主机。直接 SQL、原始本地/RunStore port 均为管理 API。

## 执行时授权

| 身份 | 权限 |
| --- | --- |
| Definition maintainer | 在自身范围内验证和发布不可变契约；不能启动 run 或审批。 |
| Runner | 启动和控制范围内 run；不能发布、审批或 dispatch。 |
| Viewer | 范围内读取；不能改变状态或导出管理审计。 |
| Approver / signal source | 仅提交匹配声明策略的决定/信号；actor 与 scope 来自认证。 |
| Scheduler | 获取带 fencing 的 ownership，向显式允许的 worker 派发精确契约。 |
| Worker | 获取和结算自己的当前 assignment；authority 检查精确能力版本、契约摘要、模型/effect/产物策略。 |
| Recovery | 检查未解决工作，在当前 ownership 下执行有界对账，导出作用域审计。 |
| Administrator | 配置/撤销身份，导出作用域审计，执行显式存储/定义迁移；不具备通用 run-start、审批或 worker 权限。 |

所有读写同时按 tenant 和 project 过滤。同一认证事务锁定凭证以防撤销竞态，检查 assignment/run/attempt/generation，并在提交前再次检查数据库时间。轮换创建新凭证并撤销旧凭证；assignment 从不隐式转移。撤销不能撤回已到达提供方的请求。已交付 effect assignment 保留在 outstanding 账本中，后继执行者必须按原 operation 身份查询/对账。参见 [effect 恢复](../05-shared-execution/05-remote-effects.md)。

模型输出只是冻结节点策略下的提案。只有列表内只读工具可以执行，真实 Worker 检查完整契约。完成数据不能替换流程图、提供审批或变成管理 RPC。服务操作类型化且有界，拒绝未知字段/操作；不存在通用 reducer-write 或 shell-execution RPC。

## 短期凭证与轮换

提供方/gateway 绑定可以引用 broker 管理的凭证租约：

```json
{
  "schema_version": 1,
  "provider": "openai_responses",
  "model": "HOST_SELECTED_MODEL",
  "endpoint": "https://model-gateway.example/v1/responses",
  "credential": {
    "path": "/run/workflow/worker-model-lease.json",
    "principal": {"tenant": "example", "project": "review", "actor": "model-worker"}
  }
}
```

私有交付文件包含 `schema_version: 1`、完全相同的 `principal`、等于完整配置端点的 `audience`、正数 `not_before_unix_ms`、`expires_at_unix_ms` 和 `secret`。寿命最长 300,000 ms。`secret` 是提供方/gateway 签发的短期 bearer，绝不是 workflow 参数。broker 认证主机身份，请求相应上游 grant，写新私有文件、fsync 后原子替换旧文件。父目录必须私有且由主机控制。binding 和 CLI 参数都不接受凭证值。

提供方/gateway 必须执行已签发 key 的身份、资源范围和到期约束。把永久 API key 包装在本地到期文档中，不会让它在上游变成短期 key。供应商只提供永久 key 时，应将其放在可信 gateway 中，向 worker 交付会过期的 gateway 凭证。gateway 是外部主机集成，不是内嵌供应商 IAM 服务。提供方侧撤销及权威 effect 查询仍属于该集成契约。

`workflow-credentials` 在每次模型/effect 调用时重读并验证交付文件，拒绝错误 principal/audience、尚未生效/过期/超长租约、相对路径、组/其他用户可访问文件、错误 owner、硬链接、符号链接、特殊文件和过大文件。凭证 Debug 已脱敏且未实现序列化。网络 timeout 同时受凭证到期和任务截止时间限制。轮换不改变引用路径或 binding digest。

`remote work-models` 和 `remote work-effects` 对生产端点要求此类租约。assignment 请求将预期主机主体作为额外限制，服务在同一交付事务中与认证身份比较。tenant、project 或 actor 任一不符，都会在提供方 I/O 前、消费 effect 交付前拒绝。服务凭证文件在请求之间改变也不会绕过此限制。SDK 主机使用带配置主体的 `work_once_bound` / `work_effects_once_bound`。同一 worker 的主机绑定必须使用一致主体。

命名 `api_key_env` 引用仍支持可信本地执行和显式字面 loopback HTTP fixture。绑定必须恰好选择一种来源。共享生产 CLI 拒绝仅环境变量的提供方绑定。不允许浏览器重定向、环境代理或自动 HTTP 重试。

成功的提供方回复也不可信。在保留任何 proposal、model identifier、receipt、output 或 reason 前，适配器拒绝回显本次调用凭证的内容，包括解码后的 JSON 转义和对象 key。模型回显成为固定 invalid-response 失败；effect 回显成为固定 unknown 观察，保留 query-first 恢复，绝不被解释为“没有发生写入”的证明。提供方错误 body 被丢弃。传输还拒绝请求数据（含字节数组）携带当前 bearer。这些检查覆盖已知调用凭证，不覆盖任意未知秘密或编码外泄。应用作者仍须避免将秘密放入业务数据和产物字节。

## 工作区、参数与网络范围

注册 builtin 能力接受类型化数据，不提供任意命令或 shell 插值。实际隔离执行为每个 attempt 分配私有工作区，并记录能力/工具/环境身份。工作区源码使用主机选择的本地 Git 对象数据库和完整对象 ID，从不执行源码脚本、hook、filter 或文件系统监控器。Git 使用显式参数数组、清空环境、禁用网络协议/lazy fetch，以及禁用全局/系统配置。子进程不包含提供方/数据库凭证。符号链接、硬链接、特殊文件、父目录遍历和未声明输出在发布前失败。经审核的合并内容成为新保留 revision，并要求新证据。

HTTP 适配器只能联系主机配置端点和固定 effect `write`/`query` 后缀。任务/模型输出不能选择主机、URL、代理、命令或额外能力。除显式字面 loopback fixture 外必须使用 HTTPS。此范围不提供通用恶意代码运行器。主机若添加任意第三方可执行适配器，必须先提供 OS/容器隔离和出口策略；注册属于特权主机代码。文件系统分离不能防御以同一 UID 运行的代码，不是安全沙箱。

## 产物访问、保留与归档

共享产物协议要求每个分块携带 bearer。下载 grant 绑定 tenant、project、精确凭证，worker 读取还绑定 assignment，并同时受配置 TTL 和凭证/assignment 窗口限制。复制 grant 到另一凭证或 tenant 不会授权数据访问。声明的类型化输入及传递血缘决定 worker 读取范围。assignment 策略决定写入，服务提供来源身份；客户端路径名绝不成为主机文件系统路径。上传、恢复及完整下载时都检查内容大小和摘要。共享 CLI 下载只在验证内容后创建新私有文件。

可信对象存储适配器产生的 S3 预签名 URL 是 bearer capability，不能替代认证共享下载协议。共享 tenant 客户端收到绑定凭证的协议 grant，不会得到原始 S3 签名凭证或无保护的预签名 URL。

| 数据 | 保留 / 删除策略 |
| --- | --- |
| 已发布定义、run 历史、execution/effect 证明、已接受决定、已提交产物与血缘 | 保留整个 run 生命周期，包括完成、取消和迁移。无在线选择性删除 API；删除被引用依赖会破坏回放/恢复。 |
| 凭证与审计 | 过期/撤销凭证失去权限，但非秘密标识和哈希保留供审计/对账。无在线审计截断。 |
| 暂存上传 / 未提交工作区或对象内容 | 仅有界孤儿清理协议可删除过期未提交对象，排除已提交依赖。 |
| 归档 | 本地已验证备份保留定义/产物闭包；完整 PostgreSQL 备份包含全部 authority/access/artifact/effect schema。归档视为机密，限制读者，并按部署策略另存加密密钥。 |
| 永久停用 | 停止准入、对账 effect、撤销凭证、验证归档及恢复，再由部署保管者按保留决策退役整个隔离 store 及备份。tenant actor 和在线 API 不能物理删除数据库。在被删除 store 之外的部署审计系统中记录保管者、归档摘要、保留决定和删除证据。 |

仓库不推断法定保留期，也不静默清除数据。不提供跨共享归档的 tenant 选择性擦除 API。整库恢复保留 tenant/project ownership，验证依赖闭包、撤销旧身份，并在任何业务工作前建立新 ownership。[R04 恢复演练](../05-shared-execution/07-shared-recovery.md) 通过真实 pg_dump/restore、新作用域管理员、旧凭证拒绝和 effect 对账验证这些行为。

## 作用域审计导出

```sh
workflow remote audit-export administrator-client.json /private/new-audit.json
```

仅 administrator/recovery 凭证可导出自身 tenant/project。一条 PostgreSQL 语句在一致快照下读取完整 scope 审计；导出自身的审计条目随后提交。导出包含 scope、认证 exporter、有序 sequence/actor/credential ID/operation/resource/outcome/time 条目，以及规范 SHA-256 内容摘要。不含请求 payload、原始错误、凭证值、bearer 值哈希或提供方响应。普通审计记录只包含有界标识和固定结果。超过 10,000 条或 8 MiB 规范内容时整个导出被拒绝，不静默截断，也不循环读取自身新增条目。更大部署在更大审计导出协议推出前，需要独立治理的数据库归档。

CLI 在独占创建 0600 文件前验证文档，fsync 字节及父目录，只打印 scope/count/digest。不覆盖已有路径。摘要可与保留值比较发现损坏，但不是签名，也无法防御可同时改写数据和摘要的特权数据库管理员。保管者通过自己的认证归档通道保留导出摘要。未认证入口及最终到期检查回滚不声称具有持久应用审计；主机负责有界传输失败监控。

## 可执行准入证据

常规 Cargo/Bazel 套件测试提供方成功/错误回显、畸形租约、文件攻击、策略限制、工作区遍历，以及不含秘密的 Git 子进程。必跑 PostgreSQL CI 执行真实授权、产物、撤销/对账、恢复和 HTTPS 契约，并运行：

```sh
python3 examples/security/acceptance.py target/debug/workflow
```

CLI 演练启动 TLS 服务和独立监听的提供方。它使用两个认证 tenant，拒绝外部 run/history/assignment，在提供方 I/O 前拒绝三种不匹配租约主体字段；在同一 worker 进程两次调用间轮换提供方凭证，不改变定义或 binding 身份；验证提供方拒绝旧凭证；阻止四种恶意输出；验证导出角色/摘要/私有文件，并扫描捕获日志、历史和导出中的每个已签发 fixture secret。

| Issue #9 验收项 | 必需证据 |
| --- | --- |
| 拒绝外部 run/event/task/artifact 访问和结果提交 | `authenticated_scope_roles_dispatch_result_and_approval_contract`、`authenticated_artifact_upload_download_recovery_and_scope_contract`、`tls_artifact_transfer_resume_binding_and_result_recovery_contract`、security CLI。 |
| 输出不能提权、伪造审批或修改流程图 | Policy/worker 契约及 security CLI 的 `escalate`、`approval`、`graph` 模式；graph digest 与节点集合不变。 |
| 已撤销 worker/旧租约不能结算；可能发生的 effect 可对账 | `worker_revocation_rotation_old_lease_and_expiry_reject_results`、`effect_revocation_takeover_requires_query_and_retains_operation_identity`、`provider_lease_principal_is_checked_in_the_assignment_delivery_transaction`、真实 HTTPS 提供方崩溃恢复。 |
| 不改定义即可轮换；记录/导出无秘密 | Lease 契约、两种 provider-wire 回显套件、Git 子进程测试，以及 security CLI 进程内轮换和保留输出扫描。 |
| 过期/复制/路径遍历下载 grant 不能泄露内容 | 产物 scope/revocation/expiry/lineage 契约、真实 HTTPS 分块传输、私有文件系统测试。 |
| 角色/恢复/审计导出正反案例；恢复保留 scope | `audit_export_is_complete_scoped_role_checked_and_tamper_evident`、security CLI、`database_restore_verifies_dependencies_atomically_and_fences_every_old_identity`、R04 pg_dump/restore 演练。 |

这些是可复现的契约观察，不宣称渗透测试覆盖、抵御可信主机失陷、任意代码隔离、生产吞吐或可用性。调度/配额/排空由 R09 负责，运行可见性要求由 R12 负责。

<!-- book-navigation -->

6.6 安全验收

[全书目录](../README.md) · [6. 验收与维护](README.md) · [English](../../en/06-acceptance-and-maintenance/06-security-acceptance.md) · [上一章: 6.5 本地平台验收](05-local-acceptance.md) · [下一章: 6.7 故障排查与贡献](07-troubleshooting.md)

<!-- /book-navigation -->
