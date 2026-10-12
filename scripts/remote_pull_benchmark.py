#!/usr/bin/env python3
"""Compare serial and concurrent XetFS Hub pulls on a pinned public revision."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

REPOSITORY = "xet-team/xet-spec-reference-files"
REVISION = "c4aa3a3f15b1395fff5ce934784bf6c8f2d62de8"
CSV_PATH = "Electric_Vehicle_Population_Data_20250917.csv"
CSV_SHA256 = "f41255b252f776125f2b657136654e4d4d5d2ccf8ef4db0ec186bd1981b69734"


def run(args: list[str]) -> str:
    completed = subprocess.run(args, check=True, capture_output=True, text=True)
    return completed.stdout


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def host_info() -> dict[str, str]:
    host = {
        "system": platform.system(),
        "release": platform.release(),
        "architecture": platform.machine(),
        "model": "unknown",
        "cpu": "unknown",
    }
    if host["system"] == "Darwin":
        for key, name in (("model", "hw.model"), ("cpu", "machdep.cpu.brand_string")):
            result = subprocess.run(["sysctl", "-n", name], capture_output=True, text=True)
            if result.returncode == 0:
                host[key] = result.stdout.strip()
    return host


def one_pull(binary: Path, mode: str, workers: int) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix="xetfs-pull-bench-") as temporary:
        root = Path(temporary)
        repository = root / "repo"
        run([str(binary), "--repo", str(repository), "init"])
        command = [
            str(binary),
            "--repo",
            str(repository),
            "remote",
            "pull",
            REPOSITORY,
            "--repo-type",
            "dataset",
            "--revision",
            REVISION,
        ]
        command.extend(["--workers", str(workers)])
        start = time.perf_counter()
        report = json.loads(run(command))
        elapsed = time.perf_counter() - start

        exported = root / "export.csv"
        run([str(binary), "--repo", str(repository), "get", f"/{CSV_PATH}", str(exported)])
        csv_digest = sha256(exported)
        if csv_digest != CSV_SHA256:
            raise RuntimeError(f"{mode} CSV SHA-256 mismatch: {csv_digest}")
        fsck = json.loads(run([str(binary), "--repo", str(repository), "fsck"]))
        return {
            "mode": mode,
            "workers": workers,
            "pull_seconds": elapsed,
            "pull_report": report,
            "csv_sha256": csv_digest,
            "fsck": fsck,
        }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.trials < 1:
        parser.error("--trials must be positive")

    modes = [
        ("workers_1", 1),
        ("workers_4", 4),
        ("workers_8", 8),
    ]
    results = []
    for trial in range(args.trials):
        rotation = trial % len(modes)
        order = modes[rotation:] + modes[:rotation]
        for mode, workers in order:
            result = one_pull(args.binary.resolve(), mode, workers)
            result["trial"] = trial + 1
            results.append(result)

    medians = {
        mode: statistics.median(
            result["pull_seconds"] for result in results if result["mode"] == mode
        )
        for mode, _ in modes
    }
    baseline = medians["workers_1"]
    parallel = medians["workers_8"]
    output = {
        "status": "passed",
        "date": time.strftime("%Y-%m-%d", time.gmtime()),
        "host": host_info(),
        "binary_sha256": sha256(args.binary.resolve()),
        "repository": REPOSITORY,
        "revision": REVISION,
        "source_revision": args.source_revision,
        "method": (
            "Alternating runs of the same release binary using the bulk snapshot path with one, "
            "four, and eight workers. Each pull uses a fresh local repository; timing covers remote "
            "transfer, local XetFS import, and commit, while CSV export and fsck run outside the timer."
        ),
        "trials": args.trials,
        "modes": medians,
        "one_to_eight_worker_speedup": baseline / parallel,
        "eight_worker_time_reduction_percent": 100 * (baseline - parallel) / baseline,
        "prior_serial_implementation": {
            "status": "excluded",
            "reason": (
                "The previously published binary consistently returned ENOENT while opening "
                "the staged .gitattributes file for this revision; its timing is not comparable."
            ),
        },
        "integrity": {
            "expected_csv_sha256": CSV_SHA256,
            "all_trials_csv_sha256_matched": all(
                result["csv_sha256"] == CSV_SHA256 for result in results
            ),
            "all_trials_fsck_passed": all(
                result["fsck"].get("xorbs_verified", 0) > 0
                and result["fsck"].get("chunks_verified", 0) > 0
                and result["fsck"].get("recipes_verified", 0) > 0
                for result in results
            ),
        },
        "runs": results,
        "scope_note": (
            "Measures client-side download and local import while varying worker count on one host "
            "and public repository. It does not establish throughput superiority over Hugging Face "
            "server-side CAS."
        ),
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + "\n")
    print(
        json.dumps(
            {
                key: output[key]
                for key in (
                    "status",
                    "modes",
                    "one_to_eight_worker_speedup",
                    "eight_worker_time_reduction_percent",
                    "integrity",
                )
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
