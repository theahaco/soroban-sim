#!/usr/bin/env python3
"""Keep the published packages' test data and WIT byte-identical to their sources.

Run after recapturing a fixture or editing WIT. CI uses --check; it never silently
regenerates release inputs. Copies (not symlinks) also work in Windows checkouts.
"""
import argparse
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]


def assets():
    for name in ("core", "facade", "cli", "component"):
        crate = ROOT / "crates" / f"soroban-sim-{name}"
        for source in sorted((ROOT / "fixtures/parity").glob("*.json")):
            yield source, crate / "fixtures/parity" / source.name
        if name != "component":
            for source in sorted((ROOT / "fixtures/contracts").glob("*.wasm")):
                yield source, crate / "fixtures/contracts" / source.name
        if name == "cli":
            for source in sorted((ROOT / "fixtures/replay").glob("*.json.gz")):
                yield source, crate / "fixtures/replay" / source.name
            for source in sorted((ROOT / "examples/session").glob("*.yaml")):
                yield source, crate / "examples/session" / source.name
            yield ROOT / "packages/web-demo/tests/fixtures/testnet-fork.json", crate / "fixtures/testnet-fork.json"
        if name == "component":
            for source in sorted((ROOT / "wit").rglob("*")):
                if source.is_file():
                    yield source, crate / "wit" / source.relative_to(ROOT / "wit")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    failures = []
    expected = set()
    for source, destination in assets():
        expected.add(destination)
        if args.check:
            if not destination.is_file() or source.read_bytes() != destination.read_bytes():
                failures.append(str(destination.relative_to(ROOT)))
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
    for name in ("core", "facade", "cli", "component"):
        crate = ROOT / "crates" / f"soroban-sim-{name}"
        for directory in (crate / "fixtures", crate / "wit", crate / "examples/session"):
            for destination in directory.rglob("*"):
                if destination.is_file() and destination not in expected:
                    if args.check:
                        failures.append(f"stale: {destination.relative_to(ROOT)}")
                    else:
                        destination.unlink()
    if failures:
        raise SystemExit("Package assets are out of sync; run scripts/sync-package-assets.py:\n" + "\n".join(failures))
    print(f"{'Verified' if args.check else 'Synchronized'} {len(expected)} package assets")


if __name__ == "__main__":
    main()
