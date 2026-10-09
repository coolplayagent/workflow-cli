# 安装与发布 workflow skill

一个 `workflow-cli` skill 提供完整 CLI 入口。发布包包含任务参考、完整中英文书籍、示例、schema、
文件摘要清单和匹配的可执行程序。内部 Rust crate 仍是实现模块。

## 安装

从[发布页](https://github.com/coolplayagent/workflow-cli/releases/latest)下载
`workflow-cli-skill-v0.2.0-linux-x86_64.tar.gz` 和 `SHA256SUMS`，在下载目录执行：

```sh
sha256sum --check SHA256SUMS
mkdir -p ~/.codex/skills
tar -xzf workflow-cli-skill-v0.2.0-linux-x86_64.tar.gz -C ~/.codex/skills
~/.codex/skills/workflow-cli/scripts/workflow.sh version --format json
```

其他 agent 宿主使用其配置的 skill 目录。升级前保留原安装目录和 CLI，并检查存储兼容性。
包装脚本自行定位安装目录，保持任务当前工作目录不变；优先使用随包二进制，也允许通过
`WORKFLOW_BIN` 明确选择。只有 CLI 版本与 skill 完全一致时，才接受 PATH 回退。

完整发布包支持 Linux x86_64、Ubuntu 24.04 / glibc 2.39 及更新环境。工作区功能还需要 Git 与
`/proc`，演示程序需要 Python 3。本地内置能力执行无需编译器、模型账号或远程数据库。
本版不提供 macOS、Windows 和 Linux ARM 安装包。

## 运行本地工作流

```sh
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision approve
```

演示使用临时存储，执行两个真实的内置校验任务，提交明确的演示操作员决策，验证历史与备份，
最后停止本地守护进程。换成 `--decision reject` 可观察取消结果，不会调用模型或外部业务服务。
其他测试适配器和集成测试可能需要源码仓库或测试服务，运行前应阅读相应前提。

原先的八个 skill 已合并为单入口下的任务参考。内置描述符保留 `workflow-capability@1.0.0`
以维持已有摘要；其用法对应能力参考，无需安装第二个 skill。文档重组不会改变存储中的契约。

## 构建并验证发布包

在源码仓库运行：

```sh
python3 -m pip install -r website/requirements.txt
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests -v
cargo build --release --locked -p workflow-cli
python3 scripts/package_skill.py --binary target/release/workflow --output dist --tag v0.2.0
python3 scripts/verify_skill.py dist/workflow-cli-skill-v0.2.0-linux-x86_64.tar.gz
```

验证器解压到含空格的临时路径，检查全部文件摘要、递归本地 Markdown 链接和标题片段，然后从
无关工作目录运行 schema/定义校验、真实成功与失败流程，证明已提交任务不会再次执行，并检查
守护进程的批准/拒绝、备份与运行时解析。它不使用源码仓库的 CLI 或资源路径。打包拒绝版本、
tag、平台不匹配、缺失本地资源或覆盖已有压缩包。

源码 skill 已包含 `references/manuals/`、`assets/examples/` 和 `assets/schemas/`，
手册、语言切换、示例和 schema 的所有本地链接均留在 skill 目录内。打包只复制这些现有资源，
无需读取仓库文档。维护者运行 `python3 scripts/sync_skill_resources.py` 同步提交资源，CI 检查
副本是否过期。缺失资源不能用 GitHub URL 替代。`docs/book.json`
声明唯一阅读顺序；Markdown 与 Pages 都提供章节导航和语言切换。旧版平铺 Pages URL 跳转到
对应英文页面，并保留片段锚点。

`Release skill and Pages` 工作流要求 main 分支上准确提交的 `CI` push 运行成功；通过后再推送
匹配的 `v*` tag。它在 Ubuntu 24.04 构建、打包并验证解压后的 skill，把压缩包和 SHA256SUMS
发布到 GitHub Releases，再通过官方 Pages artifact/deployment action 部署文档。
仓库 Pages 来源必须设置为 **GitHub Actions**。

发布包的 `manifest.json` 记录 `source_revision` 和各文件摘要。发布变化后的资源时，skill 与 CLI
版本必须一起递增。定义、能力、策略与数据库 schema 有各自独立的版本契约。替换二进制不会升级
数据库；需要升级时遵循[版本迁移](../04-effects-and-recovery/05-version-migration.md)。

<!-- book-navigation -->

1.2 安装与发布 skill

[全书目录](../README.md) · [1. 起步](README.md) · [English](../../en/01-getting-started/02-skill-distribution.md) · [上一章: 1.1 如何阅读本书](01-preface.md) · [下一章: 1.3 第一个持久化工作流](03-getting-started.md)

<!-- /book-navigation -->
