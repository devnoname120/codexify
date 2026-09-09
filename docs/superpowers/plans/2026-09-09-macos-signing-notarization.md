# macOS Signing and Asynchronous Notarization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sign every macOS release as `dev.codexify`, stage all assets in a draft GitHub release, and publish only after two asynchronous Apple notarizations are independently verified.

**Architecture:** The tag workflow signs both macOS binaries and creates a draft release on macOS, then submits two notarization ZIPs with `--no-wait`. A Cloudflare Worker translates Apple's webhook into `repository_dispatch`; a short macOS finalizer validates Apple status, signatures, hashes, and draft state before publication. Stable installers and updaters remain pinned to GitHub's latest published non-prerelease release.

**Tech Stack:** GitHub Actions, macOS `codesign`/`security`/`notarytool`/`ditto`, Python 3 standard library, GitHub CLI, Cloudflare Workers modules, Node.js test runner, Rust/serde.

---

## File map

- Create `scripts/sign-macos-release.sh`: sign and verify one final Mach-O.
- Create `scripts/release_notarization.py`: draft staging, asynchronous submission, manifest validation, Apple status checks, and publication.
- Create `scripts/test_release_notarization.py`: unit and fake-command integration tests for release orchestration.
- Modify `.github/workflows/release.yml`: import certificate, sign macOS builds, stage a draft, submit without waiting.
- Create `.github/workflows/finalize-release.yml`: short macOS dispatch/manual finalizer.
- Create `ops/cloudflare-notary-webhook/worker.mjs`: bounded webhook-to-`repository_dispatch` relay.
- Create `ops/cloudflare-notary-webhook/wrangler.toml.example`: deployable Worker configuration without secrets.
- Create `ops/cloudflare-notary-webhook/README.md`: exact Cloudflare setup and secret values.
- Create `scripts/test-cloudflare-notary-webhook.mjs`: Worker request and dispatch tests.
- Modify `src/self_update.rs`: reject draft/prerelease metadata even under a custom or future metadata source.
- Modify `install.ps1`: reject draft/prerelease API responses.
- Modify `scripts/test-posix-installer.sh` and `scripts/test-windows-installer.ps1`: lock discovery to `/releases/latest` and test rejection.
- Modify `.github/workflows/ci.yml`: run orchestration and Worker tests.
- Modify `README.md`, `docs/REFERENCE.md`, `docs/ARCHITECTURE.md`, and `CHANGELOG.md`: document signing identity and asynchronous release lifecycle.

## Task 1: Signing contract

**Files:**
- Create: `scripts/sign-macos-release.sh`
- Test: `scripts/test_release_notarization.py`

- [ ] **Step 1: Add failing tests for the signing command contract**

Create tests that run the script with a fake `codesign` in `PATH` and assert the invocation contains the exact production invariants:

```python
self.assertIn("--identifier dev.codexify", calls)
self.assertIn("--options runtime", calls)
self.assertIn("--timestamp", calls)
self.assertIn("--sign Developer ID Application: Example", calls)
```

