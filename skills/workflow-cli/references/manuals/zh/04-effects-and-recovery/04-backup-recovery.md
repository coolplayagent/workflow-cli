# 本地备份与所有权隔离恢复

`workflow-backups` 定义可移植清单和 reader 契约；`workflow-backup-local` 创建验证过的 SQLite 镜像并
复制不可变产物，两者都有 Cargo/Bazel 目标。发布当前需要 Linux、`/dev/urandom` 以及支持原子
`renameat2(RENAME_NOREPLACE)` 的文件系统。

## 内容与快照边界

备份含 `runs.sqlite`、可选 `registry.sqlite`、可选 `artifacts/catalog.sqlite` 及保留对象，以及
`backup.json`。运行镜像包括冻结定义、版本锁、事件、Inbox、检查点、Outbox/回执、租约、attempt、门禁、
模型记录和操作账本。可选注册表还保留未发布和已删除草稿历史。产物清单保留原所有权、生产者、源码
revision、输入 lineage 和内容摘要。

SQLite backup API 复制固定读取事务，包括已提交 WAL；复制活动数据库文件不是该协议。每个数据库
先做 SQLite integrity/foreign-key 检查，再做应用验证。先捕获运行镜像；产物不可变且保留，因此稍后
产物目录可为超集，但必须包含所有已提交运行依赖。发布前用复制的产物完整回放运行，证明依赖齐全。
注册表独立拍快照，每个运行已包含准确冻结执行定义。

每个文件都有长度与 SHA-256，索引绑定文件清单和各运行身份/revision/状态摘要。多余文件、缺失字节、
摘要错误、不支持版本、历史不一致或符号链接都拒绝。清单路径来自固定布局。摘要只保证完整性，不证明
来源身份或授权。宿主管理源/目标目录，同等 OS 权限的恶意进程替换不在适配器边界内。

新私有 staging 使用 0700/0600。数据和目录条目 sync 后原子发布，拒绝任何既有目标，包括并发新建的
空目录。发布前失败不留下可用目标。中断 staging 不会自动成为备份；只有确认创建进程已停止后才检查/
移除废弃 staging，清理不删除活动运行、已提交产物或完成备份。

格式上限为单 SQLite 镜像 256 MiB、合计 1 GiB、10000 运行、10000 产物清单、10003 载荷文件、8 MiB 索引；
注册表全量验证最多 10000 保留 revision 和 10000 发布。备份只包含选定 store，不包含未提交工作区、
任意仓库文件、环境凭据或宿主适配器配置。

## CLI

创建含实际已有路径的来源文件，路径按当前工作目录解析，与 JSON 文件所在位置无关：

```json
{
  "runs": "/path/to/runs.db",
  "artifacts": "/path/to/artifacts",
  "registry": "/path/to/definitions.db"
}
```

不存在的可选 store 用 null。省略必需产物不会关闭验证，而是使备份被拒绝。

```sh
workflow backup create sources.json /new/backup-directory
workflow backup verify /new/backup-directory
workflow backup inspect /new/backup-directory
workflow schema backup-index
```

Create/verify 返回摘要、计数、字节数和快照时间范围；inspect 返回完整已验证清单。备份不停止源 worker，
捕获运行快照后源仍可继续提交。

恢复请求标明真实本地操作员与 reason，这个注释不认证远程用户：

```json
{"actor":"local-operator","reason":"recover after source storage loss"}
```

```sh
workflow backup restore /new/backup-directory /new/restored-directory restore-request.json
workflow run --artifacts /new/restored-directory/artifacts status /new/restored-directory/runs.sqlite my-run
workflow run --artifacts /new/restored-directory/artifacts recovery /new/restored-directory/runs.sqlite my-run
```

无产物时省略 `--artifacts`。恢复只有在验证并隔离旧所有权后，才发布 runs、可选产物/注册表和
`restore.json`；不覆盖来源，不合并已有数据库。回复丢失时先检查目标运行状态、屏障及报告，再考虑新路径重试。

## 恢复所有权与外部操作

每个恢复运行获得新的随机 ownership generation，绑定备份摘要和准确源快照。日志释放旧租约；新租约
保留递增 epoch 并携带新 generation。即使源在备份后签发的租约 owner/acquisition/epoch/时间恰好相同，
也不能向恢复库提交。历史与重试预算不重置；running 运行恢复后暂停，原已暂停运行保留 reason 和 deadline。

恢复还设置持久化屏障。明确 resume 可推进只读工作、定时器和查询，但有屏障时不能写入。
继续执行不表示外部效果不存在。

