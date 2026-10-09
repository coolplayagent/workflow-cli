# 尝试级工作区与输出捕获

`workflow-workspaces` 定义可移植的分配、观察和输出契约，`workflow-workspace-local` 以 Git 对象读取、
独立文件及不可变 SQLite 分配目录实现 Linux 适配器。两者都有明确 Bazel `rust_library`，与内核、worker
和产物契约分离。

每次分配绑定一个运行/节点实例/attempt、准确 worker 请求与输入摘要、固定源码提交、输入产物引用、
声明的类型化输出及明确合并策略。集成的 `run drive-workspaces` 绑定真实持久化领取，分配工作区、验证
输入字节并捕获类型化报告。独立分配仍是可信宿主基础能力，见 [R07 验收](../06-acceptance-and-maintenance/04-artifact-acceptance.md)。

## 运行真实示例

```sh
cargo build --locked --bin workflow
python3 examples/workspaces/validate-isolated.py "$PWD/target/debug/workflow" /tmp/new-workspace-demo
```

使用新目录。示例从已提交输入样例创建自己的 Git 仓库，启动带门禁运行并领取真实 worker 请求，分配
工作区并证明定义文件等于请求的不可变内联输入。随后调用真实校验能力，把类型化报告写到工作区、
捕获报告，再在租约下提交报告与输出清单。两个后置条件都通过，不重复调用 worker。

然后修改一个文件，观察树摘要变化，再移动工作区 store，确认可移植引用和观察仍相同。调用者仓库只读。
保存请求、引用、捕获、观察及运行验证记录，展示完整流程。

## CLI 与来源绑定

```sh
workflow workspace init /path/to/workspaces
workflow workspace prepare request.json source.json input-refs.json outputs.json
workflow workspace checkout /path/to/workspaces repository-id /path/to/repository spec.json
workflow workspace show /path/to/workspaces <workspace-id>
workflow workspace path /path/to/workspaces <workspace-id>
workflow workspace observe /path/to/workspaces <workspace-id>
workflow workspace verify-clean /path/to/workspaces <workspace-id>
workflow workspace capture /path/to/workspaces <workspace-id> /path/to/artifacts
workflow workspace cleanup-orphans /path/to/workspaces
workflow schema workspace-checkout
workflow schema workspace-ref
workflow schema workspace-observation
workflow schema workspace-output
```

输入是原始 JSON；把 prepare 的 `result` 保存为 checkout spec。`source.json` 包含 `repository` 与完整
小写 Git commit `revision`，`input-refs.json` 是准确产物引用数组，`outputs.json` 声明准确路径与完整类型：

```json
[{"path":"report.txt","artifact_type":{"identity":{"id":"example.report","version":"1.0.0"},"content":{"format":"utf8"}}}]
```

Prepare 绑定提供的流程请求，宿主分发前仍需确认它是当前租约下真实 prepared attempt。CLI 的仓库 ID/
路径对是宿主映射，适配器不能证明仓库名称所有权或认证远程来源。Spec 冻结输入产物，capture 发布依赖
输出前通过产物 store 验证。Checkout 当前只物化 Git 文件，不下载输入产物。

