#!/usr/bin/env python3
"""Build and test the exact published sources, before any first publication.

Requires Python 3.12+ and Cargo 1.99+. Cargo's workspace packaging verifies the
unpublished dependency chain using its temporary registry. The second pass
tests extracted packages in a separate workspace, with only their packaged
siblings patched in; it cannot accidentally read repository fixtures or WIT.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args, cwd=ROOT, env=None):
    subprocess.run(args, cwd=cwd, env=env, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-dirty", action="store_true", help="For local development only")
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT))
    members = [p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]]
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    if any(p["source"] and p["source"].startswith("git+") for p in metadata["packages"]):
        raise SystemExit("Release dependency graph still contains Git-only sources")
    host = next(p for p in members if p["name"] == "soroban-sim-host")
    if "recording_mode" in nodes[host["id"]]["features"]:
        raise SystemExit("The isolated application host has recording_mode enabled")
    package_args = ["cargo", "package", "--workspace", "--locked"]
    if args.allow_dirty:
        package_args.append("--allow-dirty")
    run(*package_args)
    package_dir = Path(metadata["target_directory"]) / "package"
    with tempfile.TemporaryDirectory(prefix="soroban-sim-packages-") as temporary:
        isolated = Path(temporary)
        directories = {}
        for package in members:
            name = f"{package['name']}-{package['version']}"
            archive = package_dir / f"{name}.crate"
            if archive.stat().st_size >= 10 * 1024 * 1024:
                raise SystemExit(f"Package exceeds crates.io size limit: {archive}")
            with tarfile.open(archive) as packed:
                packed.extractall(isolated, filter="data")
            directories[package["name"]] = name
        workspace = '[workspace]\nresolver = "2"\nmembers = ' + json.dumps(list(directories.values())) + '\n\n[patch.crates-io]\n'
        for package, directory in directories.items():
            workspace += f'{package} = {{ path = "{directory}" }}\n'
        (isolated / "Cargo.toml").write_text(workspace)
        shutil.copyfile(ROOT / "Cargo.lock", isolated / "Cargo.lock")
        # CARGO_MANIFEST_DIR is embedded in test binaries. A shared target cache
        # can reuse a binary pointing at an earlier, deleted extraction folder.
        env = dict(os.environ, CARGO_TARGET_DIR=str(isolated / "target"))
        run("cargo", "test", "--workspace", "--exclude", "soroban-sim-host", "--locked", cwd=isolated, env=env)
        run("cargo", "build", "-p", "soroban-sim-component", "--target", "wasm32-wasip2", "--release", "--locked", cwd=isolated, env=env)
        # Use Cargo's actual install interface against the normalized CLI package.
        destination = isolated / "installed"
        run("cargo", "install", "--path", str(isolated / directories["soroban-sim-cli"]), "--root", str(destination), "--locked", "--debug", cwd=isolated, env=env)
        executable = destination / "bin" / ("soroban-sim.exe" if os.name == "nt" else "soroban-sim")
        run(str(executable), "--version", cwd=isolated)
        run(str(executable), "--help", cwd=isolated)
    print("Verified package builds, extracted tests, WASI component and CLI installation")


if __name__ == "__main__":
    main()
