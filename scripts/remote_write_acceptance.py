#!/usr/bin/env python3
"""Push a synthetic XetFS file to a specified Hub dataset and verify both read paths."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import random
import subprocess
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

from huggingface_hub import HfApi


def run_text(command: list[str]) -> str:
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode:
        detail = (result.stderr or result.stdout).strip()
        raise RuntimeError(
            f"command failed with exit code {result.returncode}: {command[0]}\n{detail[-4000:]}"
        )
    return result.stdout


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_payload(path: Path, size: int) -> str:
    digest = hashlib.sha256()
    rng = random.Random(0x5845544653)
    with path.open("wb") as destination:
        remaining = size
        while remaining:
            block = rng.randbytes(min(1024 * 1024, remaining))
            destination.write(block)
            digest.update(block)
            remaining -= len(block)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("repo_id", help="Target Hugging Face dataset, for example owner/name")
    parser.add_argument("--revision", default="main")
    parser.add_argument("--size-mib", type=int, default=12)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if args.size_mib < 11 or args.size_mib > 64:
        parser.error("--size-mib must be between 11 and 64 so the file uses the Xet path")

    binary = args.binary.resolve()
    size = args.size_mib * 1024 * 1024
    run_id = f"{datetime.now(timezone.utc):%Y%m%dT%H%M%SZ}-{uuid.uuid4().hex[:10]}"
    prefix = f"xetfs-acceptance/{run_id}"
    file_path = f"{prefix}/payload.bin"
    api = HfApi()
    before = api.dataset_info(args.repo_id, files_metadata=True)
    if any(item.rfilename == file_path for item in (before.siblings or [])):
        raise RuntimeError(f"acceptance path already exists: {file_path}")

    with tempfile.TemporaryDirectory(prefix="xetfs-remote-write-") as temporary:
        root = Path(temporary)
        local_repo = root / "local.xetfs"
        payload = root / "payload.bin"
        expected_sha256 = write_payload(payload, size)

        run_text([str(binary), "--repo", str(local_repo), "init"])
        run_text([str(binary), "--repo", str(local_repo), "mkdir", "-p", f"/{prefix}"])
        run_text(
            [
                str(binary),
                "--repo",
                str(local_repo),
                "put",
                str(payload),
                f"/{file_path}",
            ]
        )
        fsck = json.loads(run_text([str(binary), "--repo", str(local_repo), "fsck"]))
        push_started = time.perf_counter()
        push = json.loads(
            run_text(
                [
                    str(binary),
                    "--repo",
                    str(local_repo),
                    "remote",
                    "push",
                    args.repo_id,
                    "--repo-type",
                    "dataset",
                    "--revision",
                    args.revision,
                    "--prefix",
                    prefix,
                    "--message",
                    f"XetFS remote write acceptance {run_id}",
                ]
            )
        )
        native_push_seconds = time.perf_counter() - push_started
        commit_oid = push.get("commit_oid")
        if not commit_oid:
            raise RuntimeError("Hub upload returned no commit identifier")

        cas_readback = root / "cas-readback.bin"
        cas_readback_started = time.perf_counter()
        with cas_readback.open("wb") as destination:
            subprocess.run(
                [
                    str(binary),
                    "remote",
                    "cat",
                    args.repo_id,
                    file_path,
                    "--repo-type",
                    "dataset",
                    "--revision",
                    commit_oid,
                    "--length",
                    str(size),
                ],
                check=True,
                stdout=destination,
            )
        cas_readback_seconds = time.perf_counter() - cas_readback_started
        cas_sha256 = sha256(cas_readback)
        if cas_readback.stat().st_size != size or cas_sha256 != expected_sha256:
            raise RuntimeError("direct CAS readback did not match the uploaded bytes")

        official_dir = root / "official-download"
        official_download_started = time.perf_counter()
        run_text(
            [
                "hf",
                "download",
                args.repo_id,
                file_path,
                "--repo-type",
                "dataset",
                "--revision",
                commit_oid,
                "--local-dir",
                str(official_dir),
                "--force-download",
                "--max-workers",
                "8",
                "--quiet",
            ]
        )
        official_download_seconds = time.perf_counter() - official_download_started
        official_path = official_dir / file_path
        official_sha256 = sha256(official_path)
        if official_path.stat().st_size != size or official_sha256 != expected_sha256:
            raise RuntimeError("official Hugging Face download did not match uploaded bytes")

        after = api.dataset_info(args.repo_id, revision=commit_oid, files_metadata=True)
        remote_file = next(
            (item for item in (after.siblings or []) if item.rfilename == file_path), None
        )
        if remote_file is None or remote_file.size != size:
            raise RuntimeError("Hub metadata did not list the uploaded file at its expected size")

        report = {
            "status": "passed",
            "date": time.strftime("%Y-%m-%d", time.gmtime()),
            "host": {
                "system": platform.system(),
                "release": platform.release(),
                "architecture": platform.machine(),
            },
            "source_revision": args.source_revision,
            "binary_sha256": sha256(binary),
            "repository": args.repo_id,
            "repository_private": before.private,
            "upload_source_strategy": "native Xet CAS transfer from committed XetFS Xorbs and a generated metadata shard",
            "xetfs_full_file_staging_bytes": 0,
            "native_push_seconds": native_push_seconds,
            "initial_revision": before.sha,
            "commit_oid": commit_oid,
            "commit_url": push.get("commit_url"),
            "path": file_path,
            "bytes": size,
            "sha256": expected_sha256,
            "local_fsck": fsck,
            "remote_file_size": remote_file.size,
            "direct_cas_readback": {
                "passed": True,
                "sha256": cas_sha256,
                "elapsed_seconds": cas_readback_seconds,
                "client_checked_xet_file_hash": True,
            },
            "official_hf_download_readback": {
                "passed": True,
                "sha256": official_sha256,
                "elapsed_seconds": official_download_seconds,
            },
        }

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
