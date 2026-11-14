#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Collect actual locked Cargo dependency notices for the Linux binary."""
import json
import re
import subprocess
from pathlib import Path

root = Path(__file__).resolve().parents[1]
rust_version = subprocess.check_output(["rustc", "--version"], text=True).split()[1]
if rust_version != "1.93.1":
    raise SystemExit("Use Rust 1.93.1, or review/update the bundled Rust notices first")
target = re.search(r"^host: (.+)$", subprocess.check_output(
    ["rustc", "-vV"], text=True), re.M).group(1)
metadata = json.loads(subprocess.check_output([
    "cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform",
    target, "--manifest-path", str(root / "daemon/Cargo.toml")], text=True))
resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
sections = [
    "OpenSilexLink third-party notices\n\n"
    "Application source: MIT; see the accompanying copyright file.\n"
    "The following notices cover dependencies of the Rust daemon. All offered\n"
    "license texts are retained; this does not change their licensing choices.\n"
    "Qt/PySide6, Python, USB/IP and system libraries are supplied separately\n"
    "by the operating system and are not bundled in these packages.\n"
]
count = 0
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["id"] not in resolved or package["id"] == metadata["resolve"]["root"]:
        continue
    expression = package.get("license") or ""
    identifiers = set(re.findall(r"[A-Za-z0-9.-]+", expression)) - {"AND", "OR"}
    if not identifiers or not identifiers <= {"MIT", "Apache-2.0", "Unicode-3.0", "Unlicense"}:
        raise SystemExit(f"Review new license before release: {package['name']}: {expression}")
    directory = Path(package["manifest_path"]).parent
    notices = sorted(p for p in directory.rglob("*") if p.is_file()
                     and p.name.upper().startswith(("LICENSE", "COPYING", "NOTICE")))
    if not notices:
        raise SystemExit(f"Missing license text: {package['name']}")
    sections.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\n"
                    f"Declared license: {expression}\n"
                    f"Source: {package.get('repository') or 'https://crates.io/crates/' + package['name']}\n")
    for path in notices:
        sections.append(f"\n--- {path.relative_to(directory)} ---\n{path.read_text()}\n")
    count += 1
sections.append("\n" + "=" * 72 + "\nRust standard library / toolchain notices\n"
                "The following distribution notice also lists components not linked\n"
                "into this application. Rust version: " + rust_version + "\n\n" +
                (root / "LICENSES/Rust-1.93.1-copyright.txt").read_text())
output = root / "packaging/build/THIRD_PARTY_NOTICES.txt"
output.parent.mkdir(parents=True, exist_ok=True)
output.write_text("\n".join(sections))
output.chmod(0o644)
print(f"Collected notices for {count} locked crates and Rust {rust_version}")
