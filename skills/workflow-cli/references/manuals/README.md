# Documentation Bookshelf / 文档书架

[English book](en/README.md) · [中文实践指南](zh/README.md)

Both editions contain the same 6 numbered volumes and 37 chapters. Start with volume 01, then follow the chapter navigation.

中英文版采用相同的 6 卷、37 章结构。从第 01 卷开始，按每章的前后导航阅读。

| Volume / 分卷 | English | 中文 |
| --- | --- | --- |
| `01-getting-started` | [Getting started](en/01-getting-started/README.md) | [起步](zh/01-getting-started/README.md) |
| `02-process-definition` | [Define the process](en/02-process-definition/README.md) | [定义流程](zh/02-process-definition/README.md) |
| `03-execution-and-evidence` | [Execute and verify](en/03-execution-and-evidence/README.md) | [执行与验证](zh/03-execution-and-evidence/README.md) |
| `04-effects-and-recovery` | [Effects and recovery](en/04-effects-and-recovery/README.md) | [外部操作与恢复](zh/04-effects-and-recovery/README.md) |
| `05-shared-execution` | [Shared execution](en/05-shared-execution/README.md) | [共享执行](zh/05-shared-execution/README.md) |
| `06-acceptance-and-maintenance` | [Acceptance and maintenance](en/06-acceptance-and-maintenance/README.md) | [验收与维护](zh/06-acceptance-and-maintenance/README.md) |

## Structure and numbering / 目录与编号

- Each language lives in `en/` or `zh/`, with matching `NN-topic/NN-chapter.md` paths.
- Every directory containing Markdown has a `README.md` cover and index (chapter 0).
- Volume and chapter prefixes use two digits, start at 01 and follow reading dependencies.
- Use lowercase kebab-case; keep English and Chinese chapter numbers identical.
- `book.json` is the reading-order source. Run `python3 scripts/book.py` from the repository root after edits.

每个语言目录及分卷都有 `README.md` 封面与目录（第 0 章）。分卷和章文件使用从 01 开始的两位编号，按阅读依赖顺序排列；中英文保持同一编号。名称使用小写连字符。修改 `book.json` 后，在仓库根目录运行 `python3 scripts/book.py` 同步全部目录与导航。
