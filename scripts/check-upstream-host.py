#!/usr/bin/env python3
"""Verify upstream provenance and the explicitly maintained inspection patches.

This is a source-integrity contract. Behavioral tests separately prove that
inspection preserves transaction results, fees and metering.
"""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
for root in sorted((ROOT / "crates").glob("soroban-sim-host*")):
    upstream = json.loads((root / "UPSTREAM.json").read_text())
    patches = json.loads((root / "PATCHES.json").read_text())
    expected = dict(upstream["files"])
    for name, hashes in patches["files"].items():
        if expected.get(name) != hashes["upstream_sha256"]:
            raise SystemExit(f"{root.name}: patch is not based on the recorded upstream file: {name}")
        expected[name] = hashes["patched_sha256"]
    actual = {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in [root / "build.rs", *sorted((root / "src").rglob("*"))]
        if path.is_file()
    }
    if actual != expected:
        raise SystemExit(f"{root.name}: source differs from upstream + inspection patch manifest; see docs/host-inspection.md")
    print(f"Verified {root.name}: {upstream['crate']} {upstream['version']}, {len(patches['files'])} explicitly patched files")
