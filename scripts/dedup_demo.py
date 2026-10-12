#!/usr/bin/env python3
"""Reproducible CLI acceptance and measured deduplication on synthetic data."""
import argparse
import hashlib
import json
from pathlib import Path
import random
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--mib", type=int, default=16)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    assert args.mib >= 1
    binary = str(args.binary.resolve())
    with tempfile.TemporaryDirectory(prefix="xetfs-demo-") as tmp:
        tmp = Path(tmp)
        repo = tmp / "repo"

        def run(*cmd, raw=False):
            out = subprocess.check_output([binary, "--repo", str(repo), *map(str, cmd)])
            return out if raw else (json.loads(out) if out.strip() else None)

        run("init")
        source = tmp / "weights.bin"
        data = random.Random(20261012).randbytes(args.mib * 1024 * 1024)
        source.write_bytes(data)
        start = time.monotonic()
        run("put", source, "/weights-v1")
        initial_seconds = time.monotonic() - start
        baseline = run("stats")
        run("put", source, "/identical")
        duplicate = run("stats")
        assert duplicate["xorb_bytes"] == baseline["xorb_bytes"]
        run("snapshot", "create", "v1")
        insertion = b"xetfs-content-defined-insertion" * 9
        edited = data[:123456] + insertion + data[123456:]
        source.write_bytes(edited)
        start = time.monotonic()
        run("put", source, "/weights-v1")
        edit_seconds = time.monotonic() - start
        after = run("stats")
        actual = run("cat", "/weights-v1", raw=True)
        assert hashlib.sha256(actual).digest() == hashlib.sha256(edited).digest()
        assert run("cat", "/weights-v1", "--snapshot", "v1", raw=True) == data
        assert run("cat", "/weights-v1", "--offset", 65530, "--length", 1024, raw=True) == edited[65530:66554]
        run("fsck")
        added_raw = after["unique_chunk_bytes"] - baseline["unique_chunk_bytes"]
        report = dict(status="passed", dataset="seeded pseudorandom bytes", seed=20261012,
                      original_bytes=len(data), insertion_bytes=len(insertion),
                      identical_copy_added_xorb_bytes=duplicate["xorb_bytes"] - baseline["xorb_bytes"],
                      edited_version_added_unique_raw_bytes=added_raw,
                      edited_version_added_xorb_bytes=after["xorb_bytes"] - baseline["xorb_bytes"],
                      edited_version_reused_fraction=1 - added_raw / len(edited),
                      import_seconds=initial_seconds, edited_import_seconds=edit_seconds,
                      snapshot_readback=True, range_readback=True, fsck=True)
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
