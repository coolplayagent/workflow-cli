# 经过认证的 PostgreSQL 应用边界

`workflow_runstore_postgres::access::AuthenticatedService` 是可信服务宿主的进程内应用接口，数据库连接
保留在宿主。不可信调用者只提供 bearer 凭据和操作输入，不能提供权威 tenant、project、actor、worker
或 task lease。原始 `PostgresRunStore` 是高权限存储适配器，绝不能作为通用远程 RPC 暴露。

## 凭据与角色

部署代码通过可信 PostgreSQL client 调用 bootstrap，为每个 tenant/project 建立一个初始管理员。
Advisory lock 串行化 bootstrap；作用域存在任何凭据后不再开放，即使所有管理员都已过期/撤销。
持数据库凭据的宿主是信任边界，bootstrap 不是公共 API。

凭据来自 OS 的 256 位随机数，固定 `wf1_` 格式；数据库只存高熵 bearer 的 SHA-256。
`IssuedCredential` 特意不提供序列化且 Debug 脱敏，宿主调用 `expose_secret()` 通过私有通道交付。
TTL 为 1 毫秒至 1 小时，用主数据库时间衡量。

每个凭据有一个不可变角色、tenant/project、actor 和能力允许列表；不同角色需独立凭据，管理员签发
继承自身作用域。

| 角色 | 操作 |
| --- | --- |
| Administrator | 签发、轮换、撤销凭据，查审计/未解决 assignment，清理过期产物传输，预览/应用定义及存储迁移 |
| Definition maintainer | 校验定义、发布不可变 bundle、读取运行 |
| Runner | 校验定义、仅启动已发布 bundle、读取运行、以 CAS 暂停/继续/取消 |
| Viewer | 读取状态、历史、Inbox、waits |
| Approver | 读取运行，提交绑定冻结响应者/评审对象策略的人工决策 |
| SignalSource | 读取运行，向冻结事件策略投递外部事件，不能批准人工等待 |
| Scheduler | 读取运行、取得/释放所有权、推进 timer、分发有权任务/操作 |
| Worker | 获取自己的任务并返回结果，在明确能力策略内发布/读取产物 |
| Recovery | 读取运行/审计/未解决 assignment，为审计核对取得/释放所有权、控制运行、确认既有恢复屏障、清理过期传输 |

发布在任何 start 前，在同一事务接纳 bundle 摘要并冻结流程、能力和策略版本。不同内容返回
`binding_conflict`，相同发布幂等。共享身份计算与排序加锁也适用于运行提交。旧版只有摘要的发布在
重发或启动时建立绑定，已有运行绑定仍为权威。发布允许列表尚不提供共享草稿编辑、撤回或完整注册表历史。

没有通用 `apply(Event)`。Worker 不能提交审批、改定义/运行状态、取得调度所有权或读整个运行。
提交任务成功只返回 revision 与 duplicate。持久化 reducer 检查结果 schema、准确请求摘要、协议、图变更、
当前 lease、attempt 和 prepared task。已提交 assignment 不能再次获取用于执行。

## 事务与撤销

每次操作按凭据摘要认证，对有效凭据行取 `FOR SHARE` 锁，scope 由该行推导；撤销更新同一行。
锁跨越授权、reducer 修改、assignment 写、审计和提交，因此有明确数据库先后关系：

- 已持锁提交可先于撤销完成，撤销等待。
- 撤销提交后，等待认证的操作读到撤销状态并失败；revoke 成功后隔离使用该凭据的后续提交，包括其他服务进程。

操作变化使用 savepoint，应用拒绝回滚试探修改但保留有界拒绝审计。操作/审计写后，最终数据库时间检查
复验 credential、worker、assignment、reducer 和回调窗口。无法确认持久化提交则错误，调用者按既有
幂等语义重试；数据库丢失不能返回试探性成功。

