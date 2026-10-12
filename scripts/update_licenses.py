#!/usr/bin/env python3
"""Regenerate the bundled Cargo license notices with normalized text output."""

from pathlib import Path
import subprocess
import sys


root = Path(__file__).resolve().parents[1]
result = subprocess.run(
    ["cargo", "about", "generate", "about.hbs", "--locked", "--offline"],
    cwd=root,
    capture_output=True,
    text=True,
)
if result.returncode:
    sys.stderr.write(result.stderr)
    raise SystemExit(result.returncode)
text = result.stdout.replace("\r\n", "\n").replace("\r", "\n")
text = "\n".join(line.rstrip(" \t") for line in text.splitlines()).rstrip("\n") + "\n"
with (root / "THIRD_PARTY_LICENSES.txt").open("w", encoding="utf-8", newline="\n") as output:
    output.write(text)
