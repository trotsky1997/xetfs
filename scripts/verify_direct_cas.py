#!/usr/bin/env python3
"""Verify XetFS direct CAS full-file and ranged reads against a pinned public file."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import subprocess
import tempfile
import time
from pathlib import Path

REPOSITORY = "xet-team/xet-spec-reference-files"
REVISION = "c4aa3a3f15b1395fff5ce934784bf6c8f2d62de8"
FILE_PATH = "Electric_Vehicle_Population_Data_20250917.csv"
FILE_SIZE = 63_527_244
FILE_SHA256 = "f41255b252f776125f2b657136654e4d4d5d2ccf8ef4db0ec186bd1981b69734"
RANGE_OFFSET = 1_048_576
RANGE_LENGTH = 65_536


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def command(binary: Path, offset: int, length: int) -> list[str]:
    return [
        str(binary),
        "remote",
        "cat",
        REPOSITORY,
        FILE_PATH,
        "--repo-type",
        "dataset",
        "--revision",
        REVISION,
        "--offset",
        str(offset),
        "--length",
        str(length),
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()

    with tempfile.TemporaryDirectory(prefix="xetfs-direct-cas-") as temporary:
        full_path = Path(temporary) / "full.csv"
        start = time.perf_counter()
        with full_path.open("wb") as destination:
            subprocess.run(command(binary, 0, FILE_SIZE), check=True, stdout=destination)
        full_seconds = time.perf_counter() - start
        actual_size = full_path.stat().st_size
        actual_sha256 = sha256(full_path)
        if actual_size != FILE_SIZE or actual_sha256 != FILE_SHA256:
            raise RuntimeError(
                f"full CAS read mismatch: bytes={actual_size}, sha256={actual_sha256}"
            )

        start = time.perf_counter()
        ranged = subprocess.check_output(
            command(binary, RANGE_OFFSET, RANGE_LENGTH)
        )
        range_seconds = time.perf_counter() - start
        with full_path.open("rb") as source:
            source.seek(RANGE_OFFSET)
            expected_range = source.read(RANGE_LENGTH)
        if ranged != expected_range:
            raise RuntimeError("direct CAS range bytes differ from the full-file read")

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
            "repository": REPOSITORY,
            "revision": REVISION,
            "file_path": FILE_PATH,
            "read_path": "Hub file metadata + Xet read token + CAS v2 reconstruction + signed Xorb ranges",
            "full_file": {
                "bytes": actual_size,
                "sha256": actual_sha256,
                "xet_file_hash_checked_by_client": True,
                "seconds": full_seconds,
            },
            "range": {
                "offset": RANGE_OFFSET,
                "bytes": len(ranged),
                "seconds": range_seconds,
                "matches_full_file_slice": True,
            },
        }
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