Git 适配器通过 [`git cat-file --batch`](https://git-scm.com/docs/git-cat-file) 读取 commit/tree/blob，验证
对象摘要并自行遍历提交树。支持完整 SHA-1、SHA-256 commit ID，拒绝缩写、tag 和非 commit 对象。
不使用源码工作区脏文件或 index，禁用替代对象、懒惰网络获取及 Git 协议。不从文件名拼 shell 命令，
不执行 checkout hook、smudge/text 转换或 attribute filter。Git 程序与本地对象库映射是宿主工具。

只支持普通 Git 文件及可执行位。符号链接、子模块、非 UTF-8 路径、路径穿越、`.git` 条目、控制字符与
不支持的可移植路径形状都会拒绝整次分配，不会悄悄漏文件。工作区是已提交文件导出，没有 `.git`。
需要 Git 元数据的工具应使用其他工作区适配器或明确宿主绑定。

## 标识与并行工作

工作区 ID 由运行、节点实例和 attempt ID 推导。清单摘要还绑定请求/输入摘要、源码 revision、输入产物、
输出声明、基线文件、Git tree 与观察环境（OS、架构、Git 版本）。文件条目含路径、长度、SHA-256 与
可执行位。`workspace://<id>` 不含平台路径，`workspace path` 单独解析宿主目录。

每次分配独立创建文件，不同 attempt 不共享可写 inode、源码 worktree 或 index。同一 attempt 改契约冲突；
准确重试返回原分配并保留所有编辑，回复丢失后也如此，不重置工作目录。当前策略为 `merge_policy: explicit`，
编辑只是候选，直到独立授权合并产生新 revision 与新证据。Seal/plan/apply 产生独立经过验证的 Git revision，
从不写回源码仓库。

目录分离不是 OS 进程沙箱。宿主执行任意命令时必须另行约束文件、网络、凭据和共享资源；可写产物/目录
数据库仍应由宿主管理。此适配器不锁定工作区之外资源，也不证明随后外部命令的环境。

## 观察与捕获

Observe 通过相对描述符、不跟随链接的操作读取全部普通文件，包括 ignored/untracked 文件和可执行位变化。
拒绝符号链接、特殊文件和共享硬链接。两次扫描必须一致，单文件读取期间元数据不能变化。输出包含完整
文件列表、树摘要、新增/修改/删除路径及是否声明输出，是确定性内容数据，不带权限 token 或时间戳。

`clean` 表示整棵观察树等于分配基线，生成报告也算变化。Observe 完成检查退出 0，工作区脏也如此；
verify-clean 在脏时退出 1。两者都不推进运行，不豁免输出路径，也不授权写入。宿主用于决策时必须将观察
绑定到当前时间和目标动作。

Capture 只读声明的交付路径，检查准确类型并发布保留产物。文件产物保留原输入依赖；输出清单含工作区
ID/摘要、基线/观察树摘要、cleanliness、各路径/可执行位与准确产物引用。直接 lineage 包含输入和捕获
文件。源码 revision 仍是分配基线，修改文件不会虚构新 Git commit。

读取输出后重新扫描；树变化、输出缺失、类型错误、输入依赖无效或存储错误都不返回捕获成功。失败可能
留下先前已发布的文件产物，它们继续保留；最终输出清单仅在校验及响应预算检查后发布。后续工作文件变化
不改变不可变捕获。必须在结果提交前附上真实文件/清单引用；提交后发布不能追加附件或追溯成为合格证据。

文件系统观察与外部动作不是原子 CAS。宿主在捕获期间应串行化配合的 writer；恶意/不配合进程可在读后
修改，相同两次扫描也不能证明中途没变。集成执行器在提交前验证完整来源观察并捕获输出，门禁按冻结
来源/输入消费不可变产物。可变工作目录不是发布目标，需先 seal/merge 到新 revision，再重新验证。

## 持久性、保留与预算

发布创建私有 staging，写入并 sync 各文件、验证基线，再不覆盖地 rename 文件树，之后才在一个 SQLite
事务提交不可变清单和目录完整性头。提交前不返回分配。崩溃留下未登记 staging/发布树，或完整分配。
重开验证全部目录条目及链，不把单独目录视为成功。完整性头与条目来自同一快照，避免并发分配混合代际。

`cleanup-orphans` 与分配/capture 共用目录写锁，只删除自身 staging 和不在已提交目录中的树，保留已提交
工作区与编辑。目录条目非法或目录数据库损坏时停止，不是保留期删除 API。

| 预算 | 上限 |
| --- | --- |
| Spec/reference/CLI JSON | 2 MiB |
| 文件 / 目录 | 各 4096，含根目录 |
| 单文件 / 全树 | 64 MiB / 256 MiB |
| 相对路径 | 1024 UTF-8 字节，32 段，每段 255 字节 |
| 输入产物 / 声明输出 | 64 / 64 |
| 分配数 / 目录文档 | 1000 / 64 MiB |
| Git commit / tree 元数据 | 1 MiB / 4 MiB |
| 单 Git 命令 / stderr | 60 秒 / 16 KiB |

适配器需要 Linux、`/proc`、Git 及支持所用描述符/sync/rename 的文件系统。工作区目录 schema 1 与运行
schema 12、产物目录 schema 1 独立。打开缺失 store 不初始化，外来 schema/目录拒绝；失败不会声称分配/
捕获成功。

Cargo/Bazel 测试覆盖真实 Git 对象、脏源码、replacement ref/filter、对象损坏、SHA-256 仓库、路径/类型/
预算拒绝、隔离、幂等重试、修改/删除/模式检测、不安全文件类型、捕获 lineage 与上游损坏、SQLite 容量
耗尽、独立进程竞争、并发目录快照及发布/提交前后终止。这证明受测确定性与进程故障行为，不证明断电
恢复、生产吞吐量或业务收益。[R07 验收](../06-acceptance-and-maintenance/04-artifact-acceptance.md)描述自动执行绑定、评审合并/重验、当前
证据失效和共享对象存储。[R14 安全](../06-acceptance-and-maintenance/06-security-acceptance.md)说明秘密/工具控制及任意进程沙箱边界。

<!-- book-navigation -->

3.6 尝试级工作区

[全书目录](../README.md) · [3. 执行与验证](README.md) · [English](../../en/03-execution-and-evidence/06-workspaces.md) · [上一章: 3.5 产物与来源](05-artifacts.md) · [下一章: 3.7 证据门禁](07-evidence-gates.md)

<!-- /book-navigation -->
