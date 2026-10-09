#!/usr/bin/env python3
"""Render both book editions and skill references with checked offline navigation."""
import argparse
import html
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import shutil
import subprocess
import tomllib
from urllib.parse import quote, urlsplit

from book import NAV_END, NAV_START, ROOT, chapters, edition_path, load_book
from check_docs import check
from doc_links import link_tokens, local_target, parse

REPO = 'https://github.com/coolplayagent/workflow-cli'


def site_routes(root, book):
    routes = {}
    for lang in ('en', 'zh'):
        routes[edition_path(root, lang, 'README.md')] = Path(f'docs/{lang}/index.html')
        overview = 'README.md' if lang == 'en' else 'README.zh-CN.md'
        routes[root / overview] = Path(f'docs/{lang}/overview.html')
        for chapter in chapters(book):
            routes[edition_path(root, lang, chapter['file'])] = Path(f'docs/{lang}/{Path(chapter["file"]).stem}.html')
    skill = root / 'skills/workflow-cli'
    routes[skill / 'SKILL.md'] = Path('skill/index.html')
    routes.update({p: Path('skill') / (p.stem + '.html') for p in (skill / 'references').glob('*.md')})
    return routes


def check_site(out):
    class Links(HTMLParser):
        def __init__(self):
            super().__init__()
            self.links, self.ids = [], set()

        def handle_starttag(self, tag, attrs):
            for key, value in attrs:
                if key == 'id':
                    if value in self.ids:
                        raise ValueError(f'Duplicate HTML id: {value}')
                    self.ids.add(value)
                if key in ('href', 'src'):
                    self.links.append(value)

    parsed = {}
    for page in out.rglob('*.html'):
        parser = Links()
        parser.feed(page.read_text())
        parsed[page.resolve()] = parser
    for source, parser in parsed.items():
        for href in parser.links:
            resolved = local_target(source, href, out)
            if resolved is None:
                continue
            target, fragment = resolved
            if not target.is_file():
                raise ValueError(f'Site destination is not a file: {source}: {href}')
            if fragment and target.suffix == '.html' and fragment not in parsed[target].ids:
                raise ValueError(f'Missing site anchor: {source}: {href}')
    return len(parsed)


