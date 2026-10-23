#!/usr/bin/env python3
"""Verify the public archive contract consumed by cargo-binstall."""
import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import zipfile

archives = [Path(os.environ["ARCHIVE"])]
if os.environ.get("ZIP_ARCHIVE"):
    archives.append(Path(os.environ["ZIP_ARCHIVE"]))
checksums = {}
for line in Path(os.environ["CHECKSUM"]).read_text().splitlines():
    digest, name = line.split(maxsplit=1)
    checksums[Path(name.lstrip("* ")).name] = digest
for archive in archives:
    if hashlib.sha256(archive.read_bytes()).hexdigest() != checksums.get(archive.name):
        raise SystemExit(f"Missing or incorrect checksum for {archive}")
    windows = "windows" in archive.name
    executable = "soroban-sim.exe" if windows else "soroban-sim"
    with tempfile.TemporaryDirectory() as directory:
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as packed:
                names = packed.namelist()
                if names != [executable]:
                    raise SystemExit(f"Unexpected archive layout: {names}")
                packed.extractall(directory)
        else:
            with tarfile.open(archive) as packed:
                members = packed.getmembers()
                if len(members) != 1 or members[0].name != executable or not members[0].isfile():
                    raise SystemExit(f"Unexpected archive layout: {[m.name for m in members]}")
                packed.extractall(directory, filter="data")
        if os.environ.get("SMOKE") == "true":
            binary = Path(directory) / executable
            binary.chmod(0o755)
            actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
            expected = f"soroban-sim {os.environ['VERSION']}"
            if actual != expected:
                raise SystemExit(f"Expected {expected!r}, got {actual!r}")
            subprocess.run([str(binary), "--help"], check=True)
    print(f"Verified {archive}")
