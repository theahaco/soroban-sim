#!/usr/bin/env python3
"""Verify the intentionally unchanged upstream source copy against its byte manifest.

This checks provenance/drift, not host behavior. The CLI's offline replay suite
is the independent behavioral check for exact application metering.
"""
import hashlib
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1] / "crates/soroban-sim-host"
upstream = json.loads((root / "UPSTREAM.json").read_text())
actual = {
    str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
    for path in [root / "build.rs", *sorted((root / "src").rglob("*"))]
    if path.is_file()
}
if actual != upstream["files"]:
    raise SystemExit("Compatibility host differs from its upstream byte manifest; see docs/releases.md")
print(f"Verified {len(actual)} unchanged upstream files from {upstream['crate']} {upstream['version']}")
