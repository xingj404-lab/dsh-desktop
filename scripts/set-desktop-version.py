#!/usr/bin/env python3
"""Keep every desktop-version source in sync."""

import json
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def replace_cargo_version(path: Path, version: str) -> None:
    text = path.read_text()
    updated, count = re.subn(
        r'(?m)^(\[package\]\nname = "dsh-desktop"\nversion = ")[^"]+("$)',
        rf"\g<1>{version}\2",
        text,
        count=1,
    )
    if count != 1:
        raise RuntimeError(f"could not find dsh-desktop package version in {path}")
    path.write_text(updated)


def main() -> None:
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} MAJOR.MINOR.PATCH")
    version = sys.argv[1]

    for relative in ("package.json", "package-lock.json"):
        path = ROOT / relative
        data = json.loads(path.read_text())
        data["version"] = version
        if relative == "package-lock.json":
            data["packages"][""]["version"] = version
        path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")

    config_path = ROOT / "src-tauri/tauri.conf.json"
    config = json.loads(config_path.read_text())
    config["version"] = version
    config_path.write_text(json.dumps(config, ensure_ascii=False, indent=2) + "\n")

    replace_cargo_version(ROOT / "src-tauri/Cargo.toml", version)
    replace_cargo_version(ROOT / "src-tauri/Cargo.lock", version)


if __name__ == "__main__":
    main()
