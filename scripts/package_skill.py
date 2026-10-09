#!/usr/bin/env python3
"""Build the versioned, self-contained Linux skill from a verified CLI binary."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

from doc_links import check_links

ROOT = Path(__file__).resolve().parents[1]


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def copy_resources(root, package):
    """Package the skill as-is; never import resources from outside its directory."""
    source = root / 'skills/workflow-cli'
    for path in source.rglob('*'):
        if path.is_symlink():
            raise ValueError(f'Symlink cannot be packaged as a local resource: {path}')
    check_links(list(source.rglob('*.md')), source)
    shutil.copytree(source, package, ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    check_links(list(package.rglob('*.md')), package)
    return {p: package / p.relative_to(source) for p in source.rglob('*') if p.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--output', default=ROOT / 'dist', type=Path)
    parser.add_argument('--tag', help='Require this release tag to match the package version')
    args = parser.parse_args()
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
    skill = ROOT / 'skills/workflow-cli'
    if (skill / 'VERSION').read_text().strip() != version:
        raise SystemExit('Skill VERSION and Cargo workspace version differ')
    if f'version: "{version}"' not in (skill / 'SKILL.md').read_text():
        raise SystemExit('Skill metadata version differs from Cargo')
    if args.tag and args.tag != f'v{version}':
        raise SystemExit('Release tag must match the package version')
    binary = args.binary.resolve(strict=True)
    runtime = json.loads(subprocess.check_output([str(binary), 'version', '--format', 'json']))
    if (runtime['version'], runtime['os'], runtime['architecture']) != (version, 'linux', 'x86_64'):
        raise SystemExit('A matching Linux x86_64 CLI binary is required')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f'workflow-cli-skill-v{version}-linux-x86_64.tar.gz'
    if archive.exists():
        raise SystemExit(f'Refusing to replace {archive}')
    with tempfile.TemporaryDirectory(prefix='workflow-package-') as temp:
        package = Path(temp) / 'workflow-cli'
        copy_resources(ROOT, package)
        destination = package / 'assets/linux-x86_64/workflow'
        destination.parent.mkdir(parents=True)
        shutil.copy2(binary, destination)
        destination.chmod(0o755)
        (package / 'scripts/workflow.sh').chmod(0o755)
        manifest = {
            'schema_version': 1, 'name': 'workflow-cli', 'version': version,
            'source_revision': revision, 'target': 'x86_64-unknown-linux-gnu',
            'runtime': runtime,
            'files': {str(p.relative_to(package)): sha256(p)
                      for p in sorted(package.rglob('*')) if p.is_file()},
        }
        (package / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        with archive.open('xb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode='w') as tar:
                for path in [package, *sorted(package.rglob('*'))]:
                    info = tar.gettarinfo(str(path), arcname=str(path.relative_to(package.parent)))
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ''
                    if path.is_file():
                        with path.open('rb') as stream:
                            tar.addfile(info, stream)
                    else:
                        tar.addfile(info)
    (args.output / 'SHA256SUMS').write_text(f'{sha256(archive)}  {archive.name}\n')
    print(json.dumps({'archive': str(archive), 'sha256': sha256(archive), 'version': version,
                      'source_revision': revision}))


if __name__ == '__main__':
    main()
