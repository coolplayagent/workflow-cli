# 经过认证的远程执行

`workflow-service` 通过 HTTPS 传输既有认证 PostgreSQL 应用操作。API、scheduler、worker 是独立进程。
PostgreSQL 保存状态、意图、assignment、身份和审计，通知或 HTTP 响应不是唯一任务记录。

服务器在 TLS 1.2/1.3 上接收 HTTP/1.1 `POST /v1/operations`，没有明文监听器或通用 storage/event 入口。
每请求一个 `Authorization: Bearer ...`、`Content-Type: application/json` 和以下 envelope：

```json
{
  "protocol_version": 1,
  "request_id": "status-1",
  "operation": {"type": "get", "run_id": "review-1"}
}
```

`protocol.rs` 定义完整带标签操作/响应，未知版本、操作、字段拒绝。请求不能提供 tenant/project/actor
权限。Validate/read/publish/start/approve、调度租约/分发、worker assignment/finish/fail、审计、撤销及
恢复确认都调用 AuthenticatedService，在同一事务检查身份和状态。凭据创建/轮换与 bootstrap 是高权限
本地库操作，不是公共 RPC。CLI 本地 bootstrap/签发采用排他私有文件输出，从不打印 bearer。

响应重复协议版本与 request ID，含 Rust/Serde `Result`：`{"Ok":{"type":...,"value":...}}` 或 `{"Err":...}`。
客户端检查响应变体对应操作，拒绝重定向。HTTP 成功在持久化提交之后；认证失败 401、作用域资源缺失
404、输入无效 400、变更冲突 409、数据库/容量失败 503。畸形 HTTP/协议可在 envelope 前返回有界固定文本错误。

Request ID 只关联响应，**不是通用幂等键**。Start 重试绑定运行 ID 和原定义/输入，结果重试绑定服务器
assignment 与准确结果。Acquisition ID 对 scheduler 启动/尝试唯一。超时/断连可能发生在提交后，客户端
不自动重放修改；先核对持久化状态/assignment。响应过大也可能在提交后返回 unavailable。

## 宿主配置

`examples/remote/` 提供不含凭据的绑定。路径按调用进程目录解析。服务证书 SAN 必须匹配客户端 endpoint，
提供受信 CA 和私有 TLS key。客户端只信任指定 CA、验证 hostname，禁用重定向/隐式代理，并拒绝 URL
中凭据或 query string。

秘密以 `{"type":"file","path":"..."}` 或 `{"type":"environment","name":"..."}` 引用。文件必须普通、
由有效用户拥有、不允许组/其他用户访问、只有一个硬链接且最终路径不为符号链接，按打开的描述符检查。
Debug 脱敏。引用是宿主配置，不是流程输入。每次请求/连接重读文件，原子替换可在不改流程情况下轮换
bearer 或 DB 凭据；环境变化需重启，内存不是安全保险库。

数据库 `tls` 绑定要求明确 CA 与 `sslmode=require`，不回退明文；`local` 只允许明确数字 loopback TCP
或 Unix socket，覆盖 hostaddr 也检查。配置由可信宿主管理，远程请求不能改连接/TLS。集成测试使用
隔离 loopback PostgreSQL。

```sh
workflow service bootstrap server.json tenant project operator secrets/admin
workflow service issue server.json admin-ref.json runner-provision.json secrets/runner
workflow service serve server.json
workflow remote call client.json request.json
workflow remote validate author-client.json examples/review.yaml
workflow remote schedule scheduler-client.json scheduler.json 1000 100
workflow remote work worker-client.json 1000 100
```

先创建私有秘密目录。Bootstrap/issue 只输出公开 ID、expiry 等元数据，token 写入新的 0600 文件。
交付新凭据失败可能留下数据库身份而没有可用文件，按管理员审计核对/撤销；不能删除身份记录重试 bootstrap，
已有 scope 永不重新开放。初始管理员恢复需要可信 DB 操作员。

## 调度、恢复与资源边界

Scheduler 扫描有界作用域运行页，记住当前租约，按配置 worker 身份列表分发，轮转页面与 worker，
使用新 boot nonce 构造 acquisition ID。本地时钟只丢弃过期提示，DB 判断租约。Worker 扫描自己的 pending
assignment ID、检查当前 assignment，调用与本地相同 Worker，再向 API 提交。丢通知/分发回复可通过扫描恢复。

Worker 传输失败在日志使用固定诊断，已提交 assignment 不重复执行，完成传输不确定则暴露供核对。
普通 worker CLI 注册内置只读能力，显式启用的操作 worker 用[认证外部操作协议](remote-effects.md)。
宿主可用库组合兼容 Worker，拥有 assignment 不代表任意文件、模型、产物或外部写权限。

