#!/usr/bin/env python3

import importlib
import hashlib
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

    def test_uses_an_explicit_temporary_keychain_when_provided(self) -> None:
        result = self.run_signer(CODE_SIGN_KEYCHAIN="/tmp/codexify-signing.keychain-db")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text()
        self.assertIn(
            "--force --keychain /tmp/codexify-signing.keychain-db --sign Developer ID Application: Example",
            calls,
        )

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

    def test_stage_accepts_githubs_branch_target_for_an_existing_tag(self) -> None:
        module = release_module()
        services = FakeStageServices(
            {
                "id": 77,
                "tag_name": TAG,
                "target_commitish": "main",
                "draft": True,
                "prerelease": False,
                "assets": [],
            }
        )
        module.stage_release(self.options(), services)
        self.assertIn("upload_public", [event[0] for event in services.events])

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
        self.assertIn("CODE_SIGN_KEYCHAIN", workflow)
        self.assertIn("scripts/sign-macos-release.sh", workflow)
        self.assertIn("stage-release:", workflow)
        self.assertIn("python3 scripts/release_notarization.py stage", workflow)
        self.assertIn("APPLE_NOTARY_WEBHOOK_URL", workflow)
        self.assertIn("runs-on: macos-14", workflow)
        self.assertNotIn("softprops/action-gh-release", workflow)
        self.assertLess(workflow.index("scripts/sign-macos-release.sh"), workflow.index("Stage artifacts"))


class FakeFinalizeServices:
    def __init__(self, release, asset_bytes, statuses, published_tags=()) -> None:
        self.release = release
        self.asset_bytes = dict(asset_bytes)
        self.statuses = dict(statuses)
        self.published_tags = list(published_tags)
        self.events = []

    def list_releases(self, repo):
        self.events.append(("list_releases", repo))
        published = [
            {
                "id": 1000 + index,
                "tag_name": tag,
                "target_commitish": "b" * 40,
                "draft": False,
                "prerelease": False,
                "assets": [],
            }
            for index, tag in enumerate(self.published_tags)
        ]
        return [self.release, *published] if self.release is not None else published

    def download_asset(self, repo, asset, destination):
        self.events.append(("download_asset", repo, asset["name"]))
        destination.write_bytes(self.asset_bytes[asset["name"]])

    def resolve_tag_commit(self, repo, tag):
        self.events.append(("resolve_tag", repo, tag))
        return COMMIT

    def notarization_status(self, submission_id, credentials):
        self.events.append(("notary_status", submission_id, credentials.key_id))
        return self.statuses[submission_id]

    def download_notarization_log(self, submission_id, credentials, destination):
        self.events.append(("notary_log", submission_id, destination.name))
        destination.write_text(json.dumps({"id": submission_id, "issues": ["invalid"]}))

    def upload_internal_asset(self, repo, tag, path):
        self.events.append(("upload_internal", repo, tag, path.name, path.read_bytes()))

    def verify_signed_binary(self, binary, team_id, identifier):
        self.events.append(("verify_signature", binary.read_bytes(), team_id, identifier))

    def delete_asset(self, repo, asset_id):
        self.events.append(("delete_asset", repo, asset_id))

    def publish_release(self, repo, release_id, make_latest):
        self.events.append(("publish", repo, release_id, make_latest))
        self.release["draft"] = False
        self.release["assets"] = [
            asset
            for asset in self.release["assets"]
            if asset["name"] != "codexify-notarization.json"
            and not asset["name"].startswith("codexify-notarization-log-")
        ]
        self.published_tags.append(self.release["tag_name"])


class FinalizeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = release_module()
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        self.notary_key = self.root / "AuthKey_TEST.p8"
        self.notary_key.write_text("private-key-placeholder")
        self._create_archives()
        archive_names = self.module.expected_archive_names(TAG)
        self.module.create_checksums(self.artifacts, archive_names)
        public_paths = [self.artifacts / name for name in self.module.expected_public_asset_names(TAG)]
        self.submission_ids = {
            "darwin-x64": "11111111-1111-1111-1111-111111111111",
            "darwin-arm64": "22222222-2222-2222-2222-222222222222",
        }
        submissions = {
            platform: {
                "id": self.submission_ids[platform],
                "archive": f"codexify-{TAG}-{platform}.tar.gz",
                "binarySha256": hashlib.sha256(f"binary-{platform}".encode()).hexdigest(),
            }
            for platform in self.module.MAC_PLATFORMS
        }
        options = self.module.StageOptions(
            repo="devnoname120/codexify",
            tag=TAG,
            commit=COMMIT,
            artifacts_dir=self.artifacts,
            credentials=self.credentials(),
            webhook_url="https://notary.example/apple/secret",
            team_id=TEAM_ID,
            identifier=IDENTIFIER,
        )
        self.manifest = self.module.build_manifest(options, 77, public_paths, submissions)
        self.asset_bytes = {path.name: path.read_bytes() for path in public_paths}
        self.asset_bytes[self.module.MANIFEST_ASSET] = (
            json.dumps(self.manifest, indent=2, sort_keys=True) + "\n"
        ).encode()
        self.release = {
            "id": 77,
            "tag_name": TAG,
            "target_commitish": "main",
            "draft": True,
            "prerelease": False,
            "assets": [
                {"id": index + 100, "name": name, "size": len(payload)}
                for index, (name, payload) in enumerate(self.asset_bytes.items())
            ],
        }

    def tearDown(self) -> None:
        self.temp.cleanup()

    def credentials(self):
        return self.module.NotaryCredentials(
            key_path=self.notary_key,
            key_id="KEY1234567",
            issuer="33333333-3333-3333-3333-333333333333",
        )

    def options(self, tag=None):
        return self.module.FinalizeOptions(
            repo="devnoname120/codexify",
            credentials=self.credentials(),
            requested_tag=tag,
            team_id=TEAM_ID,
            identifier=IDENTIFIER,
        )

    def _create_archives(self) -> None:
        for platform in ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64"]:
            archive = self.artifacts / f"codexify-{TAG}-{platform}.tar.gz"
            payload = self.root / f"finalize-{platform}"
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

    def services(self, x64="Accepted", arm64="Accepted", published_tags=()):
        return FakeFinalizeServices(
            self.release,
            self.asset_bytes,
            {
                self.submission_ids["darwin-x64"]: x64,
                self.submission_ids["darwin-arm64"]: arm64,
            },
            published_tags,
        )

    def test_pending_submission_keeps_draft_without_uploading_logs(self) -> None:
        services = self.services(x64="Accepted", arm64="In Progress")
        summary = self.module.finalize_releases(self.options(), services)
        self.assertEqual(summary, {"ignored": 0, "pending": 1, "published": 0})
        names = [event[0] for event in services.events]
        self.assertNotIn("publish", names)
        self.assertNotIn("notary_log", names)
        self.assertNotIn("delete_asset", names)

    def test_invalid_submission_uploads_log_and_keeps_draft(self) -> None:
        services = self.services(x64="Accepted", arm64="Invalid")
        with self.assertRaisesRegex(RuntimeError, "Invalid"):
            self.module.finalize_releases(self.options(), services)
        names = [event[0] for event in services.events]
        self.assertIn("notary_log", names)
        self.assertIn("upload_internal", names)
        self.assertNotIn("publish", names)
        self.assertTrue(self.release["draft"])

    def test_invalid_submission_wins_over_another_submission_still_processing(self) -> None:
        services = self.services(x64="Invalid", arm64="In Progress")
        with self.assertRaisesRegex(RuntimeError, "Invalid"):
            self.module.finalize_releases(self.options(), services)
        names = [event[0] for event in services.events]
        self.assertIn("notary_log", names)
        self.assertIn("upload_internal", names)
        self.assertNotIn("publish", names)

    def test_accepted_submissions_verify_every_asset_and_publish(self) -> None:
        services = self.services()
        summary = self.module.finalize_releases(self.options(), services)
        self.assertEqual(summary, {"ignored": 0, "pending": 0, "published": 1})
        events = services.events
        self.assertEqual([event[0] for event in events].count("verify_signature"), 2)
        self.assertEqual([event[0] for event in events].count("publish"), 1)
        publish = next(event for event in events if event[0] == "publish")
        self.assertEqual(publish[3], True)
        deleted_ids = {event[2] for event in events if event[0] == "delete_asset"}
        manifest_id = next(
            asset["id"] for asset in self.release["assets"] if asset["name"] == "codexify-notarization.json"
        ) if self.release["draft"] else 106
        self.assertIn(manifest_id, deleted_ids)

    def test_changed_asset_prevents_publication(self) -> None:
        name = f"codexify-{TAG}-darwin-arm64.tar.gz"
        self.asset_bytes[name] += b"tampered"
        services = self.services()
        with self.assertRaisesRegex(RuntimeError, "changed"):
            self.module.finalize_releases(self.options(), services)
        self.assertNotIn("publish", [event[0] for event in services.events])

    def test_oversized_manifest_metadata_is_rejected_before_download(self) -> None:
        manifest_asset = next(
            asset for asset in self.release["assets"] if asset["name"] == self.module.MANIFEST_ASSET
        )
        manifest_asset["size"] = self.module.MAX_MANIFEST_BYTES + 1
        services = self.services()
        with self.assertRaisesRegex(RuntimeError, "manifest exceeds"):
            self.module.finalize_releases(self.options(), services)
        self.assertNotIn("download_asset", [event[0] for event in services.events])

    def test_public_asset_metadata_must_match_manifest_before_download(self) -> None:
        name = f"codexify-{TAG}-darwin-arm64.tar.gz"
        release_asset = next(asset for asset in self.release["assets"] if asset["name"] == name)
        release_asset["size"] += 1
        services = self.services()
        with self.assertRaisesRegex(RuntimeError, "metadata size"):
            self.module.finalize_releases(self.options(), services)
        downloaded_names = [event[2] for event in services.events if event[0] == "download_asset"]
        self.assertNotIn(name, downloaded_names)

    def test_manifest_cannot_choose_a_different_signing_identity(self) -> None:
        self.manifest["identifier"] = "dev.other"
        self.asset_bytes[self.module.MANIFEST_ASSET] = (
            json.dumps(self.manifest, indent=2, sort_keys=True) + "\n"
        ).encode()
        services = self.services()
        with self.assertRaisesRegex(RuntimeError, "identifier"):
            self.module.finalize_releases(self.options(), services)
        self.assertNotIn("notary_status", [event[0] for event in services.events])

    def test_newer_published_version_keeps_older_release_from_becoming_latest(self) -> None:
        services = self.services(published_tags=["v9.9.0"])
        self.module.finalize_releases(self.options(), services)
        publish = next(event for event in services.events if event[0] == "publish")
        self.assertEqual(publish[3], False)

    def test_missing_manifest_is_an_idempotent_noop(self) -> None:
        self.release["assets"] = [
            asset for asset in self.release["assets"] if asset["name"] != "codexify-notarization.json"
        ]
        services = self.services()
        summary = self.module.finalize_releases(self.options(), services)
        self.assertEqual(summary, {"ignored": 1, "pending": 0, "published": 0})
        self.assertNotIn("notary_status", [event[0] for event in services.events])

    def test_duplicate_callback_after_publication_is_a_noop(self) -> None:
        services = self.services()
        self.module.finalize_releases(self.options(), services)
        before = len([event for event in services.events if event[0] == "publish"])
        summary = self.module.finalize_releases(self.options(), services)
        after = len([event for event in services.events if event[0] == "publish"])
        self.assertEqual(before, after)
        self.assertEqual(summary["published"], 0)


