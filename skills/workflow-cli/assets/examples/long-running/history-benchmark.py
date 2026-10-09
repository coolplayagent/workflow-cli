#!/usr/bin/env python3
"""Release-binary status/replay measurements; local results are not a capacity SLA."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time


def main(binary, output, counts):
    root = Path(__file__).resolve().parents[2]
    binary = Path(binary).resolve()

    def cli(*args):
        result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True, check=True, timeout=90)
        value = json.loads(result.stdout)
        return value.get('result', value)

    with tempfile.TemporaryDirectory(prefix='workflow-history-benchmark-') as temp:
        directory = Path(temp)
        db = directory / 'runs.db'
        start = json.loads((root / 'examples/runs/review-start.json').read_text())
        start['run_id'] = 'history-benchmark'
        (directory / 'start.json').write_text(json.dumps(start))
        cli('run', 'init', db)
        state = cli('run', 'start', db, directory / 'start.json')['snapshot']
        rows = []
        for n in range(max(counts) + 1):
            if n in counts:
                times = []
                for _ in range(5):
                    began = time.perf_counter()
                    observed = cli('run', 'status', db, start['run_id'])
                    times.append((time.perf_counter() - began) * 1000)
                    assert observed['revision'] == n + 1
                usage = cli('run', 'history-usage', db, start['run_id'])
                assert usage['replayed_events'] <= 15
                rows.append(dict(events=n, status_median_ms=round(statistics.median(times), 3),
                                 database_bytes=db.stat().st_size, replayed_events=usage['replayed_events']))
            if n == max(counts):
                break
            event = dict(event_id=f'tick-{n+1}', run_id=start['run_id'], run_digest=state['run_digest'],
                         expected_revision=state['revision'], at_unix_ms=1001+n, kind=dict(type='advance_time'))
            (directory / 'event.json').write_text(json.dumps(event))
            state = cli('run', 'event', db, directory / 'event.json')['snapshot']
        began = time.perf_counter()
        audit = cli('run', 'verify', db, start['run_id'])
        audit_ms = round((time.perf_counter() - began) * 1000, 3)
        assert audit['events_checked'] == max(counts)
    report = dict(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  platform=platform.platform(), logical_cpus=os.cpu_count(), observations=rows,
                  full_audit_ms=audit_ms, audited_events=max(counts), repetitions=5)
    if output:
        Path(output).write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('--output')
    parser.add_argument('--counts', default='0,64,128,256,512')
    args = parser.parse_args()
    main(args.binary, args.output, [int(n) for n in args.counts.split(',')])
