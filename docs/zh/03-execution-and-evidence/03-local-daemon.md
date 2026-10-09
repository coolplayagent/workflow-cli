# 本地无人值守执行

`workflow daemon serve` 是可选的 Linux 前台服务，反复扫描前台 `run drive` 使用的同一 SQLite 运行，
调用注册适配器，推进到期的绝对 wait/loop deadline。内置本地能力不需要云账号、队列或外部数据库；
模型和 HTTP 外部操作节点仍需配置相应网络服务。

## 启动、检查与停止

先初始化目标运行数据库，再保存配置，把路径换成已有数据库和新的私有控制目录：

```json
{
  "schema_version": 1,
  "database": "/path/to/runs.db",
  "control_directory": "/tmp/my-workflow-control",
  "artifacts": null,
  "model_bindings": null,
  "effect_bindings": null,
  "poll_interval_ms": 250,
  "error_backoff_ms": 1000
}
```

相对路径按启动进程当前目录解析。带证据运行需配置 `artifacts`。模型/操作文件与 `run drive-models`/
`drive-effects` 使用相同绑定；启动前一次性读取并验证，内容参与配置摘要。互不相关的模型策略可共用
宿主绑定列表，但运行里的每个策略必须匹配准确绑定。改文件需重启；秘密值仍由适配器从环境读取。

```sh
workflow daemon serve daemon.json
# 在另一个终端：
workflow daemon status /tmp/my-workflow-control
workflow daemon stop /tmp/my-workflow-control
workflow daemon status /tmp/my-workflow-control
```

`serve` 留在前台，第一行 JSON 报告启动，最后一行报告正常终止。若要跨登录/退出保持服务，使用 OS
服务管理器。前台 `run drive` 仍在有界工作后退出，不会安装定时器服务。运行持久化状态与服务可用性
独立，查询准确控制目录才能知道无人值守调度是否可用。

| 观察 | 含义 |
| --- | --- |
| `responsive`，phase 为 `polling` | 控制监听器回复；还需检查 `last_scan_unix_ms` 和诊断确认调度进度 |
| `responsive`，phase 为 `busy` | 已接纳一次 drive，`active_run` 标识它；其他定时器可能等待该同步调用 |
| `responsive`，phase 为 `draining` | 停止已接纳，不再接纳新 drive，原 drive 可以完成 |
| `unreachable` | 所有权锁仍被持有，但控制通道未在 deadline 内回复；调度进度未知 |
| `stopped` | 未观察到响应服务或持有的锁；不保证调度存在 |

这是查询时观察，不承诺未来存活。控制监听器活着不代表数据库健康。有界诊断环包含错误码和运行 ID，
不包含提供方响应体、输入或秘密。`completed_drives` 统计 drive 尝试，包括拒绝，不是业务交付指标；
`last_completed_run` 只说明该次尝试已返回。

Stop 先查询实例，再发送绑定代际的请求，旧请求不能停止替代服务。`stop_requested: true, stopped: false`
表示接受排空；继续查询至 stopped 才确认终止。此前已接纳的 drive 最多处理一个命令/提供方调用。
适配器必须遵守 deadline；卡住的同步调用不会被强杀或谎称已排空。OS supervisor 可以终止它，随后通过
租约和外部操作查询恢复遗留任务。不要对旧状态文件中的 PID 发信号。

## 所有权与调度

控制目录必须由本地用户拥有且禁止组/其他用户访问（0700）。锁与 socket 私有，路径组成必须是真实目录，
Unix socket 路径应短。OS 排他锁防止两个监听器共用控制目录；只有新持锁者能移除旧 socket，端点处的
普通文件和符号链接都拒绝。这是可信本地主机边界，不是远程认证或租户隔离。

两个不同控制目录的服务可以指向同一数据库，实际执行权限仍由事务运行租约决定，仅一个租约有效。
Busy 租约或旧结果只是诊断，不能作为调用 grant。暂停停止新工作和时间推进，继续保留原 deadline。
Daemon 不会清除恢复核对屏障。

