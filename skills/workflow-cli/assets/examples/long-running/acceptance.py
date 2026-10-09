#!/usr/bin/env python3
"""Real process/HTTP fixtures for bounded retries, durable sessions and cancellation."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[2]


def wait(test, label, seconds=30):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        value = test()
        if value:
            return value
        time.sleep(.05)
    raise AssertionError(f'timed out: {label}')


def descendants(pid):
    children = set()
    for task in Path(f'/proc/{pid}/task').glob('*'):
        try:
            children.update(map(int, (task / 'children').read_text().split()))
        except (FileNotFoundError, ProcessLookupError):
            pass
    return children


def stopped(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().split()[2] == 'Z'
    except (FileNotFoundError, ProcessLookupError):
        return True


class Provider:
    def __init__(self, mode):
        self.mode = mode
        self.contexts = []
        self.blocked = threading.Event()
        self.release = threading.Event()
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                context = json.loads(body['input'])
                with owner.lock:
                    owner.contexts.append(context)
                    count = len(owner.contexts)
                if (mode == 'restart' and count == 2) or (mode == 'cancel' and count == 1):
                    owner.blocked.set()
                    if not owner.release.wait(40):
                        return
                if mode == 'retry' and count == 1:
                    self.send_response(503)
                    self.send_header('Retry-After', '1')
                    self.send_header('Content-Length', '0')
                    self.end_headers()
                    return
                tools = [e for e in context['events'] if e['type'] == 'tool_finished'
                         and e['response']['status'] == 'received']
                if tools:
                    outputs = tools[-1]['response']['result']['outcome']['outputs']
                    action = dict(type='complete', outputs=outputs, summary='Return observed compiler output')
                else:
                    action = dict(type='call', capability=context['policy']['tools'][0]['capability'],
                                  inputs=context['inputs'], summary='Invoke the declared compiler')
                proposal = json.dumps(dict(protocol_version=1, action=action))
                reply = dict(id=f'reply-{count}', model='fixture', status='completed',
                             output=[dict(type='message', role='assistant', status='completed',
                                          content=[dict(type='output_text', text=proposal)])])
                data = json.dumps(reply).encode()
                try:
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


def main(binary, output):
    results = []
    with tempfile.TemporaryDirectory(prefix='workflow-long-running-') as temporary:
        root = Path(temporary)
        processes = []
        providers = []

        def cli(*args):
            completed = subprocess.run([binary, *map(str, args)], capture_output=True, text=True, timeout=30)
            if completed.returncode:
                raise AssertionError(f'{args[:2]}: {completed.stdout} {completed.stderr}')
            value = json.loads(completed.stdout)
            return value.get('result', value)

        def save(path, value):
            path.write_text(json.dumps(value))
            return path

        try:
            for mode in ['retry', 'restart', 'cancel']:
                directory = root / mode
                directory.mkdir()
                db = directory / 'runs.db'
                provider = Provider(mode)
                providers.append(provider)
                start = json.loads((ROOT / 'examples/models/start.json').read_text())
                start['run_id'] = mode
                start['started_at_unix_ms'] = int(time.time() * 1000)
                policy = start['bundle']['model_policies'][0]
                policy['retry'] = dict(max_retries=2, initial_backoff_ms=20, max_backoff_ms=2000)
                policy['budget']['model_calls'] = 4
                policy['task']['error_codes'].update(model_rate_limited='permanent', model_authentication='permanent')
                for cap in start['bundle']['capabilities']:
                    if cap['capability'] == policy['task']['capability']:
                        cap.update(policy['task'])
                binding = cli('model', 'check-policy', save(directory / 'policy.json', policy))['binding']
                models = save(directory / 'bindings.json', [dict(policy=binding, http=dict(
                    schema_version=1, provider='openai_responses', model='fixture',
                    endpoint=f'http://127.0.0.1:{provider.server.server_port}/model',
                    api_key_env='WORKFLOW_LONG_RUNNING_FIXTURE_KEY', allow_loopback_http=True))])
                cli('run', 'init', db)
                cli('run', 'start', db, save(directory / 'start.json', start))
                config = save(directory / 'daemon.json', dict(schema_version=1, database=str(db),
                    control_directory=str(directory / 'control'), model_bindings=str(models),
                    artifacts=None, effect_bindings=None, lease_ms=2000, poll_interval_ms=50, error_backoff_ms=100))

                def daemon():
                    child = subprocess.Popen([binary, 'daemon', 'serve', str(config)],
                        env=dict(os.environ, WORKFLOW_LONG_RUNNING_FIXTURE_KEY='fixture-only'),
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                    processes.append(child)
                    return child

                child = daemon()
                if mode in ['restart', 'cancel']:
                    wait(provider.blocked.is_set, f'{mode} external call')
                    activity = wait(lambda: descendants(child.pid), 'activity subprocess')
                    if mode == 'restart':
                        child.kill()
                        child.wait(timeout=10)
                        for pid in activity:
                            wait(lambda pid=pid: stopped(pid), 'parent-death cancellation')
                        child = daemon()
                        wait(lambda: len(provider.contexts) >= 3, 'resumed provider context')
                        provider.release.set()
                    else:
                        snapshot = cli('run', 'status', db, mode)
                        cli('run', 'cancel', db, mode, 'operator-cancel', snapshot['revision'], int(time.time()*1000))
                        for pid in activity:
                            wait(lambda pid=pid: stopped(pid), 'cancelled subprocess reaped')
                        provider.release.set()
                expected = 'cancelled' if mode == 'cancel' else 'succeeded'
                wait(lambda: cli('run', 'status', db, mode)['status'] == expected, f'{mode} terminal state')
                cli('daemon', 'stop', directory / 'control')
                child.wait(timeout=20)
                cli('run', 'verify', db, mode)
                records = cli('run', 'execution-history', db, mode, 0, 100)['items']
                finished = [r['action'] for r in records if r['action']['type'] == 'finished']
                if mode != 'cancel':
                    assert len(finished) == 1
                    record = finished[0]['result']['model_record']
                    observed_tools = [e for e in record['events'] if e['type'] == 'tool_finished']
                    assert len(observed_tools) == 1, 'acknowledged tool repeated'
                    if mode == 'retry':
                        assert record['events'][0]['response']['failure'] == dict(temporary=dict(retry_after_ms=1000))
                        admissions = [r['action']['checkpoint']['admitted'] for r in records
                                      if r['action']['type'] == 'model_checkpoint' and r['action']['checkpoint']['admitted']]
                        assert admissions[1]['at_unix_ms'] >= record['events'][0]['at_unix_ms'] + 1000
                    else:
                        assert record['schema_version'] == 2
                        assert any(e.get('response', {}).get('failure') == 'uncertain' for e in record['events'])
                        assert sum(r['action']['type'] == 'prepared' for r in records) == 2
                else:
                    assert not finished
                    assert len(provider.contexts) == 1
                results.append(dict(case=mode, status=expected, actual_http_calls=len(provider.contexts),
                                    execution_records=len(records)))
        finally:
            for provider in providers:
                provider.close()
            for child in processes:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=10)
    report = dict(ok=True, cases=results, provider='deterministic loopback fixture')
    if output:
        Path(output).write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('--output')
    args = parser.parse_args()
    main(str(Path(args.binary).resolve()), args.output)