The fake verification output must expose `Identifier=dev.codexify`, `TeamIdentifier=4HN6WUZ995`, and a designated requirement containing `identifier "dev.codexify"` and `anchor apple generic`. Add negative fixtures for the wrong identifier and Team ID.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
python3 -m unittest scripts.test_release_notarization.SigningScriptTests -v
```

Expected: failure because `scripts/sign-macos-release.sh` does not exist.

- [ ] **Step 3: Implement the signing script**

The script interface is:

```text
scripts/sign-macos-release.sh <binary> <identity> <team-id> <identifier>
```

It must invoke:

```bash
codesign --force --sign "$identity" --identifier "$identifier" --options runtime --timestamp "$binary"
codesign --verify --strict --verbose=2 "$binary"
```

It must inspect `codesign --display --verbose=4` and `codesign --display --requirements -` output and fail unless the identifier, Team ID, identifier requirement, and Apple generic anchor match the supplied values.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run the same unittest command. Expected: all signing tests pass.

- [ ] **Step 5: Commit**

```bash
git add scripts/sign-macos-release.sh scripts/test_release_notarization.py
git commit -m "build: add stable macOS signing contract"
```

## Task 2: Draft staging and asynchronous submission

**Files:**
- Create: `scripts/release_notarization.py`
- Modify: `scripts/test_release_notarization.py`
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add failing manifest and staging tests**

Tests must require exactly these public assets for tag `v9.8.7`:

```python
{
    "codexify-v9.8.7-linux-x64.tar.gz",
    "codexify-v9.8.7-linux-arm64.tar.gz",
    "codexify-v9.8.7-darwin-x64.tar.gz",
    "codexify-v9.8.7-darwin-arm64.tar.gz",
    "codexify-v9.8.7-windows-x64.zip",
    "checksums.txt",
}
```

Add a fake-command integration test that verifies staging:

- creates or reuses only a draft for the exact tag;
- removes an old manifest before clobbering assets;
- uploads all six public assets;
- extracts and verifies both signed binaries;
- invokes `xcrun notarytool submit` twice with `--no-wait`, `--webhook`, JSON output, key path, key ID, and issuer;
- uploads `codexify-notarization.json` only after both IDs are available;
- sends `repository_dispatch` event `apple-notarization-complete` after the manifest upload;
- fails if a matching release is already published.

- [ ] **Step 2: Run staging tests and verify RED**

Run:

```bash
python3 -m unittest scripts.test_release_notarization.StageTests -v
```

Expected: missing orchestration module/functions.

- [ ] **Step 3: Implement manifest and command boundaries**

Create dataclasses or validated dictionaries with this schema:

```json
{
  "schemaVersion": 1,
  "releaseId": 123,
  "tag": "v9.8.7",
  "commit": "40 lowercase hex characters",
  "identifier": "dev.codexify",
  "teamId": "4HN6WUZ995",
  "assets": [{"name": "...", "size": 123, "sha256": "64 lowercase hex"}],
  "submissions": {
    "darwin-x64": {"id": "UUID", "archive": "...", "binarySha256": "..."},
    "darwin-arm64": {"id": "UUID", "archive": "...", "binarySha256": "..."}
  }
}
```

Reject unknown top-level/submission keys, duplicate assets, malformed hashes/UUIDs, missing architectures, a tag/commit/release mismatch, or any unexpected public asset.

- [ ] **Step 4: Implement `stage`**

Expose:

```bash
python3 scripts/release_notarization.py stage \
  --repo devnoname120/codexify \
  --tag "$GITHUB_REF_NAME" \
  --commit "$GITHUB_SHA" \
  --artifacts-dir artifacts \
  --notary-key "$RUNNER_TEMP/AuthKey.p8" \
  --notary-key-id "$APPLE_NOTARY_KEY_ID" \
  --notary-issuer "$APPLE_NOTARY_ISSUER_ID" \
  --webhook-url "$APPLE_NOTARY_WEBHOOK_URL" \
  --team-id "$APPLE_TEAM_ID" \
  --identifier dev.codexify
```

Use argument arrays, never shell interpolation, for `gh`, `codesign`, `ditto`, and `xcrun`. Generate deterministic SHA-256 checksums after all archives exist. Draft creation uses `gh release create --draft --generate-notes --verify-tag --latest=false`; draft refresh deletes the old internal manifest first and uses `gh release upload --clobber`.

- [ ] **Step 5: Rework the tag workflow**

For each Darwin matrix entry:

1. Decode `MACOS_DEVELOPER_ID_P12_BASE64` under `$RUNNER_TEMP`.
2. Create and unlock a temporary keychain.
3. Import the P12 using `MACOS_DEVELOPER_ID_PASSWORD`.
4. Grant `codesign` key partition access.
5. Discover the single Developer ID Application identity.
6. Build, sign with `scripts/sign-macos-release.sh`, then package.
7. Delete the keychain and P12 in an `if: always()` cleanup step.

Replace the direct publication job with a `macos-14` staging job that downloads all build artifacts, reconstructs the `.p8` key, invokes `stage`, and cleans the key. Keep installer-site deployment as a prerequisite to staging.

- [ ] **Step 6: Run tests and workflow parsing**

Run:

```bash
python3 -m unittest scripts.test_release_notarization.StageTests -v
ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml")'
```

Expected: pass.

- [ ] **Step 7: Commit**

```bash
git add scripts/release_notarization.py scripts/test_release_notarization.py .github/workflows/release.yml
git commit -m "build: stage signed releases for asynchronous notarization"
```

## Task 3: Notarization finalizer

**Files:**
- Modify: `scripts/release_notarization.py`
- Modify: `scripts/test_release_notarization.py`
- Create: `.github/workflows/finalize-release.yml`

- [ ] **Step 1: Add failing finalization tests**

Cover these decisions:

```text
In Progress + any             -> unchanged draft, success
Accepted + In Progress        -> unchanged draft, success
Invalid + any terminal state  -> logs uploaded, unchanged draft, failure
Accepted + Accepted           -> full verification then publish
```

Add tests for duplicate callbacks, missing manifests, asset hash changes, wrong tag commit, wrong code identity, and out-of-order stable versions. A lower accepted version must publish with `make_latest=false` when a higher stable release is already published.

- [ ] **Step 2: Run finalizer tests and verify RED**

Run:

```bash
python3 -m unittest scripts.test_release_notarization.FinalizeTests -v
```

Expected: missing `finalize` behavior.

- [ ] **Step 3: Implement `finalize`**

Expose:

```bash
python3 scripts/release_notarization.py finalize \
  --repo devnoname120/codexify \
  --notary-key "$RUNNER_TEMP/AuthKey.p8" \
  --notary-key-id "$APPLE_NOTARY_KEY_ID" \
  --notary-issuer "$APPLE_NOTARY_ISSUER_ID" \
  --tag "$OPTIONAL_TAG"
