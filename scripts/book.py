"""One numbered reading order for Markdown, Pages and the offline skill manual."""
import json
import os
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
NAV_START = '<!-- book-navigation -->'
NAV_END = '<!-- /book-navigation -->'
SLUG = r'[a-z][a-z0-9]*(?:-[a-z0-9]+)*'


def load_book(root=ROOT):
    book = json.loads((root / 'docs/book.json').read_text())
    names, aliases = set(), set()
    if not 1 <= len(book['parts']) <= 99:
        raise ValueError('Book must contain numbered parts')
    for number, part in enumerate(book['parts'], 1):
        directory = part['directory']
        if not re.fullmatch(f'{number:02}-{SLUG}', directory) or not 1 <= len(part['chapters']) <= 99:
            raise ValueError(f'Invalid part numbering or empty part: {directory}')
        for index, chapter in enumerate(part['chapters'], 1):
            name, legacy = chapter['file'], chapter['legacy']
            if not re.fullmatch(f'{directory}/{index:02}-{SLUG}\\.md', name) or name in names:
                raise ValueError(f'Invalid chapter numbering or duplicate path: {name}')
            if not re.fullmatch(SLUG, legacy) or legacy in aliases or legacy in ('index', 'overview'):
                raise ValueError(f'Invalid or duplicate legacy chapter: {legacy}')
            for lang in ('en', 'zh'):
                if not all(item[lang].strip() for item in (book['title'], part['title'], part['intro'], chapter['title'])):
                    raise ValueError(f'Missing {lang} book label: {name}')
            names.add(name)
            aliases.add(legacy)
    return book


def chapters(book):
    return [chapter for part in book['parts'] for chapter in part['chapters']]


def edition_path(root, lang, name):
    return root / 'docs' / lang / name


def chapter_label(part_number, chapter_number, chapter, lang):
    return f'{part_number}.{chapter_number} {chapter["title"][lang]}'


def navigation(book, lang, index):
    ordered = [(p, n, part, chapter) for p, part in enumerate(book['parts'], 1)
               for n, chapter in enumerate(part['chapters'], 1)]
    p, n, part, chapter = ordered[index]
    source = Path(lang) / chapter['file']

    def link(label, target):
        return f'[{label}]({os.path.relpath(target, source.parent)})'

    other = 'en' if lang == 'zh' else 'zh'
    links = [link('全书目录' if lang == 'zh' else 'Book contents', Path(lang) / 'README.md'),
             link(f'{p}. {part["title"][lang]}', Path(lang) / part['directory'] / 'README.md'),
             link('English' if lang == 'zh' else '中文', Path(other) / chapter['file'])]
    for delta, labels in [(-1, ('Previous', '上一章')), (1, ('Next', '下一章'))]:
        if 0 <= index + delta < len(ordered):
            adjacent_p, adjacent_n, _, adjacent = ordered[index + delta]
            label = labels[lang == 'zh'] + ': ' + chapter_label(adjacent_p, adjacent_n, adjacent, lang)
            links.append(link(label, Path(lang) / adjacent['file']))
    return (NAV_START + '\n\n' + chapter_label(p, n, chapter, lang) + '\n\n' +
            ' · '.join(links) + '\n\n' + NAV_END)


def bookshelf(book):
    text = ('# Documentation Bookshelf / 文档书架\n\n'
            '[English book](en/README.md) · [中文实践指南](zh/README.md)\n\n'
            f'Both editions contain the same {len(book["parts"])} numbered volumes and {len(chapters(book))} chapters. '
            'Start with volume 01, then follow the chapter navigation.\n\n'
            f'中英文版采用相同的 {len(book["parts"])} 卷、{len(chapters(book))} 章结构。从第 01 卷开始，按每章的前后导航阅读。\n\n'
            '| Volume / 分卷 | English | 中文 |\n| --- | --- | --- |\n')
    for part in book['parts']:
        directory = part['directory']
        text += f'| `{directory}` | [{part["title"]["en"]}](en/{directory}/README.md) | [{part["title"]["zh"]}](zh/{directory}/README.md) |\n'
    return text + ('\n## Structure and numbering / 目录与编号\n\n'
                   '- Each language lives in `en/` or `zh/`, with matching `NN-topic/NN-chapter.md` paths.\n'
                   '- Every directory containing Markdown has a `README.md` cover and index (chapter 0).\n'
                   '- Volume and chapter prefixes use two digits, start at 01 and follow reading dependencies.\n'
                   '- Use lowercase kebab-case; keep English and Chinese chapter numbers identical.\n'
                   '- `book.json` is the reading-order source. Run `python3 scripts/book.py` from the repository root after edits.\n\n'
                   '每个语言目录及分卷都有 `README.md` 封面与目录（第 0 章）。分卷和章文件使用从 01 开始的两位编号，'
                   '按阅读依赖顺序排列；中英文保持同一编号。名称使用小写连字符。'
                   '修改 `book.json` 后，在仓库根目录运行 `python3 scripts/book.py` 同步全部目录与导航。\n')


