#!/usr/bin/env python3
"""Linux acceptance using real kernel FUSE calls, remount and SIGKILL recovery.

Requires /dev/fuse, fusermount3 and permission to mount FUSE.
Uses only a fresh temporary repository and mountpoint.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shutil
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    assert platform.system() == "Linux"
    binary = str(args.binary.resolve())
    checks = []
    with tempfile.TemporaryDirectory(prefix="xetfs-mount-") as temp:
        root = Path(temp)
        repo, mount = root / "repo", root / "mount"
        mount.mkdir()

        def cli(*cmd, raw=False):
            output = subprocess.check_output([binary, "--repo", str(repo), *map(str, cmd)])
            return output if raw else (json.loads(output) if output.strip() else None)

        cli("init")
        process = None
        log = open(root / "mount.log", "wb")

        def start():
            child = subprocess.Popen([binary, "--repo", str(repo), "mount", str(mount)], stdout=log, stderr=log)
            for _ in range(300):
                if os.path.ismount(mount):
                    return child
                if child.poll() is not None:
                    raise RuntimeError((root / "mount.log").read_text())
                time.sleep(0.02)
            child.kill()
            child.wait()
            raise RuntimeError("mount timed out")

        def stop(child):
            subprocess.run(["fusermount3", "-u", str(mount)], check=True, timeout=10)
            child.wait(timeout=10)
            assert child.returncode == 0

        try:
            process = start()
            model = random.Random(123).randbytes(2 * 1024 * 1024)
            (mount / "models" / "sub").mkdir(parents=True)
            path = mount / "models" / "sub" / "weights.bin"
            with open(path, "wb") as file:
                file.write(model)
                file.flush()
                os.fsync(file.fileno())
            assert path.read_bytes() == model
            with open(path, "r+b", buffering=0) as file:
                file.seek(65530)
                assert file.read(100000) == model[65530:165530]
                os.pwrite(file.fileno(), b"PATCHED", 76543)
                model = model[:76543] + b"PATCHED" + model[76550:]
                os.fsync(file.fileno())
            assert path.read_bytes() == model
            checks.append("streaming writes, random reads, pwrite, fsync")

            clone = mount / "clone.bin"
            shutil.copyfile(path, clone)
            assert clone.read_bytes() == model
            hard = mount / "hard.bin"
            os.link(path, hard)
            assert hard.stat().st_ino == path.stat().st_ino and hard.stat().st_nlink == 2
            with open(hard, "ab", buffering=0) as file:
                file.write(b"tail")
            model += b"tail"
            assert path.read_bytes() == model and clone.read_bytes() == model[:-4]
            checks.append("copy deduplication, hard links, append visibility")

            os.symlink("models/sub/weights.bin", mount / "relative")
            assert (mount / "relative").read_bytes() == model
            utf = mount / "模型数据"
            utf.write_bytes("文件系统".encode())
            raw = os.fsencode(mount) + b"/raw-\xff"
            fd = os.open(raw, os.O_CREAT | os.O_WRONLY, 0o600)
            os.write(fd, b"raw-name")
            os.close(fd)
            assert b"raw-\xff" in os.listdir(os.fsencode(mount))
            os.unlink(raw)
            checks.append("relative symlinks, Unicode and non-UTF8 names")

            sparse = mount / "sparse"
            with open(sparse, "w+b", buffering=0) as file:
                file.seek(1024 * 1024)
                file.write(b"end")
                file.seek(1024 * 1024 - 10)
                assert file.read() == b"\0" * 10 + b"end"
                file.truncate(5)
                file.seek(0)
                assert file.read() == b"\0" * 5
            os.chmod(sparse, 0o640)
            os.utime(sparse, ns=(123456789000, 987654321000))
            st = sparse.stat()
            assert st.st_size == 5 and st.st_mode & 0o777 == 0o640 and st.st_mtime_ns == 987654321000
            checks.append("holes, truncate, chmod and timestamps")

            old = mount / "old"
            old.write_bytes(b"old inode")
            opened = open(old, "r+b", buffering=0)
            replacement = mount / "replacement"
            replacement.write_bytes(b"replacement")
            os.replace(replacement, old)
            assert opened.read() == b"old inode" and old.read_bytes() == b"replacement"
            opened.seek(0)
            opened.write(b"OPEN")
            os.fsync(opened.fileno())
            assert os.fstat(opened.fileno()).st_nlink == 0
            opened.close()
            keep = open(old, "rb", buffering=0)
            old.unlink()
            assert keep.read() == b"replacement"
            keep.close()
            checks.append("rename replacement and open-unlink lifetime")

            listing = mount / "listing"
            listing.mkdir()
            for i in range(600):
                (listing / f"entry-{i:04}").touch()
            assert len(list(listing.iterdir())) == 600
            shutil.rmtree(listing)
            try:
                os.rename(mount / "models", mount / "models" / "sub" / "cycle")
                raise AssertionError("directory cycle accepted")
            except OSError as exc:
                assert exc.errno == errno.EINVAL
            fd = os.open(mount, os.O_RDONLY | os.O_DIRECTORY)
            os.fsync(fd)
            os.close(fd)
            assert os.statvfs(mount).f_bsize > 0
            checks.append("paged readdir, recursive removal, directory cycle rejection and fsyncdir")
            locked = subprocess.run([binary, "--repo", str(repo), "stats"], capture_output=True)
            assert locked.returncode != 0 and b"already open" in locked.stderr
            checks.append("exclusive repository owner")

            stop(process)
            process = None
            cli("fsck")
            stats = cli("stats")
            cli("snapshot", "create", "mounted")
            process = start()
            assert path.read_bytes() == model and hard.read_bytes() == model
            assert clone.read_bytes() == model[:-4]
            checks.append("unmount and persistent remount")

            crash = mount / "crash"
            fd = os.open(crash, os.O_CREAT | os.O_RDWR, 0o600)
            durable = b"fsync durability\n" * 10000
            os.write(fd, durable)
            os.fsync(fd)
            os.write(fd, b"uncommitted tail")
            os.kill(process.pid, signal.SIGKILL)
            process.wait(timeout=10)
            try:
                os.close(fd)
            except OSError:
                pass
            subprocess.run(["fusermount3", "-u", "-z", str(mount)], check=True, timeout=10)
            process = None
            assert cli("cat", "/crash", raw=True) == durable
            cli("fsck")
            checks.append("SIGKILL: fsynced content recovered; unsynced tail excluded")
            report = dict(status="passed", system=platform.platform(), checks=checks,
                          live_stats_before_crash=stats, bytes_verified=len(model),
                          model_sha256=hashlib.sha256(model).hexdigest())
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report, indent=2))
        except BaseException:
            log.flush()
            print((root / "mount.log").read_text(), file=__import__("sys").stderr)
            raise
        finally:
            if process is not None:
                subprocess.run(["fusermount3", "-u", "-z", str(mount)], capture_output=True, timeout=10)
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            log.close()


if __name__ == "__main__":
    main()
