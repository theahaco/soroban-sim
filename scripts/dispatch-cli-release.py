#!/usr/bin/env python3
"""Dispatch CLI binaries from release-plz's successful release output."""
import json
import os
import re
import subprocess

releases = json.loads(os.environ.get("RELEASES") or "[]")
if not isinstance(releases, list):
    raise SystemExit("Expected a release-plz releases array")

tags = set()
for release in releases:
    if release["package_name"] != "soroban-sim-cli":
        continue
    tag = release["tag"]
    if tag != f"soroban-sim-cli-v{release['version']}" or not re.fullmatch(
        r"soroban-sim-cli-v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", tag
    ):
        raise SystemExit(f"Unexpected CLI release tag: {tag}")
    tags.add(tag)

for tag in sorted(tags):
    subprocess.run([
        "gh", "workflow", "run", "cd-soroban-sim.yml",
        "--repo", os.environ["GITHUB_REPOSITORY"], "--ref", tag,
    ], check=True)
    print(f"Dispatched CLI binaries for {tag}")
