#!/usr/bin/env python3
"""Compare XetFS direct CAS reads with the official Hugging Face CLI."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

REPOSITORY = "xet-team/xet-spec-reference-files"
REVISION = "c4aa3a3f15b1395fff5ce934784bf6c8f2d62de8"
FILE_PATH = "Electric_Vehicle_Population_Data_20250917.csv"
FILE_SIZE = 63_527_244
FILE_SHA256 = "f41255b252f776125f2b657136654e4d4d5d2ccf8ef4db0ec186bd1981b69734"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def isolated_environment(home: Path) -> dict[str, str]:
    environment = os.environ.copy()
    for name in (
        "HF_TOKEN",
        "HF_TOKEN_PATH",
        "HUGGING_FACE_HUB_TOKEN",
        "HF_HUB_DISABLE_XET",
        "HF_HUB_ENABLE_HF_TRANSFER",
    ):
        environment.pop(name, None)
    environment.update(
        {
            "HF_HOME": str(home),
            "HF_HUB_CACHE": str(home / "hub"),
            "HF_XET_CACHE": str(home / "xet"),
            "HF_HUB_DISABLE_IMPLICIT_TOKEN": "1",
        }
    )
    return environment


def verify(path: Path) -> None:
    size = path.stat().st_size
    digest = sha256(path)
    if size != FILE_SIZE or digest != FILE_SHA256:
        raise RuntimeError(f"download mismatch: bytes={size}, sha256={digest}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--xetfs-workers", type=int, default=1)
    parser.add_argument("--official-workers", type=int, default=8)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 1 or args.runs > 10:
        parser.error("--runs must be between 1 and 10")
    if args.xetfs_workers < 1 or args.xetfs_workers > 64:
        parser.error("--xetfs-workers must be between 1 and 64")
    if args.official_workers < 1 or args.official_workers > 64:
        parser.error("--official-workers must be between 1 and 64")
    binary = args.binary.resolve()
    cli_version = subprocess.run(
        ["hf", "--version"], capture_output=True, text=True, check=True
    ).stdout.strip()
    results: list[dict[str, object]] = []

    with tempfile.TemporaryDirectory(prefix="xetfs-cas-benchmark-") as temporary:
        root = Path(temporary)
        for run in range(args.runs):
            order = ["xetfs_direct_cas", "huggingface_cli"]
            if run % 2:
                order.reverse()
            for client in order:
                case_dir = root / f"run-{run + 1}-{client}"
                case_dir.mkdir()
                environment = isolated_environment(case_dir / "hf-home")
                output = case_dir / "payload.csv"
                start = time.perf_counter()
                if client == "xetfs_direct_cas":
                    command = [
                        str(binary),
                        "remote",
                        "cat",
                        REPOSITORY,
                        FILE_PATH,
                        "--repo-type",
                        "dataset",
                        "--revision",
                        REVISION,
                        "--length",
                        str(FILE_SIZE),
                        "--workers",
                        str(args.xetfs_workers),
                    ]
                    with output.open("wb") as destination:
                        subprocess.run(
                            command,
                            check=True,
                            stdout=destination,
                            env=environment,
                        )
                else:
                    local_dir = case_dir / "local"
                    command = [
                        "hf",
                        "download",
                        REPOSITORY,
                        FILE_PATH,
                        "--repo-type",
                        "dataset",
                        "--revision",
                        REVISION,
                        "--local-dir",
                        str(local_dir),
                        "--force-download",
                        "--max-workers",
                        str(args.official_workers),
                        "--quiet",
                    ]
                    subprocess.run(
                        command,
                        check=True,
                        capture_output=True,
                        env=environment,
                    )
                    output = local_dir / FILE_PATH
                seconds = time.perf_counter() - start
                verify(output)
                results.append(
                    {
                        "run": run + 1,
                        "client": client,
                        "seconds": seconds,
                        "bytes": output.stat().st_size,
                        "sha256": sha256(output),
                    }
                )

    direct = [row["seconds"] for row in results if row["client"] == "xetfs_direct_cas"]
    official = [row["seconds"] for row in results if row["client"] == "huggingface_cli"]
    direct_median = statistics.median(direct)
    official_median = statistics.median(official)
    report = {
        "status": "passed",
        "date": time.strftime("%Y-%m-%d", time.gmtime()),
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "architecture": platform.machine(),
        },
        "source_revision": args.source_revision,
        "xetfs_binary_sha256": sha256(binary),
        "official_cli_version": cli_version,
        "repository": REPOSITORY,
        "revision": REVISION,
        "file_path": FILE_PATH,
        "bytes": FILE_SIZE,
        "expected_sha256": FILE_SHA256,
        "comparison": "XetFS remote cat direct CAS vs official hf download, fresh isolated caches, alternating order",
        "xetfs_workers": args.xetfs_workers,
        "official_workers": args.official_workers,
        "runs": results,
        "median_seconds": {
            "xetfs_direct_cas": direct_median,
            "huggingface_cli": official_median,
        },
        "direct_cas_speedup_vs_official_cli": official_median / direct_median,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
