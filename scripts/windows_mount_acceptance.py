#!/usr/bin/env python3
"""Mount an isolated WinFsp volume, verify durable read/write and deduplication."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    drive = 'X:'
    root = Path('X:/')
    if root.exists():
        raise SystemExit('X: is already occupied')
    data = random.Random(20261106).randbytes(8 * 1024 * 1024)
    digest = hashlib.sha256(data).digest()
    with tempfile.TemporaryDirectory(prefix='xetfs-win-mount-') as temp:
        repo = Path(temp) / 'repo'
        def cli(*argv):
            return json.loads(subprocess.check_output([binary, '--repo', str(repo), *map(str, argv)]))
        cli('init')
        with open(Path(temp) / 'mount.log', 'w+') as log:
            def start():
                proc = subprocess.Popen([binary, '--repo', str(repo), 'mount', drive],
                    stdout=log, stderr=log, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
                for _ in range(300):
                    if proc.poll() is not None:
                        log.flush(); log.seek(0)
                        raise RuntimeError(log.read())
                    if root.exists():
                        return proc
                    time.sleep(.1)
                proc.terminate(); proc.wait(timeout=10)
                raise RuntimeError('mount timed out')
            def stop(proc):
                proc.send_signal(signal.CTRL_BREAK_EVENT)
                assert proc.wait(timeout=30) == 0
                for _ in range(100):
                    if not root.exists(): return
                    time.sleep(.1)
                raise RuntimeError('unmount timed out')
            proc = start()
            try:
                writes, reads = [], []
                for i in range(4):
                    path = root / f'copy-{i}'
                    started = time.perf_counter()
                    with path.open('wb') as f:
                        assert f.write(data) == len(data)
                        f.flush(); os.fsync(f.fileno())
                    writes.append(time.perf_counter() - started)
                    started = time.perf_counter()
                    assert hashlib.sha256(path.read_bytes()).digest() == digest
                    reads.append(time.perf_counter() - started)
                stop(proc); proc = None
                stats = cli('stats'); fsck = cli('fsck')
                assert stats['logical_bytes'] == 4 * len(data)
                assert stats['xorb_bytes'] < 2 * len(data)
                proc = start()
                for i in range(4):
                    assert hashlib.sha256((root / f'copy-{i}').read_bytes()).digest() == digest
                stop(proc); proc = None
                result = {'status':'passed','platform':'windows-winfsp','copies':4,
                    'bytes_per_file':len(data),'logical_bytes':4*len(data),'xorb_bytes':stats['xorb_bytes'],
                    'data_savings_fraction':1-stats['xorb_bytes']/(4*len(data)),
                    'write_fsync_seconds':writes,'read_seconds':reads,'sha256':digest.hex(),'fsck':fsck}
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(result, indent=2)+'\n')
                print(json.dumps(result, indent=2))
            finally:
                if proc is not None and proc.poll() is None:
                    proc.terminate(); proc.wait(timeout=15)
                log.flush(); log.seek(0)
                print(log.read())


if __name__ == '__main__':
    main()
