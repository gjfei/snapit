#!/usr/bin/env python3
"""Fault-inject bundle publication failures without signing or installing anything."""

import os
import shutil
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


class ReleaseRollbackTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="snapit-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.project = self.root / "project"
        scripts = self.project / "scripts"
        assets = self.project / "assets"
        macos = self.project / "macos"
        scripts.mkdir(parents=True)
        assets.mkdir()
        macos.mkdir()
        for name in ("bundle-macos.sh", "bundle-windows.sh", "package-macos.sh"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        (assets / "AppIcon.icns").write_bytes(b"icns")
        (assets / "snapit.ico").write_bytes(b"ico")
        (macos / "Info.plist").write_text(
            '<?xml version="1.0" encoding="UTF-8"?>'
            '<plist version="1.0"><dict>'
            "<key>CFBundleShortVersionString</key><string>0.0.0</string>"
            "<key>CFBundleVersion</key><string>0.0.0</string>"
            "</dict></plist>",
            encoding="utf-8",
        )
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.env = os.environ.copy()
        self.env["PATH"] = str(self.tools) + os.pathsep + self.env["PATH"]
        for name in (
            "MACOS_CERTIFICATE",
            "MACOS_CERTIFICATE_PASSWORD",
            "MACOS_SIGNING_IDENTITY",
            "APPLE_ID",
            "APPLE_TEAM_ID",
            "APPLE_APP_PASSWORD",
        ):
            self.env.pop(name, None)
        self.tool(
            "cargo",
            "printf '%s\\n' '{\"packages\":[{\"name\":\"snapit\",\"version\":\"1.2.3\"}]}'",
        )
        self.tool(
            "uname",
            'if [ "${1:-}" = -s ]; then printf Darwin; else /usr/bin/uname "$@"; fi',
        )
        self.tool("codesign", "exit 0")

    def tool(self, name, body):
        path = self.tools / name
        path.write_text("#!/bin/sh\nset -eu\n" + body + "\n", encoding="utf-8")
        path.chmod(0o755)

    def run_script(self, name, *args):
        return subprocess.run(
            [str(self.project / "scripts" / name), *map(str, args)],
            env=self.env,
            cwd=self.root,
            capture_output=True,
            text=True,
        )

    def test_macos_bundle_restores_previous_app(self):
        out = self.root / "mac"
        previous = out / "SnapIt.app"
        previous.mkdir(parents=True)
        (previous / "old").write_text("bundle", encoding="utf-8")
        binary = self.root / "snapit"
        binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        binary.chmod(0o755)
        self.tool(
            "mv",
            'if [ "${1##*/}" = SnapIt.app ] && [ "${2##*/}" = SnapIt.app ]; then exit 41; fi\n'
            'exec /bin/mv "$@"',
        )

        result = self.run_script("bundle-macos.sh", "--bin", binary, "--out", out)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((previous / "old").read_text(encoding="utf-8"), "bundle")
        self.assertEqual(list(out.glob(".SnapIt-build.*")), [])

    def test_macos_bundle_preserves_backup_when_restore_fails(self):
        out = self.root / "mac"
        previous = out / "SnapIt.app"
        previous.mkdir(parents=True)
        (previous / "old").write_text("bundle", encoding="utf-8")
        binary = self.root / "snapit"
        binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        binary.chmod(0o755)
        self.tool(
            "mv",
            'if [ "${1##*/}" = SnapIt.app ] && [ "${2##*/}" = SnapIt.app ]; then exit 41; fi\n'
            'if [ "${1##*/}" = SnapIt.previous.app ] && [ "${2##*/}" = SnapIt.app ]; then exit 42; fi\n'
            'exec /bin/mv "$@"',
        )

        result = self.run_script("bundle-macos.sh", "--bin", binary, "--out", out)

        self.assertNotEqual(result.returncode, 0)
        backups = list(out.glob(".SnapIt-build.*/SnapIt.previous.app/old"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_text(encoding="utf-8"), "bundle")
        self.assertIn("previous bundle preserved", result.stderr)

    def windows_fixture(self):
        out = self.root / "win"
        previous = out / "SnapIt"
        previous.mkdir(parents=True)
        (previous / "old").write_text("stage", encoding="utf-8")
        header = bytearray(64)
        header[:2] = b"MZ"
        struct.pack_into("<I", header, 0x3C, 64)
        binary = self.root / "snapit.exe"
        binary.write_bytes(header + b"PE\0\0" + struct.pack("<H", 0x8664))
        return out, previous, binary

    def run_windows_bundle(self, out, binary):
        return self.run_script(
            "bundle-windows.sh",
            "--bin",
            binary,
            "--target",
            "x86_64-pc-windows-msvc",
            "--out",
            out,
            "--setup-out",
            self.root / "setup",
        )

    def test_windows_bundle_restores_previous_stage(self):
        out, previous, binary = self.windows_fixture()
        self.tool(
            "mv",
            'if [ "${1##*/}" = SnapIt ] && [ "${2##*/}" = SnapIt ]; then exit 41; fi\n'
            'exec /bin/mv "$@"',
        )

        result = self.run_windows_bundle(out, binary)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((previous / "old").read_text(encoding="utf-8"), "stage")
        self.assertEqual(list(out.glob(".SnapIt-stage.*")), [])

    def test_windows_bundle_preserves_backup_when_restore_fails(self):
        out, _, binary = self.windows_fixture()
        self.tool(
            "mv",
            'if [ "${1##*/}" = SnapIt ] && [ "${2##*/}" = SnapIt ]; then exit 41; fi\n'
            'if [ "${1##*/}" = SnapIt.previous ] && [ "${2##*/}" = SnapIt ]; then exit 42; fi\n'
            'exec /bin/mv "$@"',
        )

        result = self.run_windows_bundle(out, binary)

        self.assertNotEqual(result.returncode, 0)
        backups = list(out.glob(".SnapIt-stage.*/SnapIt.previous/old"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_text(encoding="utf-8"), "stage")
        self.assertIn("previous stage preserved", result.stderr)

    def package_fixture(self):
        app = self.root / "package" / "SnapIt.app"
        (app / "Contents").mkdir(parents=True)
        out = self.root / "package-out"
        out.mkdir()
        (out / "fixture.zip").write_text("old-zip", encoding="utf-8")
        (out / "fixture.dmg").write_text("old-dmg", encoding="utf-8")
        self.tool(
            "ditto",
            'case "$1" in -c) eval "dest=\\${$#}"; printf new-zip > "$dest" ;; '
            '*) mkdir -p "$2"; cp -R "$1/." "$2/" ;; esac',
        )
        self.tool("hdiutil", 'eval "dest=\\${$#}"; printf new-dmg > "$dest"')
        self.tool("sleep", "exit 0")
        return app, out

    def run_package(self, app, out):
        return self.run_script(
            "package-macos.sh", "--app", app, "--out", out, "--name", "fixture"
        )

    def test_macos_package_restores_both_artifacts(self):
        app, out = self.package_fixture()
        self.tool(
            "mv",
            'if [ "${1##*/}" = fixture.dmg ] && [ "${2##*/}" = fixture.dmg ]; then exit 41; fi\n'
            'exec /bin/mv "$@"',
        )

        result = self.run_package(app, out)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((out / "fixture.zip").read_text(encoding="utf-8"), "old-zip")
        self.assertEqual((out / "fixture.dmg").read_text(encoding="utf-8"), "old-dmg")
        self.assertEqual(list(out.glob(".snapit-publish.*")), [])

    def test_macos_package_preserves_backup_when_restore_fails(self):
        app, out = self.package_fixture()
        self.tool(
            "mv",
            'if [ "${1##*/}" = fixture.dmg ] && [ "${2##*/}" = fixture.dmg ]; then exit 41; fi\n'
            'if [ "${1##*/}" = previous.dmg ] && [ "${2##*/}" = fixture.dmg ]; then exit 42; fi\n'
            'exec /bin/mv "$@"',
        )

        result = self.run_package(app, out)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((out / "fixture.zip").read_text(encoding="utf-8"), "old-zip")
        backups = list(out.glob(".snapit-publish.*/previous.dmg"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_text(encoding="utf-8"), "old-dmg")
        self.assertIn("backups preserved", result.stderr)


if __name__ == "__main__":
    unittest.main()