def contents(book, lang, part_index=None):
    other = 'en' if lang == 'zh' else 'zh'
    switch = 'English' if lang == 'zh' else '中文'
    if part_index is None:
        text = f'# {book["title"][lang]}\n\n'
        text += f'[{switch}](../{other}/README.md) · [Documentation Bookshelf / 文档书架](../README.md)\n\n'
        overview = 'README.zh-CN.md' if lang == 'zh' else 'README.md'
        text += f'[{"项目介绍" if lang == "zh" else "Project overview"}](../../{overview})\n\n'
        text += ('从第 01 卷开始安装并运行本地审批流程，再依次学习定义、执行与证据、恢复和共享部署。'
                 '每卷有独立目录；每章都提供前后章导航和同章语言切换。命令、字段和示例在两个版本中一致。\n\n'
                 if lang == 'zh' else
                 'Start with volume 01 to install and run a local approval workflow, then continue through '
                 'definition, execution and evidence, recovery and shared deployment. Every volume has an index; '
                 'every chapter links its neighbors and language counterpart. Commands, fields and examples match across editions.\n\n')
        parts = enumerate(book['parts'], 1)
    else:
        part = book['parts'][part_index]
        text = f'# {part_index + 1:02}. {part["title"][lang]}\n\n'
        text += (f'[{"全书目录" if lang == "zh" else "Book contents"}](../README.md) · '
                 f'[{switch}](../../{other}/{part["directory"]}/README.md)\n\n{part["intro"][lang]}\n\n')
        text += '## 章节\n\n' if lang == 'zh' else '## Chapters\n\n'
        parts = [(part_index + 1, part)]
    for p, part in parts:
        if part_index is None:
            text += f'## {p:02}. [{part["title"][lang]}]({part["directory"]}/README.md)\n\n{part["intro"][lang]}\n\n'
        for n, chapter in enumerate(part['chapters'], 1):
            target = chapter['file'] if part_index is None else Path(chapter['file']).name
            text += f'- [{chapter_label(p, n, chapter, lang)}]({target})\n'
        text += '\n'
    if part_index is not None:
        for delta, labels in [(-1, ('Previous volume', '上一卷')), (1, ('Next volume', '下一卷'))]:
            adjacent = part_index + delta
            if 0 <= adjacent < len(book['parts']):
                part = book['parts'][adjacent]
                text += f'[{labels[lang == "zh"]}: {adjacent + 1:02}. {part["title"][lang]}](../{part["directory"]}/README.md)\n\n'
    return text.rstrip() + '\n'


def sync(root=ROOT, check=False):
    book = load_book(root)
    errors = []

    def write_or_check(path, expected):
        if check:
            if not path.exists() or path.read_text() != expected:
                errors.append(f'Outdated book contents or navigation: {path}')
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(expected)

    expected_paths = {root / 'docs/README.md'}
    write_or_check(root / 'docs/README.md', bookshelf(book))
    for lang in ('en', 'zh'):
        indexes = {'README.md': contents(book, lang)}
        indexes.update({part['directory'] + '/README.md': contents(book, lang, i)
                        for i, part in enumerate(book['parts'])})
        for name, body in indexes.items():
            path = edition_path(root, lang, name)
            expected_paths.add(path)
            write_or_check(path, body)
        for index, chapter in enumerate(chapters(book)):
            path = edition_path(root, lang, chapter['file'])
            expected_paths.add(path)
            if not path.exists():
                errors.append(f'Missing {lang} chapter: {path}')
                continue
            body = path.read_text().split(NAV_START)[0].rstrip()
            write_or_check(path, body + '\n\n' + navigation(book, lang, index) + '\n')
    actual = set((root / 'docs').rglob('*.md'))
    if actual != expected_paths:
        errors.append(f'Book inventory differs: {sorted(str(p.relative_to(root)) for p in actual ^ expected_paths)}')
    if errors:
        raise ValueError('\n'.join(errors))


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    sync(check=args.check)
