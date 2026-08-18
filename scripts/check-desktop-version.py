#!/usr/bin/env python3
"""Fail when desktop-version sources disagree or differ from an expected tag."""

import json
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    package_version = json.loads((ROOT / "package.json").read_text())["version"]
    lock = json.loads((ROOT / "package-lock.json").read_text())
    lock_version = lock["version"]
    lock_root_version = lock["packages"][""]["version"]
    tauri_version = json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())["version"]
    cargo_versions = {}
    for name in ("Cargo.toml", "Cargo.lock"):
        text = (ROOT / "src-tauri" / name).read_text()
        match = re.search(
            r'(?m)^\[\[?package\]?\]\nname = "dsh-desktop"\nversion = "([^"]+)"$',
            text,
        )
        if not match:
            raise SystemExit(f"could not read dsh-desktop version from {name}")
        cargo_versions[name] = match.group(1)
    versions = {
        "package.json": package_version,
        "package-lock.json": lock_version,
        "package-lock.json root package": lock_root_version,
        "tauri.conf.json": tauri_version,
        **cargo_versions,
    }
    expected = sys.argv[1].removeprefix("v") if len(sys.argv) == 2 else package_version
    mismatches = {name: value for name, value in versions.items() if value != expected}
    if mismatches:
        details = ", ".join(f"{name}={value}" for name, value in mismatches.items())
        raise SystemExit(f"desktop version must be {expected}: {details}")
    print(f"desktop versions are synchronized at {expected}")


if __name__ == "__main__":
    main()
