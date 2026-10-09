#!/usr/bin/env python3
"""Render versioned repository documentation for GitHub Pages."""
import argparse
import html
from html.parser import HTMLParser
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib
from urllib.parse import unquote
from markdown_it import MarkdownIt

ROOT = Path(__file__).resolve().parents[1]
REPO = 'https://github.com/coolplayagent/workflow-cli'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / '_site')
    args = parser.parse_args()
    out = args.output.resolve()
    if out.exists() and any(out.iterdir()):
        raise SystemExit('Site output must be empty')
    out.mkdir(parents=True, exist_ok=True)
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    routes = {ROOT / 'README.md': Path('docs/overview.html')}
    routes.update({p: Path('docs') / (p.stem + '.html') for p in (ROOT / 'docs').glob('*.md')})
    skill = ROOT / 'skills/workflow-cli'
    routes[skill / 'SKILL.md'] = Path('skill/index.html')
    routes.update({p: Path('skill') / (p.stem + '.html') for p in (skill / 'references').glob('*.md')})
    titles = {}
    for source in routes:
        titles[source] = re.search(r'^# (.+)$', source.read_text(), re.M).group(1)
    for asset in ['style.css', 'site.js']:
        shutil.copy2(ROOT / 'website' / asset, out / asset)
    homepage = (ROOT / 'website/index.html').read_text()
    (out / 'index.html').write_text(homepage.replace('__VERSION__', version).replace('__REVISION__', revision[:7]))
    (out / '.nojekyll').touch()
    (out / 'build.json').write_text(__import__('json').dumps({'version': version, 'source_revision': revision}) + '\n')
    md = MarkdownIt('commonmark', {'html': False}).enable('table')
    for source, destination in routes.items():
        def relative(target):
            return os.path.relpath(target, destination.parent)
        text = source.read_text()
        if text.startswith('---\n'):
            text = text.split('---', 2)[2]
        tokens = md.parse(text)
        used = {}
        for i, token in enumerate(tokens):
            if token.type == 'heading_open':
                slug = re.sub(r'[^\w\- ]', '', tokens[i + 1].content.lower()).replace(' ', '-')
                count = used.get(slug, 0)
                used[slug] = count + 1
                token.attrSet('id', slug + (f'-{count}' if count else ''))
            for child in token.children or []:
                if child.type != 'link_open':
                    continue
                href = child.attrGet('href')
                if re.match(r'^[a-z]+:|^#', href):
                    continue
                path, _, anchor = href.partition('#')
                target = (source.parent / unquote(path)).resolve()
                if 'manuals' in target.parts and target.name.endswith('.md'):
                    target = ROOT / 'docs' / target.name
                if target in routes:
                    link = relative(routes[target])
                elif target.is_relative_to(ROOT):
                    link = f'{REPO}/blob/{revision}/{target.relative_to(ROOT)}'
                else:
                    raise ValueError(f'Invalid link: {source}: {href}')
                child.attrSet('href', link + ('#' + anchor if anchor else ''))
        body = md.renderer.render(tokens, md.options, {})
        title = html.escape(titles[source])
        navigation = ''.join(f'<a href="{relative(route)}">{html.escape(titles[p])}</a>'
                             for p, route in routes.items())
        output = f'''<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{title} · Workflow</title>
<link rel="stylesheet" href="{relative(Path('style.css'))}"></head><body>
<header><nav aria-label="Main navigation"><a class="brand" href="{relative(Path('index.html'))}"><span class="mark">w</span>workflow</a>
<a href="{relative(Path('docs/skill-distribution.html'))}">Install</a><a href="{REPO}">GitHub ↗</a></nav></header>
<main class="doc-layout"><aside class="sidebar"><label for="doc-search">Find a guide</label>
<input id="doc-search" type="search" placeholder="Filter documentation…"><strong>Documentation · v{version}</strong>{navigation}</aside>
<article class="document">{body}<p class="doc-source">v{version} · <a href="{REPO}/blob/{revision}/{source.relative_to(ROOT)}">View source · {revision[:7]}</a></p></article></main>
<footer><span>Workflow CLI · MIT</span><span>Published from {revision[:7]}</span></footer>
<script src="{relative(Path('site.js'))}"></script></body></html>'''
        (out / destination).parent.mkdir(parents=True, exist_ok=True)
        (out / destination).write_text(output)
    class Links(HTMLParser):
        def handle_starttag(self, tag, attrs):
            for key, value in attrs:
                if key not in ('href', 'src') or re.match(r'^[a-z]+:|^#', value):
                    continue
                target = (self.source.parent / unquote(value.split('#')[0])).resolve()
                if not target.is_relative_to(out) or not target.is_file():
                    raise ValueError(f'Broken site link: {self.source}: {value}')
    for page in out.rglob('*.html'):
        check = Links()
        check.source = page
        check.feed(page.read_text())
    print(f'Built and checked {len(routes) + 1} pages at {out} for v{version} ({revision[:7]})')


if __name__ == '__main__':
    main()
