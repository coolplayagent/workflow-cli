#!/usr/bin/env python3
"""Maintain the committed skill reading resources; installed skills never need a checkout."""
import argparse
from pathlib import Path
import shutil

from book import ROOT
from doc_links import check_links, rewrite_links


def resource_mapping(root):
    skill = root / 'skills/workflow-cli'
    mapping = {p: p for p in skill.rglob('*') if p.is_file()}
    generated = {}
    for source, target in [(root / 'docs', skill / 'references/manuals'),
                           (root / 'examples', skill / 'assets/examples'),
                           (root / 'schemas', skill / 'assets/schemas')]:
        for path in [source, *sorted(source.rglob('*'))]:
            if '__pycache__' in path.parts or path.suffix == '.pyc':
                continue
            if path.is_symlink():
                raise ValueError(f'Symlink cannot be a skill resource: {path}')
            destination = target / path.relative_to(source)
            mapping[path] = destination
            if path.is_file():
                generated[path] = destination
    for name, target in [('README.md', 'references/manuals/overview.md'),
                         ('README.zh-CN.md', 'references/manuals/overview.zh-CN.md'),
                         ('LICENSE', 'LICENSE')]:
        mapping[root / name] = generated[root / name] = skill / target
    return mapping, generated


def sync_resources(root=ROOT, check=False):
    skill = root / 'skills/workflow-cli'
    mapping, generated = resource_mapping(root)
    errors = []
    expected = set(generated.values())
    actual = {skill / 'LICENSE'}
    for directory in ['references/manuals', 'assets/examples', 'assets/schemas']:
        actual.update(p for p in (skill / directory).rglob('*')
                      if p.is_file() and '__pycache__' not in p.parts and p.suffix != '.pyc')
    for stale in sorted(actual - expected):
        if check:
            errors.append(f'Unexpected generated skill resource: {stale}')
        else:
            stale.unlink()
    for source, destination in generated.items():
        data = source.read_bytes()
        if source.suffix == '.md':
            data = rewrite_links(data.decode(), source, destination, mapping, root).encode()
        if check:
            if not destination.is_file() or destination.read_bytes() != data:
                errors.append(f'Stale or missing skill resource: {destination}')
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)
            shutil.copymode(source, destination)
    if errors:
        raise ValueError('\n'.join(errors))
    # Use the skill as the containment boundary, even inside the source checkout.
    check_links(list(skill.rglob('*.md')), skill)
    return len(expected)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    print(f'Checked/synchronized {sync_resources(check=args.check)} local skill resources')