Dispatch 同时锁定目标 worker 凭据，核对作用域及准确能力 ID/version/摘要，在 prepared attempt 同一事务
保存 assignment。Worker 发送 assignment ID 和结果，不发送权威租约。投递与提交把 prepared task 与
执行历史比较，并验证包含 generation/epoch 的当前租约；租约释放或替换后，即使相同结果重试也拒绝。

轮换原子签发新凭据并撤销旧凭据，未决 assignment 仍绑定旧身份。管理员分页检查核对，由受隔离调度
恢复创建新 attempt；轮换不暗中把任务转给新身份。[操作协议](05-remote-effects.md)增加绑定准确目标/
主体/策略的独立 assignment、单次投递与审计手工解决。撤销不能物理停止已在 worker 运行的代码。

## 审计与其他边界

审计含 scope、actor、公开凭据 ID、固定操作名、有界资源标识、固定 outcome 和数据库时间，不复制 token、
token 摘要、请求载荷、原始错误、SQL 或连接串。返回错误是固定消息。未认证尝试和数据库故障不声称有
持久化审计，传输宿主负责有界接入失败指标。最终过期/提交失败可与整个事务一起回滚审计。调用者的
流程内容仍是应用数据，API 不保证识别故意嵌入的任意秘密。

附加 `workflow_access` schema 当前为 2，不改变已有运行镜像，不升级未知 access schema。管理员 DB
访问可以修改表，部署必须限制并配置备份/保留策略；这不是防篡改审计或独立身份提供方。

[HTTPS 服务](03-remote-service.md)以明确 CA 校验、宿主秘密引用、有界接纳及独立 scheduler/worker 暴露接口。
真实 PG 验收在两个 scheduler 和三个 worker 中杀死 owner，恢复旧 worker，并比较本地业务结果。
[R14 验收](../06-acceptance-and-maintenance/06-security-acceptance.md)定义认证执行边界、broker 租约、私有审计导出和归档保留。共享定义迁移
与全作用域数据库恢复独立实现。[R09 集群验收](06-cluster-scheduling.md)记录配额/公平性、路由/排空和故障/
性能证据。企业身份配置与任意第三方程序隔离仍属高权限部署集成。

必需 PG CI 运行本 crate 的 ignored 测试，通过独立连接验证真实锁、跨作用域拒绝、角色分离、完整内置
流程与审批、伪造结果/契约拒绝、轮换/撤销、旧租约及试探修改后过期。测试观察 `pg_stat_activity` 锁
等待再释放屏障，不用睡眠延迟假装证明顺序。

实现参考：[PostgreSQL 行锁](https://www.postgresql.org/docs/17/explicit-locking.html#LOCKING-ROWS)与
[getrandom OS 熵](https://docs.rs/getrandom/0.4.3/getrandom/fn.fill.html)。

## 产物权限

[共享产物传输](04-shared-artifacts.md)在每个准确能力规则上使用可选不可变策略。Worker 按声明类型化输入及
lineage 读取，写入来源由当前 assignment 推导。具备运行读取角色可检查作用域产物。短期 grant 绑定
作用域和凭据，各阶段都参与同一撤销及最终时间检查。验证过的 PG 目录为结果提交和恢复提供证据，增加
独立 v1 存储而不改运行镜像。

Approve/signal 要求 `wait_policies` 绑定；旧发布的无绑定等待仍可读，但不能远程响应。为新运行发布
受保护 bundle，本地可信管理员 Inbox 语义仍支持。Access schema 1 安装先明确执行
`workflow service migrate-access server-binding.json`，再签发 signal_source；新安装直接创建 2。
详见[持久化审批验收](../06-acceptance-and-maintenance/03-approval-acceptance.md)。

<!-- book-navigation -->

5.2 认证与角色

[全书目录](../README.md) · [5. 共享执行](README.md) · [English](../../en/05-shared-execution/02-authenticated-authority.md) · [上一章: 5.1 PostgreSQL 权威存储](01-postgres-authority.md) · [下一章: 5.3 HTTPS 服务与 worker](03-remote-service.md)

<!-- /book-navigation -->