明确接纳上限：最多 256 并发连接、32 操作，默认由绑定提供；连接槽包含 TLS 握手时间。Handshake/header/
body 各限五秒，header 最多 32 项/32 KiB 缓冲，请求 2 MiB、响应 16 MiB，不保持连接复用。DB 工作在异步
reactor 外执行，即使 HTTP 超时/断开，仍保留 operation permit。不记录请求 body/header 或原始 DB 错误。

SIGINT/SIGTERM 停止 API 接纳，排空连接及已接纳 DB 操作，超出有界排空时限则报错。Scheduler/worker
循环有明确次数和轮询上限，适合由服务管理器运行。旧循环使用固定租约；[集群调度](cluster-scheduling.md)
增加 scheduler 续租、独立心跳 managed worker 和保持性的 drain。续租不延长冻结任务 deadline，过期
所有权用新 epoch 重获。Pause 保留在途所有权直到过期，不主动隔离已经接纳的任务。

[R14 安全验收](security-acceptance.md)定义短期提供方租约、assignment 时主体检查、作用域审计导出、
保留及已注册执行边界。[R09 集群验收](cluster-scheduling.md)定义共享配额、优先级老化、worker 路由/
排空、续租及故障/负载证据。宿主身份配置、上游 grant 签发和公共边缘接纳仍属部署集成，CLI 不接纳
任意第三方可执行程序。

## 可执行验收证据

服务测试使用生成证书和真实 HTTPS socket。普通 Cargo/Bazel 覆盖错误 CA/hostname、明文/含凭据 URL
拒绝、畸形/未知协议、超大 body、秘密权限及优雅退出。必需 PG CI 另运行 ignored 多进程实验：

- 两个真实 scheduler、三个 worker 经 TLS 共用 PG。一个 worker 算完只读结果后被 SIGSTOP，测试等待
  内核停止通知，再杀死持有它的 scheduler。
- 五秒租约到期后，接替者取得更高 epoch，完成内置 fork/branch/loop 与人工审批。
- 恢复旧 worker，明确拒绝其结果，运行不变；最终 frame、输入/输出、路由和状态与本地相同。
- 拒绝跨 tenant 读取和任务查找；只读数据库不能确认新运行；凭据文件轮换对既有 client 生效，子日志
  不含 bearer。

测试打印租约/扫描设置下接管到审批时间，60 秒断言是测试观察上限，不是生产 RTO。该故障模型覆盖
进程丢失和暂停旧 worker，不覆盖外部写、整个 DB 丢失或多机网络分区，不声称生产 RPO、吞吐量或公平性指标。

实现参考：[Hyper HTTP/1 边界](https://docs.rs/hyper/1.11.1/hyper/server/conn/http1/struct.Builder.html)与
[PostgreSQL rustls 集成](https://docs.rs/tokio-postgres-rustls/0.14.0/tokio_postgres_rustls/)。

## 共享产物操作

[共享产物](shared-artifacts.md)增加 assignment 绑定分块上传、凭据绑定短期下载、类型化输入/lineage
权限和带产物结果恢复，内容事务存储在 PG。CLI 在返回引用或写私有下载前验证完整内容。
[R07](artifact-acceptance.md)覆盖对象存储，[R04](shared-recovery.md)与[R14](security-acceptance.md)覆盖备份和保留。

## 定义校验与发布

[R01 验收](definition-acceptance.md)规定共享报告、本地/远程退出码和上限。授权 `validate_definition`
携带源码文本、json/yaml 格式和诊断 file 标签，不发布或执行。18 场景真实 HTTPS 矩阵检查完整报告一致
及权限，独立 CLI 实验比较输出字节，均在必需 CI。发布重新编译 bundle，原子冻结所有版本，首次 start
前拒绝冲突；并发不同发布恰有一个胜者。

## 认证外部操作与运行控制

[远程操作](remote-effects.md)说明准确 worker 策略 grant、意图/assignment 原子提交、单次投递、网关执行
和先查询恢复。`effects: true` 显式启用调度，work-effects 使用私有宿主绑定。类型化 control 向 runner/
recovery 暴露 pause/resume/cancel，不暴露任意事件。

管理员可经同一 HTTPS 预览/应用定义迁移、检查历史快照并升级旧事务镜像。角色、发布、来源 CAS 与租约
限制见 [R11 迁移](version-migration.md)。

## 安全操作

`remote audit-export <binding> <new-private-output>` 为 administrator/recovery 导出完整有界作用域审计。
生产 work-models/work-effects 要求 broker lease 引用和主体绑定 assignment 投递；格式、上限与可执行
证据见[安全验收](security-acceptance.md)。

<!-- book-navigation -->

[目录](README.md) · [English](../remote-service.md) · [上一章: 认证与角色](authenticated-authority.md) · [下一章: 共享产物](shared-artifacts.md)

<!-- /book-navigation -->