```

List authenticated releases with `gh api --paginate --slurp`, select drafts containing the exact manifest asset, and validate every boundary. Query Apple with `xcrun notarytool info`. On rejection, retrieve `notarytool log` into bounded draft-only assets. On acceptance, download and verify every asset and both signatures, delete internal assets, then publish with a GitHub REST PATCH whose `make_latest` value is based on stable semantic-version ordering.

- [ ] **Step 4: Add finalizer workflow**

Use:

```yaml
on:
  repository_dispatch:
    types: [apple-notarization-complete]
  workflow_dispatch:
    inputs:
      tag:
        required: false
```

Run on `macos-14` with `contents: write`, one repository-wide non-cancelling concurrency group, a 10-minute timeout, a reconstructed notary key, and cleanup under `always()`.

- [ ] **Step 5: Run tests and workflow parsing**

Run focused unittests and parse both YAML files. Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add scripts/release_notarization.py scripts/test_release_notarization.py .github/workflows/finalize-release.yml
git commit -m "build: finalize notarized draft releases"
```

## Task 4: Stable-release discovery defenses

**Files:**
- Modify: `src/self_update.rs`
- Modify: `install.ps1`
- Modify: `scripts/test-posix-installer.sh`
- Modify: `scripts/test-windows-installer.ps1`

- [ ] **Step 1: Add failing Rust tests**

Extend mocked release metadata to include:

```json
{"tag_name":"v9.8.7","draft":true,"prerelease":false}
```

and require `latest_release` to reject both `draft=true` and `prerelease=true`. Add a source-constant test that the REST and `gh api` production paths end in `/releases/latest`.

- [ ] **Step 2: Run focused Rust tests and verify RED**

Run:

```bash
cargo test self_update::tests::latest_release_rejects_non_public_metadata -- --exact
```

Expected: draft metadata is currently accepted or fields are absent.

- [ ] **Step 3: Implement explicit Rust rejection**

Extend `LatestRelease` with defaulted `draft` and `prerelease` booleans. Reject either flag before parsing the tag. Keep existing minimal fixtures compatible through `#[serde(default)]`.

- [ ] **Step 4: Add installer tests before implementation**

The POSIX harness must assert the script contains only the web `/releases/latest` discovery route and does not call a releases list or tags endpoint. The Windows harness must feed draft and prerelease API fixtures and require installation to stop before downloading an asset.

- [ ] **Step 5: Implement PowerShell rejection**

Immediately after resolving metadata, fail when:

```powershell
if ($Release.draft -or $Release.prerelease) {
    throw 'The latest-release endpoint returned an unpublished release.'
}
```

- [ ] **Step 6: Run Rust and installer tests**

Run the focused Rust tests, POSIX integration harness, PowerShell parser, and Windows harness where available.

- [ ] **Step 7: Commit**

```bash
git add src/self_update.rs install.ps1 scripts/test-posix-installer.sh scripts/test-windows-installer.ps1
git commit -m "fix: exclude unpublished releases from updates"
```

## Task 5: Cloudflare relay

**Files:**
- Create: `ops/cloudflare-notary-webhook/worker.mjs`
- Create: `ops/cloudflare-notary-webhook/wrangler.toml.example`
- Create: `ops/cloudflare-notary-webhook/README.md`
- Create: `scripts/test-cloudflare-notary-webhook.mjs`

- [ ] **Step 1: Add failing Worker tests**

Use Node's built-in test runner and injected `fetch` to verify:

- non-POST requests return `405`;
- a wrong secret path returns `404`;
- bodies over 65,536 bytes return `413`;
- a valid callback posts exactly one GitHub request to `/repos/devnoname120/codexify/dispatches` with event type `apple-notarization-complete`;
- GitHub `204` maps to Worker `202`;
- GitHub failure maps to `502` without returning its response body.

- [ ] **Step 2: Run Worker tests and verify RED**

Run:

```bash
node --test scripts/test-cloudflare-notary-webhook.mjs
```

Expected: module missing.

- [ ] **Step 3: Implement Worker and deployment template**

Export `handleRequest(request, env, fetchImpl = fetch)` for tests and a module-worker default export. Require `WEBHOOK_SECRET`, `GITHUB_TOKEN`, and `GITHUB_REPOSITORY`; bound the body; send GitHub API version and User-Agent headers; never log callback bodies or secrets.

