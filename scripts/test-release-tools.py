#!/usr/bin/env python3
"""Exercise release-tool success and failure paths through their executable interfaces."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]


class ReleaseTools(unittest.TestCase):
    def invoke(self, script, env):
        return subprocess.run([sys.executable, str(ROOT / "scripts" / script)], cwd=ROOT,
                              env=dict(os.environ, **env), text=True, capture_output=True)

    def test_manifest_tag_accepts_matching_ref_and_rejects_wrong_version(self):
        initial = self.invoke("release-tag.py", {"GITHUB_REF": "refs/pull/1/merge"})
        self.assertEqual(initial.returncode, 0, initial.stderr)
        tag = dict(line.split("=", 1) for line in initial.stdout.splitlines())["tag"]
        good = self.invoke("release-tag.py", {"GITHUB_REF": f"refs/tags/{tag}"})
        self.assertEqual(good.returncode, 0, good.stderr)
        bad = self.invoke("release-tag.py", {"GITHUB_REF": "refs/tags/soroban-sim-cli-v999.0.0"})
        self.assertNotEqual(bad.returncode, 0)

    def archive(self, directory, binary_name="soroban-sim", zip_format=False):
        target = "x86_64-pc-windows-msvc" if zip_format else "x86_64-unknown-linux-gnu"
        suffix = ".zip" if zip_format else ".tar.gz"
        archive = Path(directory) / f"soroban-sim-cli-v0.1.0-{target}{suffix}"
        binary = b'#!/usr/bin/env python3\nimport sys\nprint("soroban-sim 0.1.0" if "--version" in sys.argv else "usage: soroban-sim")\n'
        if zip_format:
            with zipfile.ZipFile(archive, "w") as packed:
                packed.writestr(binary_name, binary)
        else:
            with tarfile.open(archive, "w:gz") as packed:
                member = tarfile.TarInfo(binary_name)
                member.size, member.mode = len(binary), 0o755
                packed.addfile(member, io.BytesIO(binary))
        checksum = Path(directory) / "archive.sha256"
        checksum.write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        return archive, checksum

    def test_archive_accepts_root_binary_and_checks_native_version(self):
        with tempfile.TemporaryDirectory() as directory:
            archive, checksum = self.archive(directory)
            env = {"ARCHIVE": str(archive), "ZIP_ARCHIVE": "", "CHECKSUM": str(checksum), "VERSION": "0.1.0", "SMOKE": "true"}
            good = self.invoke("check-cli-archive.py", env)
            self.assertEqual(good.returncode, 0, good.stderr)
            env["VERSION"] = "0.2.0"
            self.assertNotEqual(self.invoke("check-cli-archive.py", env).returncode, 0)
            env["VERSION"] = "0.1.0"
            archive.write_bytes(archive.read_bytes() + b"corruption")
            self.assertNotEqual(self.invoke("check-cli-archive.py", env).returncode, 0)

    def test_archive_rejects_nested_binary_even_with_correct_checksum(self):
        with tempfile.TemporaryDirectory() as directory:
            archive, checksum = self.archive(directory, "nested/soroban-sim")
            result = self.invoke("check-cli-archive.py", {"ARCHIVE": str(archive), "ZIP_ARCHIVE": "", "CHECKSUM": str(checksum), "SMOKE": "false"})
            self.assertNotEqual(result.returncode, 0)

    def test_windows_zip_contract(self):
        with tempfile.TemporaryDirectory() as directory:
            archive, checksum = self.archive(directory, "soroban-sim.exe", zip_format=True)
            result = self.invoke("check-cli-archive.py", {"ARCHIVE": str(archive), "ZIP_ARCHIVE": "", "CHECKSUM": str(checksum), "SMOKE": "false"})
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_binstall_metadata_maps_to_release_archives(self):
        metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], cwd=ROOT))
        package = next(p for p in metadata["packages"] if p["name"] == "soroban-sim-cli")
        install = package["metadata"]["binstall"]
        for target, suffix in [("aarch64-apple-darwin", ".tar.gz"), ("x86_64-pc-windows-msvc", ".zip"), ("aarch64-pc-windows-msvc", ".zip")]:
            values = {"repo": package["repository"], "name": package["name"], "version": package["version"], "target": target, "archive-suffix": suffix}
            url = install["pkg-url"]
            for key, value in values.items():
                url = url.replace("{ " + key + " }", value)
            tag = f"{package['name']}-v{package['version']}"
            self.assertEqual(url, f"{package['repository']}/releases/download/{tag}/{tag}-{target}{suffix}")
            fmt = install.get("overrides", {}).get(target, {}).get("pkg-fmt", install["pkg-fmt"])
            self.assertEqual(fmt, "zip" if suffix == ".zip" else "tgz")


if __name__ == "__main__":
    unittest.main()