| 备份证据 | 恢复处理 |
| --- | --- |
| 有意图，无回执 | 用保留 key/意图经 drive-effects 或宿主 API 查询真实提供方；查无结果不能隔离存活旧 writer，屏障下不允许新写 |
| 源在快照后才接纳操作 | 先审计/静默源，取得原意图和真实回执；`run effect-import <db> <lease.json> <import.json>` 校验冻结任务、输入、策略、key、原回执和依赖顺序，原子记录 Applied 与任务变更 |
| 无法确定原意图或结果 | 保留屏障，继续提供方/来源审计；历史缺失不能证明新写安全 |

`schema run-restored-effect` 导出格式，携带真实 `EffectIntent` 和包含 Applied 回执、稳定 resolution ID、
actor 注释、reason、evidence 的 `ManualResolution`。只有恢复核对期间、有效租约下、下一个 pending
受管理操作才允许导入。不调用提供方，不编造遗失 attempt；导入操作的本地 call 列表为空，明确记录
来源。准确重复幂等，内容改变冲突。不能伪造原时间或回执使导入通过。

实际停用来源并完成提供方审计后，导出 `schema run-recovery-acknowledgement` 再提交：

```sh
workflow run recovery-acknowledge /new/restored-directory/runs.sqlite my-run audit.json
```

审计绑定准确 generation/backup、resolution ID、actor、reason 和 evidence。
`no_missing_effect_intents: true` 是操作员明确声明快照后的每次已接纳写都已核实。已知未解决操作仍拒绝
确认；来源/提供方历史不可用时不能作此声明。响应指出已确认代际及剩余屏障，旧确认不能清除后续恢复
屏障。保留操作不会获得新调用预算；快照后丢失的调用/历史仍未知，不声称其成本或次数。

隔离保护恢复数据库，不能自行停用仍运行的源服务或隔离外部提供方。原子源所有权交接属于独立 R10。
这是带明确核对暂停的灾难恢复，不是同时激活两份副本的许可。

## 存储、证据与恢复目标

Schema 10 引入恢复代际、屏障与导入回执；当前 schema 12 对 1–11 使用显式备份[迁移](05-version-migration.md)，
旧 reader 拒绝新存储。真实 v9 迁移测试准确保留暂停 Inbox 和 prepared 操作的 lease/history。旧记录省略
generation 字段，摘要保持不变。

测试覆盖以下行为：

- 搬迁带门禁产物、已发布/删除定义历史和暂停 pending Inbox，旧门禁完成而不重跑已提交任务。
- 备份后真实提供方写，有/无保留意图两种情况；查询/导入后仍只有一个对象、一次写。
- 备份后相同 epoch/owner/时间的旧租约、重复恢复及旧确认回放。
- 快照/隔离/发布七个进程终止点，真实子进程写配额失败，不发布不完整目标。
- 捕获后源继续提交、目标竞争、缺失/损坏/多余文件、符号链接及重算摘要但历史矛盾的注册表。
- 真实 CLI 备份/恢复，在明确提供方审计前拒绝写入。

捕获的运行快照定义备份 RPO，磁盘故障可能丢失其后数据库提交，外部效果必须按上述步骤核对，不声称
RPO=0。文件验证与隔离恢复建立可恢复的受阻状态；人工/提供方核对时间另计入业务 RTO。测试覆盖进程
终止，不是任意硬件断电。

开发 Linux 主机（16 逻辑 CPU，statfs 标为 ext2/ext3）的五次小 CLI 样本含两个运行、一个 pending 模拟
回调和一个故意未分发意图，无产物或外部调用。包含进程启动/验证/sync 的中位时间分别为：备份 95.5 ms，
验证 43.3 ms，恢复到受阻状态 211.7 ms。这是测试观察，不是生产容量或恢复保证，完整元数据随 MR 证据保留。

共享 PostgreSQL 恢复见[共享恢复与 R04 验收](../05-shared-execution/07-shared-recovery.md)。加密/签名归档分发、部署归档保留和
来源到目标所有权交接仍属独立部署/路线图工作。

<!-- book-navigation -->

4.4 本地备份与恢复

[全书目录](../README.md) · [4. 外部操作与恢复](README.md) · [English](../../en/04-effects-and-recovery/04-backup-recovery.md) · [上一章: 4.3 有序补偿](03-ordered-compensation.md) · [下一章: 4.5 版本与存储迁移](05-version-migration.md)

<!-- /book-navigation -->
