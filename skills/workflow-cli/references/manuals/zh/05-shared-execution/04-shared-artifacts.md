# 经过认证的共享产物传输

HTTPS 服务在 PostgreSQL 中发布和消费类型化产物。远程 worker 可以返回产物证据，run reducer 在结果结算和恢复时验证这些证据。清单身份、内容类型、血缘和生产者契约与[本地产物](../03-execution-and-evidence/05-artifacts.md)一致。

此适配器将字节存放在 PostgreSQL `bytea` 中。[R07](../06-acceptance-and-maintenance/04-artifact-acceptance.md) 增加独立可信的 S3/对象适配器，以及实际工作区/证据执行。[R14](../06-acceptance-and-maintenance/06-security-acceptance.md) 定义认证访问、提供方租约、保留/归档/删除策略和私有审计导出。S3 预签名 URL 不能替代绑定凭证的共享下载协议。

## 初始化与授权

新部署的 `workflow service bootstrap` 会初始化附加的 `workflow_artifacts` schema，版本为 1。已有认证部署需由持有数据库绑定的可信主机执行：

```sh
workflow service init-artifacts server-binding.json
```

对版本 1 重复初始化是幂等的；未知版本会被拒绝。公共请求不能初始化或升级存储。已有只读部署可以在没有此 schema 时运行，但初始化前不能接收依赖产物的结果。应将此 schema 的内容、元数据、凭证、assignment 及 `workflow_authority` 纳入一致的数据库备份。[共享备份/恢复](07-shared-recovery.md) 会验证完整依赖闭包。

worker 凭证中精确指定能力 ID/版本/契约摘要的规则，可以附带 `artifacts` 策略。没有策略时拒绝全部产物访问。以下策略适用于已提交 `repository`、`revision` 和 `source` 输入的能力：

```json
{
  "inputs": {
    "source": {
      "identity": {"id": "build-report", "version": "1.0.0"},
      "content": {"format": "utf8"}
    }
  },
  "output": {
    "types": [
      {
        "identity": {"id": "build-report", "version": "1.0.0"},
        "content": {"format": "utf8"}
      }
    ],
    "repository_input": "repository",
    "revision_input": "revision"
  }
}
```

`source` 必须是已准备任务输入中的 ArtifactLink。服务检查其精确声明类型、tenant/project、当前 run 和完整血缘。worker 读取权限仅覆盖这些根产物及其必需依赖。仅输出的 worker 使用 `"inputs": {}`；没有发布权限的消费者使用 `"output": null`。源码 revision 绑定到已提交输入字段；发布流程不会独立测量 worker 文件系统。

服务自行推导生产者 run/node/attempt、request digest、input digest、run 访问范围、血缘和 `run_dependency` 保留策略。worker 不能自行选择生产者清单，也不能替换为调用方提供的身份。上传的每个阶段都会重新检查凭证、assignment、精确能力契约、当前租约和主数据库时间。已释放/替换的租约、已结算 assignment、过期或撤销凭证都会阻断后续调用，包括相同的完成重试。

## 上传、续传与消费

取得当前 assignment 和主机配置的产物策略后：

```sh
workflow remote artifact-upload worker-binding.json ASSIGNMENT_ID report-request-1 artifact-type.json report.txt
workflow remote artifact-download viewer-binding.json download.json downloaded-report.txt
```

上传命令在本地计算长度/摘要，并输出完成后的 ArtifactRef。request ID 限定在 worker 凭证和 assignment 内。显式重复相同命令并提供完全相同字节，可从已持久化进度续传；改变规格或已保存分块则失败。传输错误直接返回，不盲目自动重放。并发的相同续传可以对齐进度，并验证最终引用。

`download.json` 包含精确产物链接、可选 worker assignment，以及 1–300000 毫秒的 TTL：

```json
{
  "artifact": {"artifact_id": "artifact-<manifest-sha256>", "digest": "sha256:<manifest-sha256>"},
  "assignment_id": null,
  "ttl_ms": 60000
}
```

可读取 run 的角色可访问其认证范围内的产物。worker 必须提交自己的当前 assignment，且声明输入授权该链接。单独的管理员凭证没有读取权限。下载 grant 绑定精确凭证和范围，最晚在凭证/assignment 到期时失效，每个分块都要求 bearer。它不是匿名 URL，复制给另一凭证不会转移访问权。

`RemoteClient::upload_artifact` 和 `download_artifact` 执行相同的完整清单/类型/内容检查。下载命令先验证所有字节，再独占创建 0600 文件，并同步文件及父目录。已有路径会被拒绝。文件系统同步失败时，交付尚未确认，新建的部分文件可能需要清理。文件输入会直接拒绝 FIFO 等非普通文件，不会等待写入者。

底层类型化操作为 `artifact_begin`、`artifact_put`、`artifact_complete`、`artifact_grant`、`artifact_get` 和 `artifact_cleanup`，可通过 `workflow remote call` 调用。未完成上传没有目录引用。完成时验证全部字节和血缘，然后原子提交内容、清单、完成回执及分块清理。应将返回的产物 ID/digest 附加到真实 WorkResult 的 evidence 中。已有结果入口会检查生产者/request/input 绑定，之后才能推进 run；仅上传内容不会完成任务。

提供的[共享报告 fixture](../../../../assets/examples/artifacts/shared-report-start.json) 使用测试主机能力 `fixture.report`。CLI 内置 worker 未注册此能力；HTTPS 集成测试会显式提供对应适配器。

## 上限、清理与恢复

- 单产物：最多 64 MiB；除最后一块外，分块固定为 64 KiB。
- 单 run：最多保留 512 个产物，内容和活动上传预留合计最多 512 MiB；最多 64 个未完成的有效上传和 512 条传输记录。
- 单凭证：最多 1000 个未到期下载 grant。创建 grant 时回收过期项。
- `artifact_cleanup` 对 administrator/recovery 角色开放，限定 tenant/project 和有界条数。它移除过期传输元数据、暂存分块及过期的已完成上传回执，保留清单与内容。[R14](../06-acceptance-and-maintenance/06-security-acceptance.md) 另行定义完整归档/删除保留策略。

reducer 持有 run 锁时加载有界且经过验证的目录。它一次读取一个有界内容对象并保留清单，在接受已结算状态前拒绝损坏字节、缺失引用、不一致类型或无效血缘。读取和修改都承担此扫描成本；这些上限不是生产吞吐量承诺。已保存产物可通过另一服务实例访问，不依赖生产者本地文件。

必跑的 PostgreSQL CI 任务覆盖实际上传/重试/冲突、损坏摘要、具有相同 worker 策略的跨租户写入、复制/过期/撤销 grant、过时租约、类型化输入根及传递依赖、清理和恢复。HTTPS fixture 在独立上传者提交首个分块后终止进程，再通过客户端续传，提交带产物的工作，并验证另一服务实例恢复的状态和内容。Cargo/Bazel 还验证 TLS、协议和文件边界。R07 与 R14 将这些 fixture 与各自的补充验收证据组合使用。

<!-- book-navigation -->

5.4 共享产物

[全书目录](../README.md) · [5. 共享执行](README.md) · [English](../../en/05-shared-execution/04-shared-artifacts.md) · [上一章: 5.3 HTTPS 服务与 worker](03-remote-service.md) · [下一章: 5.5 远程外部操作](05-remote-effects.md)

<!-- /book-navigation -->
