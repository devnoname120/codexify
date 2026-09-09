# macOS signing and asynchronous notarization design

## Goal

Give every public macOS Codexify build one stable Developer ID identity so macOS can recognize updates as the same executable, while withholding every release from users until both macOS architectures have been notarized successfully. Apple processing must not hold a GitHub Actions runner open.

## Identity and signing

The two macOS release binaries remain separate Intel and Apple-silicon artifacts. Each final Mach-O is signed after compilation and before archive creation with:

- a Developer ID Application certificate from team `4HN6WUZ995`;
- identifier `dev.codexify`;
- hardened runtime;
- an Apple secure timestamp;
- no provisioning profile and no additional entitlements unless a later feature requires them.

The release job verifies the signature, identifier, Team ID, and designated requirement before packaging. The same signed bytes are placed in the public tarball and in the temporary ZIP submitted to Apple. Checksums are generated only after signing.

The first Developer ID-signed update may require one final macOS privacy reapproval because it replaces the existing ad-hoc identity. Later releases preserve the same identity inputs.

## Release lifecycle

A tag-triggered release remains fail-closed but becomes asynchronous:

1. Existing checks and all platform builds run normally.
2. Each macOS matrix job imports the encrypted certificate into a temporary keychain, signs its binary, verifies the identity, and packages the signed binary.
3. A macOS staging job downloads all five platform archives, creates `checksums.txt`, and creates or refreshes a draft GitHub release for the tag.
4. The staging job uploads every final public asset to the draft.
5. It extracts each signed macOS binary, creates a temporary notarization ZIP, and invokes `xcrun notarytool submit` with `--no-wait` and the configured webhook URL.
6. It uploads a private-to-draft manifest named `codexify-notarization.json` containing the release ID, tag, commit, public-asset hashes, exact signed-binary hashes, signing identity, and both Apple submission IDs.
7. It sends one immediate `repository_dispatch` event, then exits. No GitHub runner remains allocated while Apple processes the submissions.

Draft releases are the durable staging area. A rerun may replace assets and the manifest only while the release is still a draft. A published release is immutable to this workflow.

## Cloudflare wake-up relay

Apple calls a Cloudflare Worker URL after a submission changes state. The Worker is intentionally not an authority. It:

- accepts only `POST` requests whose path contains a generated secret;
- bounds and discards the request body;
- calls GitHub's `repository_dispatch` endpoint with event type `apple-notarization-complete`;
- returns success only after GitHub accepts the dispatch.

The Worker holds a fine-grained GitHub token restricted to `devnoname120/codexify` with repository Contents read/write permission. It never receives the Developer ID private key or App Store Connect API key.

The secret path limits unsolicited wake-ups, but a forged callback cannot publish anything because the finalizer independently rechecks Apple and all draft assets.

## Finalization

A separate workflow runs on macOS for `repository_dispatch` and manual `workflow_dispatch`. It processes each Codexify draft carrying the manifest:

1. Download and strictly validate the manifest against the draft release and the commit resolved by its tag; GitHub may keep `target_commitish` as the source branch for an existing tag.
2. Query both submission IDs using `xcrun notarytool info` and the App Store Connect team API key.
3. If either status is still `In Progress`, exit successfully without changing the draft.
4. If either status is terminal but not `Accepted`, download the Apple logs, attach them to the draft for diagnosis, keep the release unpublished, and fail the workflow.
5. If both are `Accepted`, download all draft assets and verify their sizes and SHA-256 hashes against the manifest and `checksums.txt`.
6. Extract both macOS binaries and revalidate their SHA-256 hashes, Developer ID signatures, identifier, Team ID, and designated requirements.
7. Remove the internal manifest and any notarization log assets.
8. Publish that exact draft. Mark it latest only when no already-published stable release has a higher semantic version.

Duplicate callbacks and repeated manual runs are idempotent. A callback from an obsolete submission only wakes the finalizer; the current manifest remains the source of truth.

## Stable-release discovery boundary

Normal installation and self-update continue to discover versions only through GitHub's latest published stable release surfaces:

- the web `/releases/latest` redirect used by the POSIX installer;
- the REST `/releases/latest` endpoint used by PowerShell and Rust;
- the equivalent `gh api repos/.../releases/latest` fallback.

The Rust updater and PowerShell installer additionally reject metadata explicitly marked `draft` or `prerelease`. Tests lock these endpoints and checks in place. An explicit development override naming an unpublished tag is not normal discovery and remains expected to fail without authenticated draft-asset access.

## Credentials and secret handling

GitHub Actions uses these repository secrets:

- `MACOS_DEVELOPER_ID_P12_BASE64`;
- `MACOS_DEVELOPER_ID_PASSWORD`;
- `APPLE_NOTARY_KEY_P8_BASE64`;
- `APPLE_NOTARY_KEY_ID`;
- `APPLE_NOTARY_ISSUER_ID`;
- `APPLE_NOTARY_WEBHOOK_URL`.

The public team identifier is stored as repository variable `APPLE_TEAM_ID=4HN6WUZ995`.

The certificate private key and App Store Connect `.p8` key are generated or downloaded once, retained in a private local backup, and copied to GitHub only as encrypted secrets. Workflows materialize them under `$RUNNER_TEMP`, use a temporary keychain, and remove temporary files during job cleanup. Secret values never enter logs, release assets, caches, or repository files.

## Failure and recovery

- Missing credentials, signing mismatches, submission failures, malformed manifests, changed assets, invalid checksums, or rejected notarizations leave the release as a draft.
- A failed staging rerun may clobber only assets on that same draft; it cannot edit a published release.
- A finalizer invocation that sees no matching draft is a successful no-op, which makes early or duplicate webhooks harmless.
- Manual `workflow_dispatch` is the fallback when a webhook is lost or the Worker is unavailable.
- Existing published releases and `/releases/latest` remain unchanged until final publication succeeds.

## Testing

Automated coverage includes:

- manifest construction and strict validation;
- release-state decisions for pending, rejected, accepted, duplicate, and out-of-order releases;
- command construction for signing, asynchronous submission, draft creation, dispatch, and publication;
- fake `gh`, `codesign`, `ditto`, and `xcrun` integration tests that exercise staging and finalization without Apple or GitHub side effects;
- Worker tests for method/path validation, body limits, GitHub dispatch payloads, and upstream failures;
- installer/updater tests proving draft and prerelease metadata are rejected and production discovery remains on `/releases/latest`;
- workflow syntax, Rust tests, script tests, and the existing full CI suite.

## Scope boundaries

This change does not migrate Codexify into an app bundle or `.pkg`, staple tickets to a raw executable, register recipient Mac hardware, alter runtime entitlements, or change the public archive names. It also does not make Cloudflare responsible for notarization state or release publication.
