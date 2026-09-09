#!/usr/bin/env python3

import importlib
import json
import os
import stat
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SIGN_SCRIPT = ROOT / "scripts" / "sign-macos-release.sh"
RELEASE_SCRIPT = ROOT / "scripts" / "release_notarization.py"
TAG = "v9.8.7"
COMMIT = "a" * 40
TEAM_ID = "H6HYYFV7JW"
IDENTIFIER = "dev.codexify"


def release_module():
    if not RELEASE_SCRIPT.exists():
        raise AssertionError("release notarization script must exist")
    return importlib.import_module("scripts.release_notarization")


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
                TEAM_ID,
                IDENTIFIER,
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
            f"--force --sign Developer ID Application: Example (H6HYYFV7JW) --identifier {IDENTIFIER} --options runtime --timestamp {self.binary}",
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


class FakeStageServices:
    def __init__(self, release=None) -> None:
        self.release = release
        self.events = []
        self.submissions = iter(
            [
                "11111111-1111-1111-1111-111111111111",
                "22222222-2222-2222-2222-222222222222",
            ]
        )

    def resolve_tag_commit(self, repo, tag):
        self.events.append(("resolve_tag", repo, tag))
        return COMMIT

    def find_release(self, repo, tag):
        self.events.append(("find_release", repo, tag))
        return self.release

    def create_draft(self, repo, tag, commit):
        self.events.append(("create_draft", repo, tag, commit))
        self.release = {
            "id": 77,
            "tag_name": tag,
            "target_commitish": commit,
            "draft": True,
            "prerelease": False,
            "assets": [],
        }
        return self.release

    def delete_asset(self, repo, asset_id):
        self.events.append(("delete_asset", repo, asset_id))

    def upload_public_assets(self, repo, tag, paths):
        self.events.append(("upload_public", repo, tag, tuple(path.name for path in paths)))

    def verify_signed_binary(self, binary, team_id, identifier):
        self.events.append(("verify_signature", binary.read_bytes(), team_id, identifier))

    def create_notary_zip(self, binary, destination):
        self.events.append(("create_notary_zip", binary.name, destination.name))
        destination.write_bytes(b"notary-zip")

    def submit_notarization(self, archive, credentials, webhook_url):
        submission_id = next(self.submissions)
        self.events.append(
            (
                "submit",
                archive.name,
                credentials.key_id,
                credentials.issuer,
                webhook_url,
                submission_id,
            )
        )
        return submission_id

    def upload_internal_asset(self, repo, tag, path):
        self.events.append(("upload_internal", repo, tag, path.name, path.read_text()))

    def dispatch_finalizer(self, repo, tag):
        self.events.append(("dispatch", repo, tag))


class StageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        self.notary_key = self.root / "AuthKey_TEST.p8"
        self.notary_key.write_text("private-key-placeholder")
        self._create_archives()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def _create_archives(self) -> None:
        for platform in ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64"]:
            archive = self.artifacts / f"codexify-{TAG}-{platform}.tar.gz"
            payload = self.root / f"codexify-{platform}"
            stage = payload / f"codexify-{TAG}-{platform}"
            stage.mkdir(parents=True)
            binary = stage / "codexify"
            binary.write_bytes(f"binary-{platform}".encode())
            binary.chmod(0o755)
            with tarfile.open(archive, "w:gz") as tar:
                tar.add(stage, arcname=stage.name)
        windows = self.artifacts / f"codexify-{TAG}-windows-x64.zip"
        with zipfile.ZipFile(windows, "w") as archive:
            archive.writestr(f"codexify-{TAG}-windows-x64/codexify.exe", b"windows")

    def options(self):
        module = release_module()
        return module.StageOptions(
            repo="devnoname120/codexify",
            tag=TAG,
            commit=COMMIT,
            artifacts_dir=self.artifacts,
            credentials=module.NotaryCredentials(
                key_path=self.notary_key,
                key_id="KEY1234567",
                issuer="33333333-3333-3333-3333-333333333333",
            ),
            webhook_url="https://notary.example/apple/secret",
            team_id=TEAM_ID,
            identifier=IDENTIFIER,
        )

    def test_expected_public_assets_are_exact(self) -> None:
        module = release_module()
        self.assertEqual(
            set(module.expected_public_asset_names(TAG)),
            {
                f"codexify-{TAG}-linux-x64.tar.gz",
                f"codexify-{TAG}-linux-arm64.tar.gz",
                f"codexify-{TAG}-darwin-x64.tar.gz",
                f"codexify-{TAG}-darwin-arm64.tar.gz",
                f"codexify-{TAG}-windows-x64.zip",
                "checksums.txt",
            },
        )

    def test_notary_submission_command_is_asynchronous_and_uses_webhook(self) -> None:
        module = release_module()
        credentials = self.options().credentials
        command = module.notary_submit_command(
            Path("payload.zip"),
            credentials,
            "https://notary.example/apple/secret",
        )
        self.assertEqual(command[:3], ["xcrun", "notarytool", "submit"])
        self.assertIn("--no-wait", command)
        self.assertIn("--webhook", command)
        self.assertIn("https://notary.example/apple/secret", command)
        self.assertIn("--output-format", command)
        self.assertIn("json", command)
        self.assertNotIn("--wait", command)

    def test_stage_creates_draft_submits_both_architectures_then_dispatches(self) -> None:
        module = release_module()
        services = FakeStageServices()
        manifest = module.stage_release(self.options(), services)
        names = {asset["name"] for asset in manifest["assets"]}
        self.assertEqual(names, set(module.expected_public_asset_names(TAG)))
        self.assertEqual(manifest["schemaVersion"], 1)
        self.assertEqual(manifest["releaseId"], 77)
        self.assertEqual(manifest["tag"], TAG)
        self.assertEqual(manifest["commit"], COMMIT)
        self.assertEqual(set(manifest["submissions"]), {"darwin-x64", "darwin-arm64"})
        event_names = [event[0] for event in services.events]
        self.assertLess(event_names.index("upload_public"), event_names.index("submit"))
        self.assertLess(event_names.index("submit"), event_names.index("upload_internal"))
        self.assertLess(event_names.index("upload_internal"), event_names.index("dispatch"))
        self.assertEqual(event_names.count("submit"), 2)
        uploaded = next(event for event in services.events if event[0] == "upload_public")
        self.assertEqual(set(uploaded[3]), set(module.expected_public_asset_names(TAG)))
        internal = next(event for event in services.events if event[0] == "upload_internal")
        self.assertEqual(internal[3], "codexify-notarization.json")
        self.assertEqual(json.loads(internal[4]), manifest)

    def test_stage_removes_old_manifest_before_replacing_draft_assets(self) -> None:
        module = release_module()
        services = FakeStageServices(
            {
                "id": 77,
                "tag_name": TAG,
                "target_commitish": COMMIT,
                "draft": True,
                "prerelease": False,
                "assets": [
                    {"id": 900, "name": "codexify-notarization.json"},
                    {"id": 901, "name": "codexify-notarization-log-darwin-x64.json"},
                ],
            }
        )
        module.stage_release(self.options(), services)
        names = [event[0] for event in services.events]
        first_upload = names.index("upload_public")
        deletions = [i for i, name in enumerate(names) if name == "delete_asset"]
        self.assertEqual(len(deletions), 2)
        self.assertTrue(all(index < first_upload for index in deletions))

    def test_stage_refuses_to_touch_a_published_release(self) -> None:
        module = release_module()
        services = FakeStageServices(
            {
                "id": 77,
                "tag_name": TAG,
                "target_commitish": COMMIT,
                "draft": False,
                "prerelease": False,
                "assets": [],
            }
        )
        with self.assertRaisesRegex(RuntimeError, "already published"):
            module.stage_release(self.options(), services)
        self.assertNotIn("upload_public", [event[0] for event in services.events])


class ReleaseWorkflowTests(unittest.TestCase):
    def test_release_workflow_signs_mac_binaries_and_stages_a_draft_on_macos(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text()
        self.assertIn("MACOS_DEVELOPER_ID_P12_BASE64", workflow)
        self.assertIn("scripts/sign-macos-release.sh", workflow)
        self.assertIn("stage-release:", workflow)
        self.assertIn("python3 scripts/release_notarization.py stage", workflow)
        self.assertIn("APPLE_NOTARY_WEBHOOK_URL", workflow)
        self.assertIn("runs-on: macos-14", workflow)
        self.assertNotIn("softprops/action-gh-release", workflow)
        self.assertLess(workflow.index("scripts/sign-macos-release.sh"), workflow.index("Stage artifacts"))


if __name__ == "__main__":
    unittest.main()
