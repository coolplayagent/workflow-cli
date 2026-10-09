# 第一个持久化工作流

审批示例通过真实的内置校验器检查流程定义，等待明确的操作员决策，然后验证历史与备份。
用它练习区分“命令完成”和“业务流程成功”。请先完成[安装](skill-distribution.md)；
演示驱动程序需要 Python 3。

## 执行两种业务结果

在受支持的 Linux 主机上，可以从任意工作目录运行：

```sh
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision approve
python3 ~/.codex/skills/workflow-cli/assets/examples/execution/offline-demo.py \
  --workflow ~/.codex/skills/workflow-cli/scripts/workflow.sh --decision reject
```

如果安装位置不同，请替换 skill 路径。每次调用都会创建独立的临时存储和守护进程，执行校验器，
提交选定的演示决策，验证备份并停止守护进程。`approve` 达到业务成功，`reject` 达到取消终态。
两者都不调用模型或外部业务服务。这里提供的决策只属于一次性演示；真实审批必须由有权操作员给出。

## 逐步检查一个小型运行

需要保留状态进行练习时，先创建新目录，在完成历史检查前保留它。下面的 shell 函数通过发布包的
包装脚本调用运行时，不会改变当前工作目录：

```sh
workflow_skill="$HOME/.codex/skills/workflow-cli"
workflow() { "$workflow_skill/scripts/workflow.sh" "$@"; }
workflow_demo=$(mktemp -d)
workflow run init "$workflow_demo/runs.db"
workflow run start "$workflow_demo/runs.db" \
  "$workflow_skill/assets/examples/execution/valid-start.json"
workflow run drive "$workflow_demo/runs.db" inspect-valid learner 10
workflow run status "$workflow_demo/runs.db" inspect-valid
workflow run execution-history "$workflow_demo/runs.db" inspect-valid 0 100
workflow run verify "$workflow_demo/runs.db" inspect-valid
workflow run drive "$workflow_demo/runs.db" inspect-valid learner 10
```

第一次 drive 执行一个校验任务，修改响应中的 `result.snapshot.status` 为 `succeeded`；
独立的 status 查询使用 `result.status`。最后一次 drive 执行零个任务，因为先前结果已经提交。
`verify` 根据保留事实回放，不会再次调用校验器。继续练习时保留数据库路径：CLI 没有隐式默认运行存储。

接着在同一数据库中启动 `assets/examples/execution/invalid-start.json`，对 `inspect-invalid`
执行 drive。校验器返回格式正确但 `valid=false` 的观察，流程选择 `failed` 终态。
CLI 退出码 0 不会把该业务结果变成成功。

## 修改定义，再做校验

阅读随包提供的[审批定义](../../examples/review.yaml)，编辑前先复制到工作目录，安装目录中的示例
属于固定版本输入。执行 `workflow validate <你的文件>`，检查 `valid`、`diagnostics` 和 `digest`。
让一条边指向不存在的节点，会得到字段级诊断且没有摘要；修正后再次校验。静态校验不会启动运行或
授予审批。下一篇将介绍类型化交接和不可变发布。

## 检查理解

现在应能说出运行 ID、准确的业务终态、新执行任务数，以及证明第二次 drive 行为的保留证据。
如果观察不同，请按[故障排查](troubleshooting.md)收集完整 CLI 错误、运行状态和执行历史，再决定重试。

<!-- book-navigation -->

[目录](README.md) · [English](../getting-started.md) · [上一章: 安装与发布 skill](skill-distribution.md) · [下一章: 流程定义与类型](definition-semantics.md)

<!-- /book-navigation -->