def build(out, root=ROOT):
    check(root)
    out = out.resolve()
    if out.exists() and any(out.iterdir()):
        raise ValueError('Site output must be empty')
    out.mkdir(parents=True, exist_ok=True)
    version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    book = load_book(root)
    routes = site_routes(root, book)
    for asset in ['style.css', 'site.js', 'redirect.js']:
        shutil.copy2(root / 'website' / asset, out / asset)
    for filename in ('index.html', 'index.en.html'):
        homepage = (root / 'website' / filename).read_text()
        (out / filename).write_text(homepage.replace('__VERSION__', version).replace('__REVISION__', revision[:7]))
    (out / '.nojekyll').touch()
    (out / 'build.json').write_text(json.dumps({'version': version, 'source_revision': revision,
                                              'languages': ['en', 'zh'], 'chapters_per_language': len(chapters(book))}) + '\n')
    for source, destination in routes.items():
        lang = 'zh' if ('zh' in source.relative_to(root).parts or source.name == 'README.zh-CN.md') else 'en'

        def relative(target):
            return quote(os.path.relpath(target, destination.parent), safe='/.-_')

        document = source.read_text().replace(NAV_START, '').replace(NAV_END, '')
        md, tokens, _ = parse(document)
        for token, attribute in link_tokens(tokens):
            href = token.attrGet(attribute)
            resolved = local_target(source, href, root)
            if resolved is None:
                continue
            target, _ = resolved
            url = urlsplit(href)
            if target in routes:
                link = relative(routes[target])
            else:
                # Examples, schemas and other reading assets are served locally.
                # A path not present in the repository has already failed validation.
                resource = Path('resources') / target.relative_to(root)
                if target.is_dir():
                    resource /= 'index.html'
                    raise ValueError(f'Directory links need an explicit index: {source}: {href}')
                resource_path = out / resource
                resource_path.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(target, resource_path)
                link = relative(resource)
            token.attrSet(attribute, link + ('?' + url.query if url.query else '') + ('#' + url.fragment if url.fragment else ''))
        body = md.renderer.render(tokens, md.options, {})
        title = next(tokens[i + 1].content for i, token in enumerate(tokens) if token.type == 'heading_open' and token.tag == 'h1')
        navigation = f'<a href="{relative(Path(f"docs/{lang}/index.html"))}">{html.escape(book["title"][lang])}</a>'
        for part in book['parts']:
            navigation += f'<strong>{html.escape(part["title"][lang])}</strong>'
            for chapter in part['chapters']:
                path = edition_path(root, lang, chapter['file'])
                current = ' aria-current="page"' if path == source else ''
                navigation += f'<a{current} href="{relative(routes[path])}">{html.escape(chapter["title"][lang])}</a>'
        navigation += '<strong>Skill</strong>'
        for path, route in routes.items():
            if route.parts[0] == 'skill':
                navigation += f'<a href="{relative(route)}">{html.escape(path.stem)}</a>'
        other = 'en' if lang == 'zh' else 'zh'
        if source.parent == edition_path(root, lang, 'README.md').parent:
            counterpart = edition_path(root, other, source.name)
        elif source.name in ('README.md', 'README.zh-CN.md'):
            counterpart = root / ('README.md' if other == 'en' else 'README.zh-CN.md')
        else:
            counterpart = edition_path(root, other, 'README.md')
        language_switch = relative(routes[counterpart])
        labels = {'zh': ['查找章节', '筛选章节…', '安装', '查看源码', '文档导航'],
                  'en': ['Find a chapter', 'Filter chapters…', 'Install', 'View source', 'Book navigation']}[lang]
        output = f'''<!doctype html><html lang="{'zh-CN' if lang == 'zh' else 'en'}"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{html.escape(title)} · Workflow</title>
<link rel="stylesheet" href="{relative(Path('style.css'))}"></head><body>
<header><nav aria-label="Main navigation"><a class="brand" href="{relative(Path('index.html' if lang == 'zh' else 'index.en.html'))}"><span class="mark">w</span>workflow</a>
<a href="{relative(Path(f'docs/{lang}/skill-distribution.html'))}">{labels[2]}</a><a class="language-switch" lang="{other}" href="{language_switch}">{'English' if other == 'en' else '中文'}</a><a href="{REPO}">GitHub ↗</a></nav></header>
<main class="doc-layout"><aside class="sidebar" aria-label="{labels[4]}"><label for="doc-search">{labels[0]}</label>
<input id="doc-search" type="search" placeholder="{labels[1]}"><strong>v{version}</strong>{navigation}</aside>
<article class="document">{body}<p class="doc-source">v{version} · <a href="{REPO}/blob/{revision}/{source.relative_to(root)}">{labels[3]} · {revision[:7]}</a></p></article></main>
<footer><span>Workflow CLI · MIT</span><span>{revision[:7]}</span></footer>
<script src="{relative(Path('site.js'))}"></script></body></html>'''
        (out / destination).parent.mkdir(parents=True, exist_ok=True)
        (out / destination).write_text(output)
    # Preserve old public URLs and their fragments without duplicating book content.
    aliases = {'overview': 'overview', **{Path(c['file']).stem: Path(c['file']).stem for c in chapters(book)}}
    for legacy, target in aliases.items():
        (out / f'docs/{legacy}.html').write_text(f'''<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Moved · Workflow</title></head>
<body><p><a id="redirect-target" href="en/{target}.html">Continue to this chapter</a></p><script src="../redirect.js"></script></body></html>''')
    count = check_site(out)
    print(f'Built and checked {count} pages at {out} for v{version} ({revision[:7]})')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / '_site')
    args = parser.parse_args()
    build(args.output)
