#!/usr/bin/env python3
"""Measure mounted Linux FUSE reads, fsynced writes, and duplicate storage."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    data = random.Random(20261106).randbytes(8 * 1024 * 1024)
    digest = hashlib.sha256(data).digest()
    with tempfile.TemporaryDirectory(prefix='xetfs-mount-benchmark-') as temp:
        root = Path(temp)
        repo, mount = root / 'repo', root / 'mount'
        mount.mkdir()
        def cli(*argv):
            return json.loads(subprocess.check_output([binary, '--repo', str(repo), *map(str, argv)]))
        cli('init')
        with open(root / 'mount.log', 'w+') as log:
            proc = subprocess.Popen([binary, '--repo', str(repo), 'mount', str(mount)], stdout=log, stderr=log)
            try:
                for _ in range(300):
                    if os.path.ismount(mount):
                        break
                    if proc.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(log.read())
                    time.sleep(.05)
                else:
                    raise RuntimeError('mount timed out')
                writes, reads = [], []
                for i in range(4):
                    path = mount / f'copy-{i}'
                    started = time.perf_counter()
                    with path.open('wb') as f:
                        assert f.write(data) == len(data)
                        f.flush()
                        os.fsync(f.fileno())
                    writes.append(time.perf_counter() - started)
                    started = time.perf_counter()
                    with path.open('rb') as f:
                        actual = f.read()
                    reads.append(time.perf_counter() - started)
                    assert hashlib.sha256(actual).digest() == digest
                subprocess.run(['fusermount3', '-u', str(mount)], check=True, timeout=20)
                assert proc.wait(timeout=20) == 0
                stats = cli('stats')
                fsck = cli('fsck')
                logical = 4 * len(data)
                assert stats['logical_bytes'] == logical
                assert stats['xorb_bytes'] < 2 * len(data)
                allocated = sum(p.stat().st_blocks * 512 for p in repo.rglob('*') if p.is_file())
                result = {'status': 'passed', 'platform': 'linux-fuse', 'copies': 4,
                          'bytes_per_file': len(data), 'logical_bytes': logical,
                          'xorb_bytes': stats['xorb_bytes'], 'repository_allocated_bytes': allocated,
                          'data_savings_fraction': 1 - stats['xorb_bytes'] / logical,
                          'repository_savings_fraction': 1 - allocated / logical,
                          'write_fsync_seconds': writes, 'read_seconds': reads,
                          'sha256': digest.hex(), 'fsck': fsck}
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(result, indent=2) + '\n')
                print(json.dumps(result, indent=2))
            finally:
                if proc.poll() is None:
                    subprocess.run(['fusermount3', '-u', '-z', str(mount)], capture_output=True, timeout=10)
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()


if __name__ == '__main__':
    main()
