# 共享 PostgreSQL 运行权威

`workflow-runstore-postgres` 实现 RunStore、ExecutionStore、InboxStore、EffectStore 与 RestorationStore。
经过认证的宿主提供 PostgreSQL client 及服务器推导的 tenant/project。库本身不暴露公共 HTTP、不认证
worker，也不实现 R09 scheduler。调用方在 `postgres::Client` 配置数据库认证与 TLS。
`PostgresRunStore::initialize` 明确创建专用 schema，open 要求准确版本，不会暗中初始化或升级。

每个运行是 PostgreSQL 中一个有界事务聚合。修改锁定运行行、验证内容摘要、构建临时 SQLite reducer 并
回放状态和执行日志，只安装编译进程序的 SQLite schema。镜像是类型化行，不包含 SQL 或原生数据页。
复用 reducer 保留任务/门禁/外部操作、回执、Inbox 和恢复规则，避免两套状态实现。只有 PostgreSQL 提交
后镜像才成为权威，内存结果不能作为持久化成功返回。

运行状态、事件、outbox、执行/操作日志与不可变版本目录一起提交。同作用域流程/能力/门禁/模型/操作
版本跨运行不能绑定不同内容。主数据库时间用于租约、attempt、timer、effect 和回调接纳，客户端时钟
不能延长权限。Reducer 保留 lease/request/gate deadline，序列化和目录加锁后，最终条件 PostgreSQL
更新再次检查；新回调也保留不含到期点的接纳窗口。行锁串行化所有权，generation 条件进一步保护更新。
数据库错误或提交结果未知均返回错误，宿主按稳定身份核对，不能假定回滚。

读取使用 repeatable-read 快照，回放聚合并验证共享绑定；分页采用确定性字节排序，每次加载一个有界
聚合。损坏/不完整行使 status/list 失败，不会悄悄跳过。数据库错误只暴露有界通用诊断，不暴露连接串、
SQL、参数或服务器原始错误。

## 验证与运维边界

Postgres CI 使用一次性 PostgreSQL 17，并显式运行平时 ignored 的集成测试：

```sh
WORKFLOW_TEST_POSTGRES='<disposable connection string>' \
  cargo test -p workflow-runstore-postgres --locked -- --ignored --nocapture
```

契约覆盖独立 client 进程争抢租约、过期 epoch、真实内置能力、人工审批/去重、不可变版本冲突、作用域
隔离、重开/回放、合成操作回执、reducer 回滚、最终写入过期、只读事务、数据库连接被终止及损坏行。
合成回执只验证存储，不证明真实发布提供方行为。镜像测试检查日志往返、删除执行记录、运行身份不匹配、
列格式错误、大小限制及拒绝多运行镜像。

每运行镜像最多 64 MiB，在 PostgreSQL schema 1 内使用 SQLite application schema 12。每次修改重写并
验证整个聚合，有明确内存/CPU/写放大成本，不声称吞吐量。大历史需要另行验证存储演进。库约束 SQL
语句、锁和空闲事务等待并使用 synchronous commit，建连超时由调用方负责。

数据库 client 是可信服务凭据，tenant/project 只提供该适配器内的数据分区，不建立 R14 身份、产物 ACL、
秘密轮换或防御直接数据库访问。产物验证由宿主提供，必须保留不可变引用内容。导出本地镜像不撤销旧
所有者，也不授权迁移。全库恢复在接纳写前必须停用旧权威并设置隔离，操作协议见
[R04 恢复](07-shared-recovery.md)。

[认证 HTTPS 服务](03-remote-service.md)在存储上增加传输和双 scheduler/三 worker 所有者丢失实验。
[R09 集群验收](06-cluster-scheduling.md)增加共享配额/背压、worker 排空、续租及有界故障/负载测量。
生产容量与灾难恢复目标仍需具体部署证据。[R14 安全验收](../06-acceptance-and-maintenance/06-security-acceptance.md)定义原始可信端口之上的
认证接纳边界，仅本地/共享存储契约不能证明部署属性。

实现参考：[PostgreSQL 行锁](https://www.postgresql.org/docs/17/explicit-locking.html)与
[Rust PostgreSQL client](https://docs.rs/postgres/0.19.14/postgres/struct.Client.html)。

[认证应用门面](02-authenticated-authority.md)从数据库 bearer 身份推导 scope/actor，并在运行变更同一事务
授权。原始 `PostgresRunStore` 仍属可信 API。旧 schema-10 镜像要求来源绑定、管理员授权的
`plan_storage_upgrade` / `upgrade_storage`；定义迁移是独立的 paused-run CAS，见[R11 迁移](../04-effects-and-recovery/05-version-migration.md)。

<!-- book-navigation -->

5.1 PostgreSQL 权威存储

[全书目录](../README.md) · [5. 共享执行](README.md) · [English](../../en/05-shared-execution/01-postgres-authority.md) · [上一章: 4.5 版本与存储迁移](../04-effects-and-recovery/05-version-migration.md) · [下一章: 5.2 认证与角色](02-authenticated-authority.md)

<!-- /book-navigation -->
