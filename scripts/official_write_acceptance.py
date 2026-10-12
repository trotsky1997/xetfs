#!/usr/bin/env python3
"""Measure an official Hugging Face Xet upload and verify its Hub readback."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import random
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

import huggingface_hub
from huggingface_hub import HfApi, hf_hub_download


def write_payload(path: Path, size: int, seed: int) -> str:
    digest = hashlib.sha256()
    rng = random.Random(seed)
    with path.open("wb") as destination:
        remaining = size
        while remaining:
            block = rng.randbytes(min(1024 * 1024, remaining))
            destination.write(block)
            digest.update(block)
            remaining -= len(block)
    return digest.hexdigest()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo_id")
    parser.add_argument("--size-mib", type=int, default=12)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--revision", default="main")
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if args.size_mib < 11 or args.size_mib > 64:
        parser.error("--size-mib must be between 11 and 64 so the file uses the Xet path")

    size = args.size_mib * 1024 * 1024
    run_id = f"{datetime.now(timezone.utc):%Y%m%dT%H%M%SZ}-{uuid.uuid4().hex[:10]}"
    path_in_repo = f"xetfs-acceptance/official-{run_id}/payload.bin"
    api = HfApi()
    before = api.dataset_info(args.repo_id, files_metadata=True)

    with tempfile.TemporaryDirectory(prefix="xetfs-official-write-") as temporary:
        payload = Path(temporary) / "payload.bin"
        expected_sha256 = write_payload(payload, size, args.seed)

        upload_started = time.perf_counter()
        commit = api.upload_file(
            path_or_fileobj=str(payload),
            path_in_repo=path_in_repo,
            repo_id=args.repo_id,
            repo_type="dataset",
            revision=args.revision,
            commit_message=f"Official Hugging Face Xet write acceptance {run_id}",
        )
        upload_seconds = time.perf_counter() - upload_started
        commit_oid = getattr(commit, "oid", None)
        if not commit_oid:
            raise RuntimeError("official Hub upload returned no commit OID")

        download_started = time.perf_counter()
        downloaded = Path(
            hf_hub_download(
                repo_id=args.repo_id,
                filename=path_in_repo,
                repo_type="dataset",
                revision=commit_oid,
                force_download=True,
            )
        )
        download_seconds = time.perf_counter() - download_started
        actual_sha256 = sha256(downloaded)
        if downloaded.stat().st_size != size or actual_sha256 != expected_sha256:
            raise RuntimeError("official Hugging Face download did not match uploaded bytes")

    after = api.dataset_info(args.repo_id, revision=commit_oid, files_metadata=True)
    remote_file = next(
        (item for item in (after.siblings or []) if item.rfilename == path_in_repo), None
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
        "writer": "huggingface_hub.HfApi.upload_file with hf_xet",
        "huggingface_hub_version": huggingface_hub.__version__,
        "repository": args.repo_id,
        "repository_private": before.private,
        "initial_revision": before.sha,
        "commit_oid": commit_oid,
        "path": path_in_repo,
        "bytes": size,
        "seed": args.seed,
        "sha256": expected_sha256,
        "official_upload_seconds": upload_seconds,
        "remote_file_size": remote_file.size,
        "official_hf_download_readback": {
            "passed": True,
            "sha256": actual_sha256,
            "elapsed_seconds": download_seconds,
        },
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