The example Wrangler config names only non-secret settings. The README gives exact dashboard and CLI instructions, including the fine-grained token scope and the final webhook URL shape.

- [ ] **Step 4: Run tests and commit**

```bash
node --test scripts/test-cloudflare-notary-webhook.mjs
git add ops/cloudflare-notary-webhook scripts/test-cloudflare-notary-webhook.mjs
git commit -m "ops: add Apple notarization webhook relay"
```

## Task 6: CI and documentation

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Modify: `docs/REFERENCE.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add orchestration tests to CI**

Add steps:

```yaml
- name: Test release notarization orchestration
  run: python3 -m unittest scripts.test_release_notarization -v

- name: Test notarization webhook relay
  run: node --test scripts/test-cloudflare-notary-webhook.mjs
```

- [ ] **Step 2: Document operational behavior**

Document the `dev.codexify` identity, one-time migration prompt expectation, draft staging, runner-free Apple wait, Cloudflare wake-up-only trust boundary, manual finalizer fallback, internal manifest lifecycle, and required GitHub secrets. State explicitly that installers and self-update discover only published stable releases.

- [ ] **Step 3: Run all non-credential verification**

Run:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all --no-fail-fast
python3 -m unittest scripts.test_release_notarization -v
node --test scripts/test-cloudflare-notary-webhook.mjs
node --test scripts/test-setup-widget.mjs
node --test scripts/test-diff-widget.mjs
sh -n install.sh
sh -n scripts/test-posix-installer.sh
./scripts/test-posix-installer.sh
ruby -e 'require "yaml"; Dir[".github/workflows/*.yml"].each { |p| YAML.load_file(p) }'
git diff --check
```

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml README.md docs/REFERENCE.md docs/ARCHITECTURE.md CHANGELOG.md
git commit -m "docs: document signed asynchronous releases"
```

## Task 7: Apple and GitHub credentials

**Local state:** `~/.codexify/signing/`

- [ ] **Step 1: Generate a private key and CSR locally**

Create a mode-`0700` directory and an encrypted private key/CSR for the account email and team. Never print the private key or password. Store the generated encryption password in the macOS login Keychain under a Codexify-specific service name.

- [ ] **Step 2: Obtain a Developer ID Application certificate**

Use the signed-in Apple Developer account to submit the CSR for a Developer ID Application certificate. If the portal requires browser-side final confirmation, 2FA, CAPTCHA, or agreement acceptance, stop only at that exact boundary and request the minimum user action.

- [ ] **Step 3: Import and package the certificate**

Verify the returned certificate chains to Apple, has Team ID `4HN6WUZ995`, and joins the generated private key. Create an encrypted P12, import it into the login Keychain, and perform a local `dev.codexify` test signature.

- [ ] **Step 4: Obtain a team App Store Connect API key**

Create a team API key suitable for notarization and download its one-time `.p8`. If key creation requires a browser confirmation or account-holder-only action, stop only at the final boundary. Validate it using `xcrun notarytool history` without exposing identifiers beyond the key ID and issuer needed for configuration.

- [ ] **Step 5: Configure GitHub**

Set these values with `gh secret set` from files/stdin without echoing them:

```text
MACOS_DEVELOPER_ID_P12_BASE64
MACOS_DEVELOPER_ID_PASSWORD
APPLE_NOTARY_KEY_P8_BASE64
APPLE_NOTARY_KEY_ID
APPLE_NOTARY_ISSUER_ID
```

Set repository variable:

```text
APPLE_TEAM_ID=4HN6WUZ995
```

Leave `APPLE_NOTARY_WEBHOOK_URL` unset until the user deploys the supplied Worker. After deployment, set it to the secret-path Worker URL and validate a manual finalizer dispatch.

## Task 8: Final integration and publication of source changes

**Files:** all changed files.

- [ ] **Step 1: Inspect aggregate diff and secret hygiene**

Call `show_diff`, review every changed file, and search tracked content for private-key headers, P12/base64 material, GitHub tokens, webhook secrets, and Apple key values. Only public IDs and documented secret names may be tracked.

- [ ] **Step 2: Run fresh full verification**

Repeat Task 6's complete command matrix after credential-independent code is final. If credentials are available, also build one disposable local binary, sign it as `dev.codexify`, and verify its designated requirement.

- [ ] **Step 3: Integrate and push**

Fast-forward the source checkout's `main` to the detached implementation chain, verify `origin/main` has not diverged, push normally, and confirm local/remote refs and GitHub CI.

- [ ] **Step 4: Report only required user actions**

List only unavoidable Apple portal handoffs and exact Cloudflare Worker deployment/secrets. Do not expose generated secret values in chat; give paths or dashboard field names where necessary.