class FinalizerWorkflowTests(unittest.TestCase):
    def test_finalizer_is_dispatch_driven_and_runs_on_macos(self) -> None:
        path = ROOT / ".github" / "workflows" / "finalize-release.yml"
        self.assertTrue(path.exists(), "finalizer workflow must exist")
        workflow = path.read_text()
        self.assertIn("repository_dispatch:", workflow)
        self.assertIn("apple-notarization-complete", workflow)
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("runs-on: macos-14", workflow)
        self.assertIn("python3 scripts/release_notarization.py \"${ARGS[@]}\"", workflow)
        self.assertIn("            finalize", workflow)
        self.assertNotIn("schedule:", workflow)


class ReleaseDiscoveryScriptTests(unittest.TestCase):
    def test_installers_use_latest_published_release_and_powershell_rejects_unpublished_metadata(self) -> None:
        posix = (ROOT / "install.sh").read_text()
        powershell = (ROOT / "install.ps1").read_text()
        self.assertIn("https://github.com/$REPOSITORY/releases/latest", posix)
        self.assertNotIn("releases?per_page", posix)
        self.assertNotIn("/tags", posix)
        self.assertIn("https://api.github.com/repos/$Repository/releases/latest", powershell)
        self.assertIn("$Release.draft -or $Release.prerelease", powershell)


if __name__ == "__main__":
    unittest.main()
