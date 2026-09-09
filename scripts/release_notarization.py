#!/usr/bin/env python3

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tarfile
import tempfile
import uuid
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Protocol, Sequence
from urllib.parse import quote


MANIFEST_ASSET = "codexify-notarization.json"
NOTARIZATION_LOG_PREFIX = "codexify-notarization-log-"
MAC_PLATFORMS = ("darwin-x64", "darwin-arm64")
ALL_PLATFORMS = (
    "linux-x64",
    "linux-arm64",
    "darwin-x64",
    "darwin-arm64",
    "windows-x64",
)
TAG_PATTERN = re.compile(r"^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
COMMIT_PATTERN = re.compile(r"^[0-9a-f]{40}$")
KEY_ID_PATTERN = re.compile(r"^[A-Za-z0-9]{10,}$")
TEAM_ID_PATTERN = re.compile(r"^[A-Z0-9]{10}$")
IDENTIFIER_PATTERN = re.compile(r"^[A-Za-z0-9.-]+$")
MAX_BINARY_BYTES = 256 * 1024 * 1024
MAX_MANIFEST_BYTES = 1024 * 1024
MAX_NOTARIZATION_LOG_BYTES = 4 * 1024 * 1024


class ReleaseError(RuntimeError):
    pass


@dataclass(frozen=True)
class NotaryCredentials:
    key_path: Path
    key_id: str
    issuer: str


@dataclass(frozen=True)
class StageOptions:
    repo: str
    tag: str
    commit: str
    artifacts_dir: Path
    credentials: NotaryCredentials
    webhook_url: str
    team_id: str
    identifier: str


@dataclass(frozen=True)
class FinalizeOptions:
    repo: str
    credentials: NotaryCredentials
    team_id: str
    identifier: str
    requested_tag: str | None = None


class StageServices(Protocol):
    def resolve_tag_commit(self, repo: str, tag: str) -> str: ...

    def find_release(self, repo: str, tag: str) -> dict[str, Any] | None: ...

    def create_draft(self, repo: str, tag: str, commit: str) -> dict[str, Any]: ...

    def delete_asset(self, repo: str, asset_id: int) -> None: ...

    def upload_public_assets(self, repo: str, tag: str, paths: Sequence[Path]) -> None: ...

    def verify_signed_binary(self, binary: Path, team_id: str, identifier: str) -> None: ...

    def create_notary_zip(self, binary: Path, destination: Path) -> None: ...

    def submit_notarization(
        self,
        archive: Path,
        credentials: NotaryCredentials,
        webhook_url: str,
    ) -> str: ...

    def upload_internal_asset(self, repo: str, tag: str, path: Path) -> None: ...

    def dispatch_finalizer(self, repo: str, tag: str) -> None: ...


class FinalizeServices(Protocol):
    def list_releases(self, repo: str) -> list[dict[str, Any]]: ...

    def download_asset(self, repo: str, asset: dict[str, Any], destination: Path) -> None: ...

    def resolve_tag_commit(self, repo: str, tag: str) -> str: ...

    def notarization_status(
        self,
        submission_id: str,
        credentials: NotaryCredentials,
    ) -> str: ...

    def download_notarization_log(
        self,
        submission_id: str,
        credentials: NotaryCredentials,
        destination: Path,
    ) -> None: ...

    def upload_internal_asset(self, repo: str, tag: str, path: Path) -> None: ...

    def verify_signed_binary(self, binary: Path, team_id: str, identifier: str) -> None: ...

    def delete_asset(self, repo: str, asset_id: int) -> None: ...

    def publish_release(self, repo: str, release_id: int, make_latest: bool) -> None: ...


def expected_archive_names(tag: str) -> tuple[str, ...]:
    validate_tag(tag)
    names = []
    for platform in ALL_PLATFORMS:
        suffix = ".zip" if platform == "windows-x64" else ".tar.gz"
        names.append(f"codexify-{tag}-{platform}{suffix}")
    return tuple(names)


def expected_public_asset_names(tag: str) -> tuple[str, ...]:
    return (*expected_archive_names(tag), "checksums.txt")


def notary_submit_command(
    archive: Path,
    credentials: NotaryCredentials,
    webhook_url: str,
) -> list[str]:
    return [
        "xcrun",
        "notarytool",
        "submit",
        str(archive),
        "--key",
        str(credentials.key_path),
        "--key-id",
        credentials.key_id,
        "--issuer",
        credentials.issuer,
        "--webhook",
        webhook_url,
        "--no-wait",
        "--output-format",
        "json",
    ]


def validate_tag(tag: str) -> None:
    if not TAG_PATTERN.fullmatch(tag):
        raise ReleaseError(f"release tag must be stable semantic versioning in vX.Y.Z form: {tag!r}")


def validate_stage_options(options: StageOptions) -> None:
    validate_tag(options.tag)
    if not COMMIT_PATTERN.fullmatch(options.commit):
        raise ReleaseError("release commit must be a 40-character lowercase Git object ID")
    if not re.fullmatch(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$", options.repo):
        raise ReleaseError(f"invalid GitHub repository: {options.repo!r}")
    if not options.artifacts_dir.is_dir():
        raise ReleaseError(f"artifact directory does not exist: {options.artifacts_dir}")
    if not options.credentials.key_path.is_file():
        raise ReleaseError(f"notary API key does not exist: {options.credentials.key_path}")
    if not KEY_ID_PATTERN.fullmatch(options.credentials.key_id):
        raise ReleaseError("notary API key ID is malformed")
    try:
        uuid.UUID(options.credentials.issuer)
    except ValueError as error:
        raise ReleaseError("notary API issuer is malformed") from error
    if not options.webhook_url.startswith("https://"):
        raise ReleaseError("notary webhook URL must use HTTPS")
    if not TEAM_ID_PATTERN.fullmatch(options.team_id):
        raise ReleaseError("Apple Team ID is malformed")
    if not IDENTIFIER_PATTERN.fullmatch(options.identifier):
        raise ReleaseError("code-signing identifier is malformed")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def create_checksums(artifacts_dir: Path, archive_names: Sequence[str]) -> Path:
    output = artifacts_dir / "checksums.txt"
    lines = [f"{sha256_file(artifacts_dir / name)}  {name}\n" for name in sorted(archive_names)]
    output.write_text("".join(lines), encoding="utf-8")
    return output


def collect_public_assets(options: StageOptions) -> list[Path]:
    expected_archives = expected_archive_names(options.tag)
    missing = [name for name in expected_archives if not (options.artifacts_dir / name).is_file()]
    unexpected = sorted(
        path.name
        for path in options.artifacts_dir.iterdir()
        if path.is_file()
        and path.name.startswith(f"codexify-{options.tag}-")
        and path.name not in expected_archives
    )
    if missing:
        raise ReleaseError(f"missing release assets: {', '.join(missing)}")
    if unexpected:
        raise ReleaseError(f"unexpected release assets: {', '.join(unexpected)}")
    checksums = create_checksums(options.artifacts_dir, expected_archives)
    return [options.artifacts_dir / name for name in expected_public_asset_names(options.tag)]


def extract_macos_binary(archive: Path, destination: Path) -> Path:
    with tarfile.open(archive, "r:gz") as bundle:
        members = [
            member
            for member in bundle.getmembers()
            if member.isfile() and PurePosixPath(member.name).name == "codexify"
        ]
        if len(members) != 1:
            raise ReleaseError(f"{archive.name} must contain exactly one regular codexify executable")
        source = bundle.extractfile(members[0])
        if source is None:
            raise ReleaseError(f"cannot read codexify from {archive.name}")
        payload = source.read(MAX_BINARY_BYTES + 1)
        if len(payload) > MAX_BINARY_BYTES:
            raise ReleaseError(f"codexify in {archive.name} exceeds {MAX_BINARY_BYTES} bytes")
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(payload)
    destination.chmod(0o755)
    return destination


def asset_records(paths: Iterable[Path]) -> list[dict[str, Any]]:
    records = [
        {"name": path.name, "size": path.stat().st_size, "sha256": sha256_file(path)}
        for path in paths
    ]
    return sorted(records, key=lambda record: record["name"])


def build_manifest(
    options: StageOptions,
    release_id: int,
    public_assets: Sequence[Path],
    submissions: dict[str, dict[str, str]],
) -> dict[str, Any]:
    manifest = {
        "schemaVersion": 1,
        "releaseId": release_id,
        "tag": options.tag,
        "commit": options.commit,
        "identifier": options.identifier,
        "teamId": options.team_id,
        "assets": asset_records(public_assets),
        "submissions": submissions,
    }
    validate_manifest(manifest, options.tag, release_id)
    return manifest


def validate_manifest(
    manifest: dict[str, Any],
    expected_tag: str | None = None,
    expected_release_id: int | None = None,
) -> None:
    expected_top = {
        "schemaVersion",
        "releaseId",
        "tag",
        "commit",
        "identifier",
        "teamId",
        "assets",
        "submissions",
    }
    if set(manifest) != expected_top:
        raise ReleaseError("notarization manifest has unexpected or missing fields")
    if manifest["schemaVersion"] != 1:
        raise ReleaseError("unsupported notarization manifest version")
    if not isinstance(manifest["releaseId"], int) or manifest["releaseId"] <= 0:
        raise ReleaseError("notarization manifest release ID is invalid")
    if expected_release_id is not None and manifest["releaseId"] != expected_release_id:
        raise ReleaseError("notarization manifest release ID does not match the draft")
    if not isinstance(manifest["tag"], str):
        raise ReleaseError("notarization manifest tag is invalid")
    validate_tag(manifest["tag"])
    if expected_tag is not None and manifest["tag"] != expected_tag:
        raise ReleaseError("notarization manifest tag does not match the draft")
    if not isinstance(manifest["commit"], str) or not COMMIT_PATTERN.fullmatch(manifest["commit"]):
        raise ReleaseError("notarization manifest commit is invalid")
    if not isinstance(manifest["identifier"], str) or not IDENTIFIER_PATTERN.fullmatch(
        manifest["identifier"]
    ):
        raise ReleaseError("notarization manifest identifier is invalid")
    if not isinstance(manifest["teamId"], str) or not TEAM_ID_PATTERN.fullmatch(manifest["teamId"]):
        raise ReleaseError("notarization manifest Team ID is invalid")

    assets = manifest["assets"]
    if not isinstance(assets, list):
        raise ReleaseError("notarization manifest assets must be an array")
    names: set[str] = set()
    for asset in assets:
        if not isinstance(asset, dict) or set(asset) != {"name", "size", "sha256"}:
            raise ReleaseError("notarization manifest contains an invalid asset record")
        name = asset["name"]
        if not isinstance(name, str) or Path(name).name != name or name in names:
            raise ReleaseError("notarization manifest contains an invalid or duplicate asset name")
        names.add(name)
        if not isinstance(asset["size"], int) or asset["size"] < 0:
            raise ReleaseError("notarization manifest contains an invalid asset size")
        if not isinstance(asset["sha256"], str) or not SHA256_PATTERN.fullmatch(asset["sha256"]):
            raise ReleaseError("notarization manifest contains an invalid asset hash")
    if names != set(expected_public_asset_names(manifest["tag"])):
        raise ReleaseError("notarization manifest public asset set is incomplete or unexpected")

    submissions = manifest["submissions"]
    if not isinstance(submissions, dict) or set(submissions) != set(MAC_PLATFORMS):
        raise ReleaseError("notarization manifest must contain both macOS submissions")
    for platform, submission in submissions.items():
        if not isinstance(submission, dict) or set(submission) != {
            "id",
            "archive",
            "binarySha256",
        }:
            raise ReleaseError(f"notarization manifest contains an invalid {platform} submission")
        try:
            uuid.UUID(submission["id"])
        except (ValueError, TypeError) as error:
            raise ReleaseError(f"notarization manifest contains an invalid {platform} submission ID") from error
        expected_archive = f"codexify-{manifest['tag']}-{platform}.tar.gz"
        if submission["archive"] != expected_archive:
            raise ReleaseError(f"notarization manifest contains the wrong {platform} archive")
        if not isinstance(submission["binarySha256"], str) or not SHA256_PATTERN.fullmatch(
            submission["binarySha256"]
        ):
            raise ReleaseError(f"notarization manifest contains an invalid {platform} binary hash")


def stage_release(options: StageOptions, services: StageServices) -> dict[str, Any]:
    validate_stage_options(options)
    resolved_commit = services.resolve_tag_commit(options.repo, options.tag)
    if resolved_commit != options.commit:
        raise ReleaseError(
            f"tag {options.tag} resolves to {resolved_commit}, not requested commit {options.commit}"
        )

    release = services.find_release(options.repo, options.tag)
    if release is None:
        release = services.create_draft(options.repo, options.tag, options.commit)
    elif not release.get("draft"):
        raise ReleaseError(f"release {options.tag} is already published")

    if release.get("tag_name") != options.tag:
        raise ReleaseError("draft release tag does not match the requested tag")
    release_id = release.get("id")
    if not isinstance(release_id, int) or release_id <= 0:
        raise ReleaseError("draft release has no valid database ID")

    for asset in release.get("assets", []):
        name = asset.get("name") if isinstance(asset, dict) else None
        asset_id = asset.get("id") if isinstance(asset, dict) else None
        if (
            isinstance(asset_id, int)
            and isinstance(name, str)
            and (name == MANIFEST_ASSET or name.startswith(NOTARIZATION_LOG_PREFIX))
        ):
            services.delete_asset(options.repo, asset_id)

    public_assets = collect_public_assets(options)
    services.upload_public_assets(options.repo, options.tag, public_assets)

    submissions: dict[str, dict[str, str]] = {}
    with tempfile.TemporaryDirectory(prefix="codexify-notarization-") as temp_name:
        temp = Path(temp_name)
        for platform in MAC_PLATFORMS:
            archive_name = f"codexify-{options.tag}-{platform}.tar.gz"
            archive = options.artifacts_dir / archive_name
            binary = extract_macos_binary(archive, temp / platform / "codexify")
            services.verify_signed_binary(binary, options.team_id, options.identifier)
            notary_zip = temp / f"codexify-{options.tag}-{platform}-notary.zip"
            services.create_notary_zip(binary, notary_zip)
            submission_id = services.submit_notarization(
                notary_zip,
                options.credentials,
                options.webhook_url,
            )
            try:
                uuid.UUID(submission_id)
            except ValueError as error:
                raise ReleaseError(f"notary service returned an invalid submission ID for {platform}") from error
            submissions[platform] = {
                "id": submission_id,
                "archive": archive_name,
                "binarySha256": sha256_file(binary),
            }

        manifest = build_manifest(options, release_id, public_assets, submissions)
        manifest_path = temp / MANIFEST_ASSET
        manifest_path.write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        services.upload_internal_asset(options.repo, options.tag, manifest_path)

    services.dispatch_finalizer(options.repo, options.tag)
    return manifest


def stable_version(tag: str) -> tuple[int, int, int] | None:
    if not TAG_PATTERN.fullmatch(tag):
        return None
    major, minor, patch = tag[1:].split(".")
    return int(major), int(minor), int(patch)


def release_asset_map(release: dict[str, Any]) -> dict[str, dict[str, Any]]:
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ReleaseError("draft release assets are not an array")
    result: dict[str, dict[str, Any]] = {}
    for asset in assets:
        if not isinstance(asset, dict):
            raise ReleaseError("draft release contains malformed asset metadata")
        name = asset.get("name")
        asset_id = asset.get("id")
        size = asset.get("size")
        if not isinstance(name, str) or Path(name).name != name:
            raise ReleaseError("draft release contains an invalid asset name")
        if not isinstance(asset_id, int) or asset_id <= 0:
            raise ReleaseError(f"draft release asset {name!r} has no valid ID")
        if not isinstance(size, int) or size < 0:
            raise ReleaseError(f"draft release asset {name!r} has no valid size")
        if name in result:
            raise ReleaseError(f"draft release contains duplicate asset {name!r}")
        result[name] = asset
    return result


def read_manifest(path: Path, tag: str, release_id: int) -> dict[str, Any]:
    if path.stat().st_size > MAX_MANIFEST_BYTES:
        raise ReleaseError("notarization manifest exceeds its size limit")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ReleaseError("notarization manifest is not valid UTF-8 JSON") from error
    if not isinstance(value, dict):
        raise ReleaseError("notarization manifest root must be an object")
    validate_manifest(value, tag, release_id)
    return value


def parse_checksums(path: Path, expected_names: set[str]) -> dict[str, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise ReleaseError("checksums.txt is not valid UTF-8") from error
    checksums: dict[str, str] = {}
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  (\S+)", line)
        if match is None:
            raise ReleaseError("checksums.txt contains a malformed line")
        digest, name = match.groups()
        if Path(name).name != name or name in checksums:
            raise ReleaseError("checksums.txt contains an invalid or duplicate filename")
        checksums[name] = digest
    if set(checksums) != expected_names:
        raise ReleaseError("checksums.txt does not cover the exact release archive set")
    return checksums


def validate_finalize_options(options: FinalizeOptions) -> None:
    if not re.fullmatch(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$", options.repo):
        raise ReleaseError(f"invalid GitHub repository: {options.repo!r}")
    if not options.credentials.key_path.is_file():
        raise ReleaseError(f"notary API key does not exist: {options.credentials.key_path}")
    if not KEY_ID_PATTERN.fullmatch(options.credentials.key_id):
        raise ReleaseError("notary API key ID is malformed")
    try:
        uuid.UUID(options.credentials.issuer)
    except ValueError as error:
        raise ReleaseError("notary API issuer is malformed") from error
    if options.requested_tag is not None:
        validate_tag(options.requested_tag)
    if not TEAM_ID_PATTERN.fullmatch(options.team_id):
        raise ReleaseError("Apple Team ID is malformed")
    if not IDENTIFIER_PATTERN.fullmatch(options.identifier):
        raise ReleaseError("code-signing identifier is malformed")


def internal_asset_name(name: str) -> bool:
    return name == MANIFEST_ASSET or name.startswith(NOTARIZATION_LOG_PREFIX)


def verify_downloaded_release(
    options: FinalizeOptions,
    services: FinalizeServices,
    manifest: dict[str, Any],
    asset_map: dict[str, dict[str, Any]],
    temp: Path,
) -> None:
    expected_names = set(expected_public_asset_names(manifest["tag"]))
    actual_public = {name for name in asset_map if not internal_asset_name(name)}
    if actual_public != expected_names:
        raise ReleaseError("draft release public asset set is incomplete or unexpected")

    manifest_records = {record["name"]: record for record in manifest["assets"]}
    downloaded: dict[str, Path] = {}
    for name in sorted(expected_names):
        record = manifest_records[name]
        if asset_map[name]["size"] != record["size"]:
            raise ReleaseError(f"release asset metadata size changed after notarization staging: {name}")
        destination = temp / "assets" / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        services.download_asset(options.repo, asset_map[name], destination)
        if destination.stat().st_size != record["size"]:
            raise ReleaseError(f"release asset size changed after notarization staging: {name}")
        if sha256_file(destination) != record["sha256"]:
            raise ReleaseError(f"release asset hash changed after notarization staging: {name}")
        downloaded[name] = destination

    archive_names = set(expected_archive_names(manifest["tag"]))
    checksums = parse_checksums(downloaded["checksums.txt"], archive_names)
    for name in archive_names:
        if checksums[name] != manifest_records[name]["sha256"]:
            raise ReleaseError(f"checksums.txt disagrees with the notarization manifest for {name}")

    for platform in MAC_PLATFORMS:
        archive_name = f"codexify-{manifest['tag']}-{platform}.tar.gz"
        binary = extract_macos_binary(
            downloaded[archive_name],
            temp / "verified" / platform / "codexify",
        )
        if sha256_file(binary) != manifest["submissions"][platform]["binarySha256"]:
            raise ReleaseError(f"signed binary hash changed for {platform}")
        services.verify_signed_binary(binary, manifest["teamId"], manifest["identifier"])


def finalize_one_release(
    options: FinalizeOptions,
    services: FinalizeServices,
    release: dict[str, Any],
    published_versions: list[tuple[int, int, int]],
) -> str:
    tag = release.get("tag_name")
    release_id = release.get("id")
    if not isinstance(tag, str) or stable_version(tag) is None:
        raise ReleaseError("draft release tag is not a stable semantic version")
    if not isinstance(release_id, int) or release_id <= 0:
        raise ReleaseError(f"draft release {tag} has no valid database ID")
    if release.get("prerelease"):
        raise ReleaseError(f"draft release {tag} must not be a prerelease")

    asset_map = release_asset_map(release)
    manifest_asset = asset_map.get(MANIFEST_ASSET)
    if manifest_asset is None:
        return "ignored"
    if manifest_asset["size"] > MAX_MANIFEST_BYTES:
        raise ReleaseError("notarization manifest exceeds its size limit")

    with tempfile.TemporaryDirectory(prefix="codexify-finalize-") as temp_name:
        temp = Path(temp_name)
        manifest_path = temp / MANIFEST_ASSET
        services.download_asset(options.repo, manifest_asset, manifest_path)
        manifest = read_manifest(manifest_path, tag, release_id)

        if manifest["teamId"] != options.team_id:
            raise ReleaseError(f"notarization manifest Team ID does not equal {options.team_id}")
        if manifest["identifier"] != options.identifier:
            raise ReleaseError(
                f"notarization manifest identifier does not equal {options.identifier}"
            )

        resolved_commit = services.resolve_tag_commit(options.repo, tag)
        if resolved_commit != manifest["commit"]:
            raise ReleaseError(f"tag {tag} no longer resolves to the notarized commit")

        statuses = {
            platform: services.notarization_status(
                manifest["submissions"][platform]["id"],
                options.credentials,
            )
            for platform in MAC_PLATFORMS
        }
        rejected = {platform: status for platform, status in statuses.items() if status != "Accepted"}
        rejected = {
            platform: status
            for platform, status in rejected.items()
            if status != "In Progress"
        }
        if rejected:
            for platform in sorted(rejected):
                log_path = temp / f"{NOTARIZATION_LOG_PREFIX}{platform}.json"
                services.download_notarization_log(
                    manifest["submissions"][platform]["id"],
                    options.credentials,
                    log_path,
                )
                if not log_path.is_file() or log_path.stat().st_size > MAX_NOTARIZATION_LOG_BYTES:
                    raise ReleaseError(f"notarization log for {platform} is missing or oversized")
                services.upload_internal_asset(options.repo, tag, log_path)
            detail = ", ".join(f"{platform}={status}" for platform, status in sorted(rejected.items()))
            raise ReleaseError(f"notarization rejected for {tag}: {detail}")
        if any(status == "In Progress" for status in statuses.values()):
            return "pending"

        verify_downloaded_release(options, services, manifest, asset_map, temp)
        version = stable_version(tag)
        assert version is not None
        make_latest = not published_versions or version > max(published_versions)

        logs = [asset for name, asset in asset_map.items() if name.startswith(NOTARIZATION_LOG_PREFIX)]
        for asset in logs:
            services.delete_asset(options.repo, asset["id"])
        services.delete_asset(options.repo, manifest_asset["id"])
        try:
            services.publish_release(options.repo, release_id, make_latest)
        except Exception:
            services.upload_internal_asset(options.repo, tag, manifest_path)
            raise
        published_versions.append(version)
        return "published"


def finalize_releases(
    options: FinalizeOptions,
    services: FinalizeServices,
) -> dict[str, int]:
    validate_finalize_options(options)
    releases = services.list_releases(options.repo)
    if not isinstance(releases, list):
        raise ReleaseError("GitHub releases response is not an array")

    published_versions = [
        version
        for release in releases
        if isinstance(release, dict)
        and not release.get("draft")
        and not release.get("prerelease")
        and isinstance(release.get("tag_name"), str)
        if (version := stable_version(release["tag_name"])) is not None
    ]
    drafts = [
        release
        for release in releases
        if isinstance(release, dict)
        and release.get("draft") is True
        and (
            options.requested_tag is None
            or release.get("tag_name") == options.requested_tag
        )
    ]
    drafts.sort(
        key=lambda release: stable_version(release.get("tag_name", "")) or (-1, -1, -1),
        reverse=True,
    )

    summary = {"ignored": 0, "pending": 0, "published": 0}
    errors: list[str] = []
    for release in drafts:
        try:
            outcome = finalize_one_release(options, services, release, published_versions)
            summary[outcome] += 1
        except ReleaseError as error:
            errors.append(str(error))
    if errors:
        raise ReleaseError("; ".join(errors))
    return summary


class CliServices:
    def _run(
        self,
        args: Sequence[str],
        action: str,
        *,
        input_data: bytes | None = None,
        redactions: Sequence[str] = (),
    ) -> subprocess.CompletedProcess[bytes]:
        result = subprocess.run(
            list(args),
            input=input_data,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode == 0:
            return result
        detail = (result.stderr or result.stdout)[-4096:].decode("utf-8", "replace").strip()
        for value in redactions:
            if value:
                detail = detail.replace(value, "<redacted>")
        raise ReleaseError(f"{action} failed with exit code {result.returncode}: {detail}")

    def _run_to_file(
        self,
        args: Sequence[str],
        action: str,
        destination: Path,
    ) -> None:
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("wb") as output:
            result = subprocess.run(
                list(args),
                stdout=output,
                stderr=subprocess.PIPE,
                check=False,
            )
        if result.returncode == 0:
            return
        destination.unlink(missing_ok=True)
        detail = result.stderr[-4096:].decode("utf-8", "replace").strip()
        raise ReleaseError(f"{action} failed with exit code {result.returncode}: {detail}")

    def _json(
        self,
        args: Sequence[str],
        action: str,
        *,
        input_data: bytes | None = None,
        redactions: Sequence[str] = (),
    ) -> Any:
        result = self._run(args, action, input_data=input_data, redactions=redactions)
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise ReleaseError(f"{action} returned malformed JSON") from error

    def _all_releases(self, repo: str) -> list[dict[str, Any]]:
        pages = self._json(
            [
                "gh",
                "api",
                "--paginate",
                "--slurp",
                f"repos/{repo}/releases?per_page=100",
            ],
            "list GitHub releases",
        )
        if not isinstance(pages, list):
            raise ReleaseError("GitHub releases response is not an array")
        releases: list[dict[str, Any]] = []
        for page in pages:
            if not isinstance(page, list):
                raise ReleaseError("GitHub releases page is not an array")
            releases.extend(item for item in page if isinstance(item, dict))
        return releases

    def list_releases(self, repo: str) -> list[dict[str, Any]]:
        return self._all_releases(repo)

    def resolve_tag_commit(self, repo: str, tag: str) -> str:
        result = self._run(
            ["gh", "api", f"repos/{repo}/commits/{quote(tag, safe='')}", "--jq", ".sha"],
            "resolve release tag",
        )
        return result.stdout.decode("utf-8", "replace").strip()

    def find_release(self, repo: str, tag: str) -> dict[str, Any] | None:
        matches = [release for release in self._all_releases(repo) if release.get("tag_name") == tag]
        if len(matches) > 1:
            raise ReleaseError(f"GitHub returned multiple releases for tag {tag}")
        return matches[0] if matches else None

    def download_asset(self, repo: str, asset: dict[str, Any], destination: Path) -> None:
        asset_id = asset.get("id")
        name = asset.get("name")
        if not isinstance(asset_id, int) or asset_id <= 0 or not isinstance(name, str):
            raise ReleaseError("cannot download malformed release asset metadata")
        self._run_to_file(
            [
                "gh",
                "api",
                "-H",
                "Accept: application/octet-stream",
                f"repos/{repo}/releases/assets/{asset_id}",
            ],
            f"download draft release asset {name}",
            destination,
        )

    def create_draft(self, repo: str, tag: str, commit: str) -> dict[str, Any]:
        self._run(
            [
                "gh",
                "release",
                "create",
                tag,
                "--repo",
                repo,
                "--draft",
                "--generate-notes",
                "--verify-tag",
                "--target",
                commit,
                "--latest=false",
            ],
            "create draft GitHub release",
        )
        release = self.find_release(repo, tag)
        if release is None:
            raise ReleaseError("created draft release could not be read back")
        return release

    def delete_asset(self, repo: str, asset_id: int) -> None:
        self._run(
            [
                "gh",
                "api",
                "--method",
                "DELETE",
                f"repos/{repo}/releases/assets/{asset_id}",
                "--silent",
            ],
            "delete internal draft asset",
        )

    def upload_public_assets(self, repo: str, tag: str, paths: Sequence[Path]) -> None:
        self._run(
            [
                "gh",
                "release",
                "upload",
                tag,
                *(str(path) for path in paths),
                "--repo",
                repo,
                "--clobber",
            ],
            "upload draft release assets",
        )

    def verify_signed_binary(self, binary: Path, team_id: str, identifier: str) -> None:
        self._run(
            ["codesign", "--verify", "--strict", "--verbose=2", str(binary)],
            "verify macOS code signature",
        )
        details = self._run(
            ["codesign", "--display", "--verbose=4", str(binary)],
            "inspect macOS code signature",
        )
        detail_text = (details.stdout + details.stderr).decode("utf-8", "replace")
        if f"Identifier={identifier}" not in detail_text:
            raise ReleaseError(f"signed binary identifier is not {identifier}")
        if f"TeamIdentifier={team_id}" not in detail_text:
            raise ReleaseError(f"signed binary Team ID is not {team_id}")
        requirement = self._run(
            ["codesign", "--display", "--requirements", "-", str(binary)],
            "inspect macOS designated requirement",
        )
        requirement_text = (requirement.stdout + requirement.stderr).decode("utf-8", "replace")
        if f'identifier "{identifier}"' not in requirement_text:
            raise ReleaseError("signed binary designated requirement has the wrong identifier")
        if "anchor apple generic" not in requirement_text or team_id not in requirement_text:
            raise ReleaseError("signed binary designated requirement has the wrong signing authority")

    def create_notary_zip(self, binary: Path, destination: Path) -> None:
        self._run(
            ["ditto", "-c", "-k", "--keepParent", str(binary), str(destination)],
            "create notarization ZIP",
        )
        if not destination.is_file() or destination.stat().st_size == 0:
            raise ReleaseError("ditto did not create the notarization ZIP")

    def submit_notarization(
        self,
        archive: Path,
        credentials: NotaryCredentials,
        webhook_url: str,
    ) -> str:
        response = self._json(
            notary_submit_command(archive, credentials, webhook_url),
            "submit macOS notarization",
            redactions=[webhook_url],
        )
        if not isinstance(response, dict) or not isinstance(response.get("id"), str):
            raise ReleaseError("notary submission response does not contain an ID")
        return response["id"]

    def notarization_status(
        self,
        submission_id: str,
        credentials: NotaryCredentials,
    ) -> str:
        response = self._json(
            [
                "xcrun",
                "notarytool",
                "info",
                submission_id,
                "--key",
                str(credentials.key_path),
                "--key-id",
                credentials.key_id,
                "--issuer",
                credentials.issuer,
                "--output-format",
                "json",
            ],
            "query macOS notarization",
        )
        if not isinstance(response, dict) or not isinstance(response.get("status"), str):
            raise ReleaseError("notary status response does not contain a status")
        return response["status"]

    def download_notarization_log(
        self,
        submission_id: str,
        credentials: NotaryCredentials,
        destination: Path,
    ) -> None:
        self._run(
            [
                "xcrun",
                "notarytool",
                "log",
                submission_id,
                str(destination),
                "--key",
                str(credentials.key_path),
                "--key-id",
                credentials.key_id,
                "--issuer",
                credentials.issuer,
            ],
            "download macOS notarization log",
        )

    def upload_internal_asset(self, repo: str, tag: str, path: Path) -> None:
        self._run(
            [
                "gh",
                "release",
                "upload",
                tag,
                str(path),
                "--repo",
                repo,
                "--clobber",
            ],
            "upload notarization manifest",
        )

    def dispatch_finalizer(self, repo: str, tag: str) -> None:
        payload = json.dumps(
            {
                "event_type": "apple-notarization-complete",
                "client_payload": {"tag": tag, "source": "release-staging"},
            }
        ).encode("utf-8")
        self._run(
            [
                "gh",
                "api",
                "--method",
                "POST",
                f"repos/{repo}/dispatches",
                "--input",
                "-",
                "--silent",
            ],
            "dispatch notarization finalizer",
            input_data=payload,
        )

    def publish_release(self, repo: str, release_id: int, make_latest: bool) -> None:
        payload = json.dumps(
            {
                "draft": False,
                "prerelease": False,
                "make_latest": "true" if make_latest else "false",
            }
        ).encode("utf-8")
        self._run(
            [
                "gh",
                "api",
                "--method",
                "PATCH",
                f"repos/{repo}/releases/{release_id}",
                "--input",
                "-",
                "--silent",
            ],
            "publish notarized GitHub release",
            input_data=payload,
        )


def stage_parser(subparsers: argparse._SubParsersAction[argparse.ArgumentParser]) -> None:
    parser = subparsers.add_parser("stage", help="stage a draft release and submit macOS assets")
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--artifacts-dir", type=Path, required=True)
    parser.add_argument("--notary-key", type=Path, required=True)
    parser.add_argument("--notary-key-id", required=True)
    parser.add_argument("--notary-issuer", required=True)
    parser.add_argument("--webhook-url", required=True)
    parser.add_argument("--team-id", required=True)
    parser.add_argument("--identifier", required=True)


def finalize_parser(subparsers: argparse._SubParsersAction[argparse.ArgumentParser]) -> None:
    parser = subparsers.add_parser("finalize", help="publish accepted notarized draft releases")
    parser.add_argument("--repo", required=True)
    parser.add_argument("--notary-key", type=Path, required=True)
    parser.add_argument("--notary-key-id", required=True)
    parser.add_argument("--notary-issuer", required=True)
    parser.add_argument("--tag")
    parser.add_argument("--team-id", required=True)
    parser.add_argument("--identifier", required=True)


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Codexify release notarization orchestration")
    subparsers = parser.add_subparsers(dest="command", required=True)
    stage_parser(subparsers)
    finalize_parser(subparsers)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        if args.command == "stage":
            options = StageOptions(
                repo=args.repo,
                tag=args.tag,
                commit=args.commit,
                artifacts_dir=args.artifacts_dir,
                credentials=NotaryCredentials(
                    key_path=args.notary_key,
                    key_id=args.notary_key_id,
                    issuer=args.notary_issuer,
                ),
                webhook_url=args.webhook_url,
                team_id=args.team_id,
                identifier=args.identifier,
            )
            manifest = stage_release(options, CliServices())
            print(
                f"Staged draft {manifest['tag']} with two asynchronous notarization submissions"
            )
            return 0
        if args.command == "finalize":
            summary = finalize_releases(
                FinalizeOptions(
                    repo=args.repo,
                    credentials=NotaryCredentials(
                        key_path=args.notary_key,
                        key_id=args.notary_key_id,
                        issuer=args.notary_issuer,
                    ),
                    requested_tag=args.tag or None,
                    team_id=args.team_id,
                    identifier=args.identifier,
                ),
                CliServices(),
            )
            print(
                "Notarization finalizer: "
                f"{summary['published']} published, "
                f"{summary['pending']} pending, "
                f"{summary['ignored']} ignored"
            )
            return 0
        raise ReleaseError(f"unsupported command: {args.command}")
    except ReleaseError as error:
        print(f"release notarization error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
