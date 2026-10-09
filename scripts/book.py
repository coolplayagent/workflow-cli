"""One reading order for Markdown, Pages and the offline skill manual."""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NAV_START = '<!-- book-navigation -->'
NAV_END = '<!-- /book-navigation -->'


def load_book(root=ROOT):
    book = json.loads((root / 'docs/book.json').read_text())
    names = [chapter['file'] for part in book['parts'] for chapter in part['chapters']]
    if len(names) != len(set(names)) or any(Path(n).name != n or not n.endswith('.md') for n in names):
        raise ValueError('Book chapters must have unique Markdown filenames')
    return book


def chapters(book):
    return [chapter for part in book['parts'] for chapter in part['chapters']]


def edition_path(root, lang, name):
    return root / 'docs' / ('zh/' if lang == 'zh' else '') / name


def navigation(book, lang, index):
    ordered = chapters(book)
    name = ordered[index]['file']
    switch = f'[English](../{name})' if lang == 'zh' else f'[中文](zh/{name})'
    links = [f'[{"目录" if lang == "zh" else "Contents"}](README.md)', switch]
    if index:
        previous = ordered[index - 1]
        label = '上一章' if lang == 'zh' else 'Previous'
        links.append(f'[{label}: {previous["title"][lang]}]({previous["file"]})')
    if index + 1 < len(ordered):
        following = ordered[index + 1]
        label = '下一章' if lang == 'zh' else 'Next'
        links.append(f'[{label}: {following["title"][lang]}]({following["file"]})')
    return NAV_START + '\n\n' + ' · '.join(links) + '\n\n' + NAV_END


def contents(book, lang):
    text = f'# {book["title"][lang]}\n\n'
    if lang == 'zh':
        text += ('[English](../README.md) · [项目介绍](../../README.zh-CN.md)\n\n'
                 '本书从一个可运行的本地审批流程开始，逐步讲解定义、持久化执行、'
                 '证据门禁、外部操作恢复与共享部署。首次使用请顺序阅读前三章；'
                 '需要维护生产运行时，先完成本地执行和恢复，再进入共享执行篇。'
                 '每章都有上一章、下一章和同章语言切换。\n\n'
                 '命令中的 JSON 字段、标识符和参数在两个版本中保持一致。'
                 '文档示例会说明所需的运行环境；验收章节区分已有测试证据与尚未完成的目标。\n\n')
    else:
        text += ('[中文](zh/README.md) · [Project overview](../README.md)\n\n'
                 'This book follows a runnable local approval workflow from its definition '
                 'through durable execution, evidence gates, effect recovery and shared deployment. '
                 'Read the first three chapters in order on your first visit. Before operating '
                 'a shared deployment, work through local execution and recovery. Every chapter '
                 'provides previous/next navigation and a link to the same chapter in Chinese.\n\n'
                 'JSON fields, identifiers and command arguments stay identical in both editions. '
                 'Examples state their runtime prerequisites; acceptance chapters distinguish '
                 'tested behavior from unfinished objectives.\n\n')
    number = 0
    for index, part in enumerate(book['parts'], 1):
        text += f'## {index}. {part["title"][lang]}\n\n'
        for chapter in part['chapters']:
            number += 1
            text += f'{number}. [{chapter["title"][lang]}]({chapter["file"]})\n'
        text += '\n'
    return text.rstrip() + '\n'


def sync(root=ROOT, check=False):
    book = load_book(root)
    errors = []
    for lang in ('en', 'zh'):
        index_path = edition_path(root, lang, 'README.md')
        expected = contents(book, lang)
        if check:
            if not index_path.exists() or index_path.read_text() != expected:
                errors.append(f'Outdated book contents: {index_path}')
        else:
            index_path.parent.mkdir(parents=True, exist_ok=True)
            index_path.write_text(expected)
        for index, chapter in enumerate(chapters(book)):
            path = edition_path(root, lang, chapter['file'])
            if not path.exists():
                errors.append(f'Missing {lang} chapter: {path}')
                continue
            body = path.read_text().split(NAV_START)[0].rstrip()
            expected = body + '\n\n' + navigation(book, lang, index) + '\n'
            if check:
                if path.read_text() != expected:
                    errors.append(f'Outdated chapter navigation: {path}')
            else:
                path.write_text(expected)
        actual = {p.name for p in index_path.parent.glob('*.md')} - {'README.md'}
        declared = {c['file'] for c in chapters(book)}
        if actual != declared:
            errors.append(f'{lang} chapter inventory differs: {sorted(actual ^ declared)}')
    if errors:
        raise ValueError('\n'.join(errors))


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    sync(check=args.check)
