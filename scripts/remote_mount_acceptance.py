#!/usr/bin/env python3
"""Verify lazy range reads and read-only behavior through a real Linux FUSE mount."""

from __future__ import annotations

import argparse
import errno
import hashlib
import json
import os
import platform
import shutil
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

WINDOW_BYTES = 4 * 1024 * 1024
READ_BYTES = 64 * 1024


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("repo_id")
    parser.add_argument("revision", help="Immutable Hub commit SHA")
    parser.add_argument("prefix", help="Remote repository directory mounted as root")
    parser.add_argument("relative_file", help="Xet-backed file path below prefix")
    parser.add_argument("expected_file", type=Path, help="Officially downloaded reference file")
    parser.add_argument("--repo-type", default="dataset")
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()

    if platform.system() != "Linux":
        parser.error("a Linux kernel with FUSE support is required")
    if not args.expected_file.is_file():
        parser.error(f"reference file does not exist: {args.expected_file}")
    binary = args.binary.resolve()
    expected_file = args.expected_file.resolve()
    relative_parts = Path(args.relative_file).parts
    if not relative_parts or any(part in ("", ".", "..") for part in relative_parts):
        parser.error("relative_file must be a safe path below prefix")
    unmount = shutil.which("fusermount3") or shutil.which("fusermount")
    if not unmount:
        parser.error("fusermount3 is required")

    size = expected_file.stat().st_size
    if size <= READ_BYTES:
        parser.error(f"reference file must be larger than {READ_BYTES} bytes")
    offset = min(size - READ_BYTES, size // 2 + 137)
    with expected_file.open("rb") as reference:
        reference.seek(offset)
        expected_range = reference.read(READ_BYTES)
    reference_sha256 = sha256_file(expected_file)
    if len(expected_range) != READ_BYTES:
        raise RuntimeError("could not read the full expected byte range")

    with tempfile.TemporaryDirectory(prefix="xetfs-remote-mount-") as temporary:
        mountpoint = Path(temporary) / "mount"
        mountpoint.mkdir()
        command = [
            str(binary),
            "remote",
            "mount",
            args.repo_id,
            str(mountpoint),
            "--repo-type",
            args.repo_type,
            "--revision",
            args.revision,
            "--prefix",
            args.prefix,
            "--workers",
            "2",
            "--cache-mib",
            "4",
        ]
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        mounted = False
        try:
            deadline = time.monotonic() + 120
            while time.monotonic() < deadline:
                if os.path.ismount(mountpoint):
                    mounted = True
                    break
                if process.poll() is not None:
                    stdout, stderr = process.communicate()
                    raise RuntimeError(
                        "remote mount exited before mounting: "
                        + (stderr or stdout).decode(errors="replace")[-4000:]
                    )
                time.sleep(0.1)
            if not mounted:
                raise TimeoutError("remote FUSE mount did not become ready within 120 seconds")

            mounted_file = mountpoint.joinpath(*relative_parts)
            if mounted_file.stat().st_size != size:
                raise RuntimeError("mounted file size differs from the official reference")
            with mounted_file.open("rb", buffering=0) as remote:
                remote.seek(offset)
                started = time.perf_counter()
                actual_range = remote.read(READ_BYTES)
                range_seconds = time.perf_counter() - started
            if actual_range != expected_range:
                raise RuntimeError("remote FUSE range differs from the official reference")

            try:
                fd = os.open(mounted_file, os.O_WRONLY)
            except OSError as error:
                if error.errno not in (errno.EROFS, errno.EACCES):
                    raise RuntimeError(
                        f"write failed with errno {error.errno}, expected EROFS or EACCES"
                    ) from error
                write_open_errno = error.errno
            else:
                os.close(fd)
                raise RuntimeError("read-only remote mount allowed a write open")

            report = {
                "status": "passed",
                "date": datetime.now(timezone.utc).date().isoformat(),
                "host": {
                    "system": platform.system(),
                    "release": platform.release(),
                    "architecture": platform.machine(),
                },
                "repository": args.repo_id,
                "repository_type": args.repo_type,
                "revision": args.revision,
                "binary_sha256": sha256_file(binary),
                "path": f"{args.prefix.rstrip('/')}/{args.relative_file}",
                "file_bytes": size,
                "reference_sha256": reference_sha256,
                "range_read": {
                    "offset": offset,
                    "bytes": len(actual_range),
                    "sha256": sha256_bytes(actual_range),
                    "elapsed_seconds": range_seconds,
                    "aligned_window_bytes": min(WINDOW_BYTES, size - offset // WINDOW_BYTES * WINDOW_BYTES),
                    "window_size_bytes": WINDOW_BYTES,
                    "fraction_of_file_windowed": min(
                        WINDOW_BYTES, size - offset // WINDOW_BYTES * WINDOW_BYTES
                    )
                    / size,
                },
                "read_only_enforced": True,
                "write_open_errno": write_open_errno,
                "cache_limit_bytes": WINDOW_BYTES,
                "network_wire_bytes_measured": False,
            }
        finally:
            if mounted:
                subprocess.run([unmount, "-u", str(mountpoint)], check=False)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            if process.returncode not in (0, None):
                _, stderr = process.communicate()
                raise RuntimeError(
                    f"remote FUSE mount exited {process.returncode}: "
                    + stderr.decode(errors="replace")[-4000:]
                )

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
