# R08 本地验收

此矩阵记录受支持的本地 Linux 产品边界，不能替代 R09 集群测试、R10 在线 ownership 交接或 R15 模型价值评估。

## 验收矩阵

| R08 验收项 | 结果检查器 / 观察证据 |
| --- | --- |
| 全新环境，无远程服务，确定性 branch/loop/parallel/人工 wait | `examples/execution/offline-demo.py` 检查两个真实 builtin 结果、显式操作者选择、精确业务终态、回放和服务停止。主机上通过批准/拒绝；全新 Ubuntu 24.04 容器在 `--network none` 下通过批准。 |
| 终止进程后恢复，不重复已提交任务 | `daemon_recovers_parallel_loop_and_branch_work_then_wakes_on_callback_and_timeout` 在持久 wait 处终止/重启 daemon，断言恰好两个原始 Finished 记录，再检查回调/超时终态和回放。已有 RunStore 测试覆盖提交边界内终止。 |
| 两个 CLI resume 不能产生两个合法 owner | `concurrent_cli_resume_and_acquire_have_only_one_cas_winner_and_owner` 为每个竞态启动两个独立 CLI 测试进程，恰好一个 resume CAS 和一个租约获取胜出。 |
| Daemon status/stop；不存在时不虚报 timer 服务 | 实时 Unix IPC、绑定 generation 的 drain 和私有锁；真实终止/SIGSTOP 测试区分 stopped 与 unreachable。旧 stop 请求不能影响重启后的实例。 |
| 移动备份保留产物/历史/wait；确认 effect 不重复 | `workflow-backup-local` 迁移及独立持久提供方测试覆盖完整 artifact/gate/Inbox/registry 数据，以及原 intent 保留/缺失的快照后 effect。query/import 恢复后仍仅一个提供方对象、一次写入。 |
| 磁盘满/缺失备份数据报告错误，不虚报成功 | `sqlite_disk_full_rolls_back_without_confirming_start_or_transition` 通过页数上限触发 SQLITE_FULL 并验证回滚。备份测试制造真实子进程文件配额失败，并删除/损坏必需文件。这些是不同的注入故障模型，不代表任意整卷满场景。 |
| 公布支持平台/文件系统与边界 | Ubuntu 24.04.4、Linux 7.0.0-31-generic x86_64、ext4；独立 Ubuntu 24.04 容器使用同一主机 kernel/ext4 绑定存储。覆盖进程终止、挂起、SQLite 回滚及归档迁移，不覆盖任意断电或无备份磁盘毁坏。 |

被测试二进制使用 Rust 1.97.1，要求兼容 glibc（此主机构建要求 GLIBC_2.39）。较旧 Debian 12 镜像在启动时拒绝该二进制，因此未列为此产物支持平台。成功的 Ubuntu 镜像身份为 `sha256:69f919497cada9a8611f3de61df902d9f94bf20f84552ef5bd93125c38608ef9`。runtime 镜像在禁网执行前准备；该执行不使用云凭证、模型调用或远程数据库。

所有 store 都是显式本地路径。daemon 每次扫描重新加载持久状态，从不移除活动数据。本地保留策略目前为 keep-all，并有显式大小/准入上限；不自动压缩/删除。导出使用经过验证的 archive 格式，携带不可变定义和 effect 身份。恢复会暂停并 fence ownership，写入前要求提供方/源审计；导出不意味着允许同时运行两个活动副本。

原始 gate 报告、独立环境输出、二进制/镜像身份和 review/CI 证据保存在 `.qualitygate/mr-33/` 与 `.qualitygate/mr-34/`（前者位于 backup worktree）。这些是维护者工作区中的历史验收记录。小示例的计时从初始化与二进制准备完成后开始，不是安装耗时、生产吞吐、端到端业务 RTO 或模型收益估计。

<!-- book-navigation -->

[目录](README.md) · [English](../local-acceptance.md) · [上一章: 产物与工作区验收](artifact-acceptance.md) · [下一章: 安全验收](security-acceptance.md)

<!-- /book-navigation -->
