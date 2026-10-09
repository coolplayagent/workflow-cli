# Workflow CLI

[English](README.md) · 中文

Workflow CLI 把业务 SOP 变成可评审、可持久化执行的流程。定义声明步骤、类型化交接、合法决策和
证据要求。Agent 通过一个 skill 操作 CLI；模型提出有界的节点决策，适配器执行实际工作，运行时
保留状态、所有权和经过验证的结果。

阅读 [Workflow CLI 实践指南](docs/zh/README.md)，从安装逐步进入共享部署；也可以打开
[文档站](https://coolplayagent.github.io/workflow-cli/)。[英文版](docs/en/README.md)使用相同章节和示例。

## 安装一个 skill

从 [v0.2.0 发布页](https://github.com/coolplayagent/workflow-cli/releases/tag/v0.2.0)下载
`workflow-cli-skill-v0.2.0-linux-x86_64.tar.gz` 和 `SHA256SUMS`，在下载目录执行：

```sh
sha256sum --check SHA256SUMS
mkdir -p ~/.codex/skills
tar -xzf workflow-cli-skill-v0.2.0-linux-x86_64.tar.gz -C ~/.codex/skills
~/.codex/skills/workflow-cli/scripts/workflow.sh version --format json
```

其他 agent 宿主使用其配置的 skill 目录。压缩包包含匹配的 CLI、[任务参考](skills/workflow-cli/SKILL.md)、
完整中英文手册、示例、schema 和文件摘要，渐进式读取始终可以在解包目录内完成。
当前发布支持 Linux x86_64、Ubuntu 24.04 / glibc 2.39 及更新环境，不提供 macOS、Windows 或 ARM
二进制包。详见[安装与升级](docs/zh/01-getting-started/02-skill-distribution.md)。

## 执行真实的本地示例

演示需要 Python 3，无需编译器、模型账号或远程服务：

```sh
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision approve
```

示例执行两个真实的内置校验任务，提交明确的演示操作员决策，检查历史与备份，然后停止守护进程。
换成 `--decision reject` 可观察取消结果。按[第一个工作流](docs/zh/01-getting-started/03-getting-started.md)逐步检查持久化
进度，并证明第二次 drive 不会重复已经提交的任务。

CLI 命令成功与业务成功分别判断：修改后检查 `result.snapshot.status`，状态查询后检查 `result.status`。
静态 `validate` 返回 `valid` 与诊断，不会执行流程或授予能力调用权限。

## 已实现的能力

- 类型化 JSON/YAML 定义、静态校验、带 revision 的草稿和不可变发布。
- 确定性控制流、检查点回放、状态/事件/outbox/Inbox 事务存储、租约和受所有权约束的结果提交。
- 类型化产物、隔离的尝试级工作区、准确来源、强制证据门禁和有界修复。
- 有界模型策略、明确宿主绑定和决策记录；持久化外部操作、结果核对与有序补偿。
- 本地守护进程、经过验证的备份和显式迁移；经过认证的 HTTPS worker、PostgreSQL 权威存储和共享调度。
- 可评审 SOP 模板，以及本地和 TLS 验收实验。

从[本书阅读路线](docs/zh/01-getting-started/01-preface.md)开始。验收章节说明测试环境与限制；[交付路线图](docs/zh/06-acceptance-and-maintenance/08-roadmap.md)
保留包括业务价值基准在内的未完成工作。只有流程需要模型、业务适配器或共享服务时，才需要配置对应绑定。

## 开发与验证

Cargo 和 Bazel 使用固定工具链构建相同 Rust 源码。在源码仓库运行：

```sh
cargo run --locked -- validate examples/review.yaml
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bazel test //...
bazel run //:workflow -- validate "$PWD/examples/review.yaml"
python3 -m pip install -r website/requirements.txt
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests -v
```

Bazel 在执行目录解析相对输入路径，因此应传入绝对路径。修改 Cargo manifest 或 `Cargo.lock` 后，
用 `bazel mod deps --lockfile_mode=update` 更新并检查 Bazel 锁文件；验证会拒绝过期锁文件。
现有 Qualitygate 策略执行格式、Clippy、Cargo 与 Bazel 检查；CI 还检查文档、解包 skill 和真实共享模式验收。
详见[贡献与诊断](docs/zh/06-acceptance-and-maintenance/07-troubleshooting.md)、[发布验证](docs/zh/01-getting-started/02-skill-distribution.md)。

采用 [MIT 许可证](LICENSE)。
