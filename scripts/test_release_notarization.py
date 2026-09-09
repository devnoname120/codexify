#!/usr/bin/env python3

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SIGN_SCRIPT = ROOT / "scripts" / "sign-macos-release.sh"


class SigningScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.calls = self.root / "codesign-calls.txt"
        self.binary = self.root / "codexify"
        self.binary.write_bytes(b"mach-o-placeholder")
        fake = self.bin_dir / "codesign"
        fake.write_text(
            """#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$FAKE_CODESIGN_CALLS"
case "$*" in
  '--display --verbose=4 '*)
    printf 'Executable=%s\nIdentifier=%s\nTeamIdentifier=%s\n' "$3" "${FAKE_IDENTIFIER:-dev.codexify}" "${FAKE_TEAM_ID:-H6HYYFV7JW}" >&2
    ;;
  '--display --requirements - '*)
    printf 'designated => identifier "%s" and anchor apple generic and certificate leaf[subject.OU] = %s\n' "${FAKE_REQUIREMENT_IDENTIFIER:-dev.codexify}" "${FAKE_REQUIREMENT_TEAM_ID:-H6HYYFV7JW}" >&2
    ;;
esac
"""
        )
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def run_signer(self, **overrides: str) -> subprocess.CompletedProcess[str]:
        self.assertTrue(SIGN_SCRIPT.exists(), "signing script must exist")
        env = os.environ.copy()
        env.update(overrides)
        env["PATH"] = f"{self.bin_dir}:{env['PATH']}"
        env["FAKE_CODESIGN_CALLS"] = str(self.calls)
        return subprocess.run(
            [
                str(SIGN_SCRIPT),
                str(self.binary),
                "Developer ID Application: Example (H6HYYFV7JW)",
                "H6HYYFV7JW",
                "dev.codexify",
            ],
            text=True,
            capture_output=True,
            env=env,
            check=False,
        )

    def test_signs_with_stable_identity_and_hardened_runtime(self) -> None:
        result = self.run_signer()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text()
        self.assertIn(
            f"--force --sign Developer ID Application: Example (H6HYYFV7JW) --identifier dev.codexify --options runtime --timestamp {self.binary}",
            calls,
        )
        self.assertIn(f"--verify --strict --verbose=2 {self.binary}", calls)
        self.assertIn(f"--display --verbose=4 {self.binary}", calls)
        self.assertIn(f"--display --requirements - {self.binary}", calls)

    def test_rejects_wrong_team_identifier(self) -> None:
        result = self.run_signer(FAKE_TEAM_ID="WRONGTEAM1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("TeamIdentifier", result.stderr)

    def test_rejects_incompatible_designated_requirement(self) -> None:
        result = self.run_signer(FAKE_REQUIREMENT_IDENTIFIER="dev.other")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("designated requirement", result.stderr)


if __name__ == "__main__":
    unittest.main()