每次扫描最多读 100 个运行，下次扫描续接游标；每个合格运行每次访问最多处理一个命令。空闲等待不会
反复取得租约，后续扫描检查持久化 deadline。回调经可信持久化 Inbox 接入，无需内存通知就能发现后继
工作。错误和不确定效果使用有界重试延迟，revision 变化则立即重新考虑。这是本地轮转扫描，不是集群
配额/公平性或并行 worker 容量。

数据库不可用阻止接纳/提交并暴露错误。进程挂起后，下次扫描观察真实时间及到期定时器，不延长 deadline。
若挂起时仍持 SQLite 事务锁，其他查询可能返回 Busy；恢复依旧验证状态，不伪装查询成功。
服务运行期间保留数据库/产物路径，搬迁时先停止并使用验证过的备份/恢复。

## 可复现离线示例

先构建，再任选一个明确本地操作员决策：

```sh
cargo build --locked -p workflow-cli
python3 examples/execution/offline-demo.py --workflow target/debug/workflow --decision approve
python3 examples/execution/offline-demo.py --workflow target/debug/workflow --decision reject
```

示例创建新临时数据库并启动真实 daemon。不可变 bundle 使用真实 `workflow.validate`、exclusive 分支、
有界循环、子流程、带 all-join 的并行控制流和明确人工 wait。本地适配器按顺序调用；循环首轮成功，
重试/耗尽另由内核测试覆盖。脚本只提交选择的本地操作员响应，检查两个真实已提交任务结果、终态、
回放证明和停止状态。不调用模型，也不声称模型质量或生产吞吐量。

## 导出

`run --artifacts <store> export <db> <new-directory>` 使用既有备份格式导出整个选定运行数据库与验证过的
产物依赖；没有依赖可省略 `--artifacts`。包含不可变 bundle、版本、等待、外部操作与来源；草稿注册表
历史需要给 `backup create` 明确注册表来源。导出不授权第二个活动所有者。恢复仍隔离租约并启用外部
操作恢复屏障；受控制的在线迁移属于独立 R10 范围。

## 故障边界与保留

进程测试强制杀死/重启、跨 deadline 挂起/继续、两个 CLI 的 resume/acquire 竞争、旧 stop 和监听器争用。
暂停回调恢复、事务终止点、SQLite-full 回滚、备份缺失/损坏和恢复后的提供方操作核对已有独立测试。
路径迁移、RPO 和明确外部操作审计见[备份恢复](../04-effects-and-recovery/04-backup-recovery.md)，进程重启无法恢复没有备份的丢失磁盘。

本地策略保留全部已接纳运行/事件/操作/Inbox 历史、不可变定义、已提交产物及备份，没有自动删除、检查点
截断或压缩命令。大小和事件上限通过拒绝新接纳处理，不悄悄丢弃活动恢复数据。删除废弃私有备份临时
目录前先停止拥有者；不能仅凭时间过去就认定废弃。超出该保守策略的保留与压缩仍是独立 R04 工作。

已验证宿主为 Ubuntu 24.04.4、Linux 7.0.0-31-generic x86_64、ext4。另一个禁网、只读根文件系统、移除
全部 Linux capabilities、启用 no-new-privileges 的 Ubuntu 24.04 容器，也通过真实 CLI/daemon/审批示例。
它使用宿主 ext4 上新建的可写绑定目录，并共享宿主内核，不是另一台物理机；镜像身份和原始测量随 MR
证据保留。支持的本地目标为具有兼容 glibc 的 Linux x86_64；宿主构建二进制不兼容 Debian 12 的旧 glibc。
本地进程故障测试不证明断电持久性、网络文件系统安全、硬件灾难恢复或多机 SQLite 部署。

准确 checker 和平台限制见 [R08 验收矩阵](../06-acceptance-and-maintenance/05-local-acceptance.md)。

<!-- book-navigation -->

3.3 本地守护进程

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/03-local-daemon.md) · [上一章: 3.2 本地执行](02-local-execution.md) · [下一章: 3.4 有界模型执行](04-model-execution.md)

<!-- /book-navigation -->
