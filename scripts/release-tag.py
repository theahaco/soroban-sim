#!/usr/bin/env python3
"""Print release metadata; reject a tag that disagrees with the CLI manifest."""
import json
import os
import subprocess

metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"]))
package = next(p for p in metadata["packages"] if p["name"] == "soroban-sim-cli")
version = package["version"]
tag = f"{package['name']}-v{version}"
ref = os.environ.get("GITHUB_REF", "")
if ref.startswith("refs/tags/") and ref != f"refs/tags/{tag}":
    raise SystemExit(f"Ref {ref} does not match manifest tag {tag}")
print(f"version={version}\ntag={tag}")
