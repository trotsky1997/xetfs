#!/usr/bin/env python3
"""Mount an isolated WinFsp volume, verify durable read/write and deduplication."""
import argparse
import ctypes
from ctypes import wintypes
import msvcrt
import hashlib
import json
import os
import platform
from pathlib import Path
import random
import signal
import subprocess
import tempfile
import time


def allocated_bytes(root):
    class FileStandardInfo(ctypes.Structure):
        _fields_ = [('allocation', ctypes.c_int64), ('end_of_file', ctypes.c_int64),
                    ('links', wintypes.DWORD), ('delete_pending', ctypes.c_ubyte),
                    ('directory', ctypes.c_ubyte)]
    api = ctypes.WinDLL('kernel32', use_last_error=True)
    api.GetFileInformationByHandleEx.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    api.GetFileInformationByHandleEx.restype = wintypes.BOOL
    total = 0
    for path in root.rglob('*'):
        if path.is_file():
            with path.open('rb') as file:
                info = FileStandardInfo()
                if not api.GetFileInformationByHandleEx(msvcrt.get_osfhandle(file.fileno()), 1, ctypes.byref(info), ctypes.sizeof(info)):
                    raise ctypes.WinError(ctypes.get_last_error())
                total += info.allocation
    return total


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
        initial_allocation = allocated_bytes(repo)
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
                folder = root / 'subdirectory'
                folder.mkdir()
                (root / 'copy-3').rename(folder / 'renamed')
                assert [p.name for p in folder.iterdir()] == ['renamed']
                assert hashlib.sha256((folder / 'renamed').read_bytes()).digest() == digest
                (folder / 'renamed').rename(root / 'copy-3')
                folder.rmdir()
                (root / 'links').mkdir()
                os.symlink('../copy-0', root / 'links' / 'file-link')
                os.symlink('links', root / 'dir-link', target_is_directory=True)
                assert (root / 'links' / 'file-link').read_bytes() == data
                assert (root / 'dir-link' / 'file-link').read_bytes() == data
                assert os.readlink(root / 'links' / 'file-link') == '../copy-0'
                acl_file = root / 'copy-0'
                subprocess.run(['icacls', str(acl_file), '/deny', '*S-1-5-32-546:(R)'], check=True, capture_output=True)
                acl_before = subprocess.check_output(['icacls', str(acl_file)], text=True)
                assert 'DENY' in acl_before
                stop(proc); proc = None
                stats = cli('stats'); fsck = cli('fsck')
                allocation = allocated_bytes(repo) - initial_allocation
                assert stats['logical_bytes'] == 4 * len(data)
                assert stats['xorb_bytes'] < 2 * len(data)
                proc = start()
                for i in range(4):
                    assert hashlib.sha256((root / f'copy-{i}').read_bytes()).digest() == digest
                assert (root / 'links' / 'file-link').read_bytes() == data
                assert (root / 'dir-link' / 'file-link').read_bytes() == data
                assert os.readlink(root / 'links' / 'file-link') == '../copy-0'
                acl_after = subprocess.check_output(['icacls', str(root / 'copy-0')], text=True)
                assert 'DENY' in acl_after
                (root / 'links' / 'file-link').unlink()
                (root / 'dir-link').rmdir()
                (root / 'links').rmdir()
                stop(proc); proc = None
                result = {'status':'passed','platform':'windows-winfsp','system':platform.platform(),'copies':4,
                    'bytes_per_file':len(data),'logical_bytes':4*len(data),'xorb_bytes':stats['xorb_bytes'],
                    'data_savings_fraction':1-stats['xorb_bytes']/(4*len(data)),
                    'repository_allocated_increase_bytes':allocation,
                    'repository_savings_fraction':1-allocation/(4*len(data)),
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
