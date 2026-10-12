#!/usr/bin/env python3
"""Verify against the pinned, Apache-2.0 licensed official Xet reference data."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import urllib.request

REVISION = "c4aa3a3f15b1395fff5ce934784bf6c8f2d62de8"
BASE = f"https://huggingface.co/datasets/xet-team/xet-spec-reference-files/resolve/{REVISION}/"
CSV = "Electric_Vehicle_Population_Data_20250917.csv"
XORB = "eea25d6ee393ccae385820daed127b96ef0ea034dfb7cf6da3a950ce334b7632"
FILE_HASH = "118a53328412787fee04011dcf82fdc4acf3a4a1eddec341c910d30a306aaf97"


def digest(path):
    hasher = hashlib.sha256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--cache", type=Path)
    parser.add_argument("--csv", type=Path)
    parser.add_argument("--chunks", type=Path)
    parser.add_argument("--xorb", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="xetfs-reference-") as tmp:
        cache = args.cache or Path(tmp)
        cache.mkdir(parents=True, exist_ok=True)

        def fetch(name, supplied):
            if supplied:
                return supplied
            target = cache / name
            if not target.exists():
                staging = target.with_suffix(target.suffix + ".download")
                urllib.request.urlretrieve(BASE + name, staging)
                staging.replace(target)
            return target

        source = fetch(CSV, args.csv)
        chunks = fetch(CSV + ".chunks", args.chunks)
        xorb = fetch(XORB + ".xorb", args.xorb)
        binary = str(args.binary.resolve())
        actual = json.loads(subprocess.check_output([binary, "hash", str(source), "--chunks"]))
        expected = [dict(hash=h, length=int(n)) for h, n in (line.split() for line in chunks.read_text().splitlines())]
        assert actual["chunks"] == expected, "CDC boundaries or chunk hashes differ from the official sample"
        assert actual["file_hash"] == FILE_HASH
        assert actual["xorb_hash"] == XORB
        output = Path(tmp) / "reconstructed.csv"
        decoded = json.loads(subprocess.check_output([binary, "inspect-xorb", str(xorb), "--output", str(output)]))
        assert decoded["xorb_hash"] == XORB
        assert decoded["chunks"] == len(expected)
        assert digest(output) == digest(source), "official Xorb reconstruction differs"
        report = dict(status="passed", reference_revision=REVISION, bytes=actual["bytes"],
                      chunks_matched=len(expected), file_hash=FILE_HASH, xorb_hash=XORB,
                      official_xorb_decoded=True, reconstructed_sha256=digest(output))
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
