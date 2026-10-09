#!/usr/bin/env python3
"""Run the deterministic offline SOP through the actual CLI and local daemon."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--workflow', required=True, help='Path to the built workflow executable')
parser.add_argument('--decision', choices=['approve', 'reject'], required=True,
                    help='Explicit local operator decision for this demonstration')
args = parser.parse_args()
workflow = str(Path(args.workflow).resolve())

def call(*argv):
    result = subprocess.run([workflow, *map(str, argv)], capture_output=True, text=True)
    if not result.stdout.strip():
        raise RuntimeError(f"CLI exited {result.returncode}: {result.stderr.strip()}")
    value = json.loads(result.stdout)
    if result.returncode or not value.get('ok'):
        raise RuntimeError(value)
    return value['result']

with tempfile.TemporaryDirectory(prefix='workflow-offline-') as directory:
    root = Path(directory)
    db = root / 'runs.db'
    control = root / 'control'
    request = json.loads(Path(__file__).with_name('offline-start.json').read_text())
    request['started_at_unix_ms'] = int(time.time() * 1000)
    start = root / 'start.json'
    start.write_text(json.dumps(request))
    call('run', 'init', db)
    call('run', 'start', db, start)
    config = root / 'daemon.json'
    config.write_text(json.dumps(dict(schema_version=1, database=str(db),
        control_directory=str(control), artifacts=None, model_bindings=None,
        effect_bindings=None, poll_interval_ms=50, error_backoff_ms=1000)))
    began = time.monotonic()
    with (root / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([workflow, 'daemon', 'serve', str(config)], stdout=log)
        try:
            deadline = time.monotonic() + 30
            while True:
                waits = call('run', 'waits', db, request['run_id'], 0, 100)['items']
                if waits:
                    break
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('daemon did not reach the approval wait')
                time.sleep(.05)
            target = waits[0]
            snapshot = call('run', 'status', db, request['run_id'])
            signal = root / 'signal.json'
            signal.write_text(json.dumps(dict(schema_version=1, run_id=request['run_id'],
                run_digest=snapshot['run_digest'], message=dict(schema_version=1,
                message_id='local-operator-response', source='local-demo-operator',
                target=target['target'], correlation_id=target['correlation_id'],
                decision=args.decision, outputs={},
                expires_at_unix_ms=target['deadline_unix_ms'],
                reason='Explicit --decision supplied by the local operator'))))
            receipt = call('run', 'receive', db, signal)
            final = call('run', 'status', db, request['run_id'])
            expected = 'succeeded' if args.decision == 'approve' else 'cancelled'
            assert final['status'] == expected, final
            proof = call('run', 'verify', db, request['run_id'])
            exported = call('run', 'export', db, root / 'export')
            assert exported['runs'] == 1 and exported['verified']
            assert call('backup', 'verify', root / 'export')['digest'] == exported['digest']
            history = call('run', 'execution-history', db, request['run_id'], 0, 100)
            assert history['next_cursor'] is None
            settled = [x for x in history['items'] if x['action']['type'] == 'finished']
            assert len(settled) == 2, settled
            for record in settled:
                result = record['action']['result']
                assert result['outcome']['status'] == 'succeeded', result
                assert result['outcome']['outputs']['valid'] is True, result
            stop = call('daemon', 'stop', control)
            assert stop['stop_requested'] and not stop['stopped']
            assert daemon.wait(timeout=5) == 0
            state = call('daemon', 'status', control)
            assert state['availability'] == 'stopped' and state['status'] is None
            print(json.dumps(dict(scenario='offline branch/loop/parallel/approval',
                decision=args.decision, status=final['status'], committed_tasks=len(settled),
                revision=proof['revision'], elapsed_ms=round((time.monotonic()-began)*1000,3),
                callback_status=receipt['entry']['status'], daemon_after_stop=state,
                models_invoked=0, network_required=False), indent=2))
        finally:
            if daemon.poll() is None:
                daemon.kill()
                daemon.wait()
