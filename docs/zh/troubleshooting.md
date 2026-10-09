# 故障排查与贡献

排查从观察开始。保留准确命令、退出状态和 JSON 错误，确认 CLI 版本、运行 ID、数据库及绑定路径。
分享日志前去掉秘密。通过 `workflow help` 和 `workflow schema <kind>` 核对当前版本的命令与输入形状。

## 按症状定位

| 症状 | 下一步检查 | 恢复边界 |
| --- | --- | --- |
| 包装脚本找不到兼容运行时 | `VERSION`、系统/架构、`WORKFLOW_BIN`、执行权限 | 安装匹配发布包，不混用 skill 与 CLI 版本 |
| 静态校验失败 | `diagnostics[].path`、节点/边标识、导出的 schema | 修改草稿再校验，此时没有执行运行 |
| drive 退出 0 但业务未完成 | `run status`、待处理命令、等待、暂停和门禁状态 | `idle` 或 `UNKNOWN` 可以是合法等待 |
| 写操作后回复丢失 | 执行历史和持久化操作账本 | 先查询、核对原操作，再决定重试 |
| revision 过期或所有权失效 | 当前 revision 和执行历史 | 获取当前权限，不能伪造 worker 观察 |
| 证据缺失或损坏 | 产物存储/绑定、载荷和清单标识 | 恢复经过验证的字节，不能用伪造 PASS 代替 |
| HTTPS 请求被拒绝 | TLS 信任、凭据作用域/角色、撤销和当前任务分配 | 修复有权修改的宿主配置，保留拒绝审计 |
| 恢复的运行无法写入 | 恢复暂停、服务端观察、来源审计 | 明确完成核对后再恢复执行 |

本地状态可以只查询和回放，不调用适配器：

```sh
workflow run status /absolute/path/runs.db run-id
workflow run execution-history /absolute/path/runs.db run-id 0 100
workflow run verify /absolute/path/runs.db run-id
```

通过 `next_cursor` 继续读取执行历史。运行使用产物时，提供执行期间相同的 `run --artifacts <store>`
配置。普通 SQLite 查询也可能需要写权限以完成崩溃恢复；复制正在使用的数据库文件不等于验证过的
备份。迁移状态前先阅读[备份与恢复](backup-recovery.md)。

## 同步维护双语书籍与离线手册

英文位于 `docs/`，中文位于 `docs/zh/`，文件名相同。`docs/book.json` 是阅读顺序和章节名称的唯一
声明。两种语言都提供完整操作指引；修改契约时，同时更新对应章节和示例。

`python3 scripts/book.py` 更新 Markdown 目录和章节导航，`--check` 只检查不写入。
`python3 scripts/check_docs.py` 检查章节对应关系以及 README、书籍和 skill 的本地链接与标题锚点。
打包前执行回归测试：

```sh
python3 -m pip install -r website/requirements.txt
python3 scripts/book.py --check
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests -v
python3 scripts/build_site.py --output /tmp/workflow-book-preview
```

输出目录必须为空。用 `python3 -m http.server --directory /tmp/workflow-book-preview 8000`
预览站点，检查两种语言、章节切换、移动端导航和片段链接。网页与 Markdown 使用相同章节顺序，
已有平铺文档 URL 继续指向对应英文页面。

## 发布前验证

通过 Cargo 和 Bazel 构建、测试相同 Rust 源码：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bazel test //...
```

修改 Cargo manifest 或 lockfile 后，运行 `bazel mod deps --lockfile_mode=update`，检查
`MODULE.bazel.lock` 的变化再执行验证。共享执行验收还需要 CI 配置的真实 PostgreSQL/TLS 测试环境；
跳过这些用例的本地测试不能证明共享行为正确。

按[发布打包](skill-distribution.md)在源码目录之外构建、运行真实压缩包。发布工作流要求 main 分支上
准确提交的 CI 成功后，才能发布对应 tag。发布后重新下载资源，校验 SHA256SUMS，再跑解包验证，并
检查线上书籍的版本和提交信息。本地包通过并不能单独证明线上发布资源与文档部署正确。

<!-- book-navigation -->

[目录](README.md) · [English](../troubleshooting.md) · [上一章: 安全验收](security-acceptance.md) · [下一章: 架构与交付路线图](roadmap.md)

<!-- /book-navigation -->
