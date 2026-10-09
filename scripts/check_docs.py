#!/usr/bin/env python3
"""Check both book editions and all README/skill local links before publication."""
import re

from book import ROOT, NAV_START, chapters, edition_path, load_book, sync
from doc_links import check_links, parse


def check(root=ROOT):
    sync(root, check=True)
    book = load_book(root)
    for chapter in chapters(book):
        for lang in ('en', 'zh'):
            source = edition_path(root, lang, chapter['file'])
            body = source.read_text().split(NAV_START)[0]
            tokens = parse(body)[1]
            prose = '\n'.join(token.content for token in tokens if token.type == 'inline')
            if len(prose) < 200 or len([t for t in tokens if t.type == 'heading_open']) < 2:
                raise ValueError(f'Chapter must contain substantive guidance: {source}')
            if lang == 'zh' and not re.search(r'[\u4e00-\u9fff]', prose):
                raise ValueError(f'Chinese chapter is untranslated: {source}')
    paths = [root / 'README.md', root / 'README.zh-CN.md']
    paths += list((root / 'docs').rglob('*.md'))
    paths += list((root / 'skills/workflow-cli').rglob('*.md'))
    count = check_links(paths, root)
    print(f'Checked {len(chapters(book))} chapters in each edition, '
          f'{len(paths)} Markdown files and {count} local links/anchors')


if __name__ == '__main__':
    check()
