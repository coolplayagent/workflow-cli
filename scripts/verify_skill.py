#!/usr/bin/env python3
"""Verify and exercise the extracted release outside the source checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

from doc_links import check_links


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='workflow-installed skill-') as temp:
        root = Path(temp)
        with tarfile.open(args.archive) as tar:
            tar.extractall(root, filter='data')
        skill = root / 'workflow-cli'
        assert list(root.rglob('SKILL.md')) == [skill / 'SKILL.md'], 'Expected one skill entrypoint'
        manifest = json.loads((skill / 'manifest.json').read_text())
        actual = {str(p.relative_to(skill)) for p in skill.rglob('*') if p.is_file()}
        assert actual == set(manifest['files']) | {'manifest.json'}
        for name, digest in manifest['files'].items():
            assert hashlib.sha256((skill / name).read_bytes()).hexdigest() == digest, name
        links = check_links(list(skill.rglob('*.md')), skill)
        book = json.loads((skill / 'references/manuals/book.json').read_text())
        for part in book['parts']:
            for chapter in part['chapters']:
                for language in ['en/', 'zh/']:
                    assert (skill / 'references/manuals' / (language + chapter['file'])).is_file()
        for language in ['en', 'zh']:
            manuals = skill / 'references/manuals' / language
            assert (manuals / 'README.md').is_file()
            for part in book['parts']:
                assert (manuals / part['directory'] / 'README.md').is_file()
        cwd = root / 'unrelated project'
        cwd.mkdir()
        wrapper = str(skill / 'scripts/workflow.sh')
        env = dict(os.environ)
        env.pop('WORKFLOW_BIN', None)

        def call(*argv, code=0, environment=env):
            result = subprocess.run([wrapper, *map(str, argv)], cwd=cwd, env=environment,
                                    capture_output=True, text=True, timeout=60)
            assert result.returncode == code, (argv, result.returncode, result.stdout, result.stderr)
            return result.stdout

        version = json.loads(call('version', '--format', 'json'))
        assert version == manifest['runtime']
        assert 'workflow run start' in call('help')
        assert json.loads(call('schema'))['title'] == 'Workflow'
        definition = skill / 'assets/examples/review.yaml'
        assert json.loads(call('validate', definition))['valid'] is True
        (cwd / 'bad.json').write_text('{}')
        assert json.loads(call('validate', 'bad.json', code=1))['valid'] is False
        assert json.loads(call('validate', 'missing.json', code=2))['valid'] is False
        # CWD preservation, real execution, negative business outcome and no repeat on recovery.
        call('run', 'init', 'runs.db')
        assert (cwd / 'runs.db').is_file()
        for scenario, terminal in [('valid', 'succeeded'), ('invalid', 'failed')]:
            run_id = f'inspect-{scenario}'
            start = skill / f'assets/examples/execution/{scenario}-start.json'
            call('run', 'start', 'runs.db', start)
            first = json.loads(call('run', 'drive', 'runs.db', run_id, 'package-test', 10))['result']
            assert first['executed_tasks'] == 1, first
            assert first['snapshot']['status'] == terminal, first
            second = json.loads(call('run', 'drive', 'runs.db', run_id, 'package-test', 10))['result']
            assert second['executed_tasks'] == 0, second
            call('run', 'verify', 'runs.db', run_id)
        # The packaged approval demo exercises daemon, callback, history and backup in both branches.
        demos = []
        for decision in ['approve', 'reject']:
            output = subprocess.check_output(
                ['python3', str(skill / 'assets/examples/execution/offline-demo.py'),
                 '--workflow', wrapper, '--decision', decision], cwd=cwd, env=env, timeout=60)
            demos.append(json.loads(output))
        # Explicit overrides fail closed; a source/light installation can resolve the same CLI on PATH.
        wrong = cwd / 'wrong-workflow'
        wrong.write_text('#!/bin/sh\necho "workflow 0.0.0"\n')
        wrong.chmod(0o755)
        call('help', code=2, environment={**env, 'WORKFLOW_BIN': str(wrong)})
        binary = skill / 'assets/linux-x86_64/workflow'
        fallback = cwd / 'bin'
        fallback.mkdir()
        binary.rename(fallback / 'workflow')
        output = call('--version', environment={**env, 'PATH': str(fallback) + os.pathsep + env['PATH']})
        assert output.strip() == f"workflow {manifest['version']}"
        print(json.dumps({'verified': True, 'version': manifest['version'],
                          'source_revision': manifest['source_revision'],
                          'manifest_files': len(manifest['files']), 'local_links': links, 'demos': demos}, indent=2))


if __name__ == '__main__':
    main()
