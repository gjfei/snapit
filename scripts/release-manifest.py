#!/usr/bin/env python3
"""Write latest.json and SHA256SUMS.txt for the release assets in a directory.

The app's updater downloads latest.json from the newest release and picks the
file for its platform from `assets`: the `macos-<arch>-dmg` disk image on macOS
(falling back to the `macos-<arch>` zip), the `windows-<arch>` setup.exe on
Windows.

Usage:
  scripts/release-manifest.py --dir DIR --version 1.2.3 --repo owner/name \
      [--tag v1.2.3] [--notes-file notes.md] [--base-url URL]

Updater packages are SnapIt-<version>-<os>-<arch>.zip, ...-setup.exe and
...-dmg (or ...-dmg.dmg); every other file only goes into SHA256SUMS.txt.
--base-url replaces the GitHub download URL, for testing against a local
server.
"""

import argparse
import datetime
import hashlib
import json
import pathlib
import re
import sys

PACKAGE = (
    r"^SnapIt-{version}-"
    r"(?P<platform>macos-(?:aarch64|x86_64)|windows-(?:x86_64|aarch64))"
    r"(?P<suffix>-setup|-dmg)?"
    r"\.(?P<ext>zip|exe|dmg)$"
)


def asset_key(name: str, pattern: re.Pattern) -> str | None:
    """The manifest key `name` fills, or None when it is not an updater package.

    The key encodes what the file is, not how it happens to be named: the macOS
    disk image is `macos-<arch>-dmg` whether or not the name repeats `-dmg`, and
    the Windows setup.exe stays on the plain platform key.
    """
    match = pattern.match(name)
    if not match:
        return None
    platform, suffix, ext = match["platform"], match["suffix"], match["ext"]
    macos = platform.startswith("macos-")
    if suffix == "-dmg" or ext == "dmg":
        return f"{platform}-dmg" if macos else None
    if suffix == "-setup" or ext == "exe":
        return platform if not macos else None
    return platform


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dir", required=True, type=pathlib.Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag")
    parser.add_argument("--notes-file", type=pathlib.Path)
    parser.add_argument("--base-url")
    args = parser.parse_args()

    tag = args.tag or f"v{args.version}"
    base_url = (
        args.base_url or f"https://github.com/{args.repo}/releases/download/{tag}"
    ).rstrip("/")
    notes = args.notes_file.read_text(encoding="utf-8").strip() if args.notes_file else ""
    pattern = re.compile(PACKAGE.format(version=re.escape(args.version)))

    files = sorted(
        p
        for p in args.dir.iterdir()
        if p.is_file() and p.name not in ("latest.json", "SHA256SUMS.txt")
    )
    assets = {}
    for path in files:
        key = asset_key(path.name, pattern)
        if key is None:
            continue
        if key in assets:
            print(f"error: two packages for {key}", file=sys.stderr)
            return 1
        assets[key] = {
            "url": f"{base_url}/{path.name}",
            "sha256": sha256(path),
            "size": path.stat().st_size,
        }
    if not assets:
        print(f"error: no SnapIt-{args.version}-* packages in {args.dir}", file=sys.stderr)
        return 1

    manifest = {
        "version": args.version,
        "notes": notes,
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "assets": dict(sorted(assets.items())),
    }
    (args.dir / "latest.json").write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    sums = [f"{sha256(p)}  {p.name}" for p in files + [args.dir / "latest.json"]]
    (args.dir / "SHA256SUMS.txt").write_text("\n".join(sums) + "\n", encoding="utf-8")

    for key, asset in manifest["assets"].items():
        print(f"{key}: {asset['url']} ({asset['size']} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
