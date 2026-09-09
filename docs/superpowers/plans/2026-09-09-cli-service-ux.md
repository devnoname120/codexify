# CLI and Service UX Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Codexify’s terminal UX readable and cross-platform, add safe config and service-management commands, streamline quickstart, and prevent installers or Windows services from creating confusing background behavior.

**Architecture:** Add a small shared terminal-style module based on `anstream`/`anstyle`, keep persisted service logs plain, and apply color only in interactive presentation. Extend the existing raw JSON configuration path with an atomic config-document editor, preserve all native service backends, and evolve quickstart/installers without migrating existing configs.

**Tech Stack:** Rust 2024, clap, serde/serde_json, tracing, anstream/anstyle, systemd user services, launchd, Windows Task Scheduler/PowerShell, POSIX shell, PowerShell.

---

## Commit map

1. `docs: plan CLI and service UX overhaul`
2. `feat: improve terminal diagnostics and service logs`
3. `feat: add config management commands`
4. `feat: expand service lifecycle controls`
5. `feat: streamline quickstart onboarding`
6. `fix: defer service startup until configuration`

Every commit uses explicit `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` values three hours ahead of its creation time. No commit is pushed.

### Task 1: Shared terminal styling and readable logs

**Files:**
- Create: `src/terminal.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/logging.rs`
- Modify: `src/tool_logging.rs`
- Modify: `src/service.rs`
- Modify: `src/doctor.rs`
- Modify: `src/main.rs`
- Test: `tests/service_logs_cli.rs`
- Test: `tests/doctor_cli.rs`

- [ ] Add failing tests that require ANSI styling on an interactive-style rendering path, plain output when color is disabled, bracketed tool names immediately after the tracing target, and indented JSON request/response payloads.
- [ ] Run the focused tests and confirm they fail because the formatter and colored renderer do not exist.
- [ ] Add `anstream` and `anstyle` as direct dependencies and implement a shared palette plus adaptive stdout/stderr writers.
- [ ] Disable ANSI in service-supervised tracing output so the persisted file is portable; strip legacy SGR sequences while displaying older logs.
- [ ] Change tool lifecycle event messages to begin with `[tool_name]` while retaining structured tracing fields.
- [ ] Implement a streaming log presenter that handles partial lines and rotation, colors timestamps/levels/targets/tool names/metadata, and pretty-prints valid JSON payload fields.
- [ ] Add colored Doctor output while preserving the existing plain renderer and machine-readable `--json` output.
- [ ] Run focused tests, formatting, and Clippy.

### Task 2: Config management CLI

**Files:**
- Create: `src/config_cli.rs`
- Modify: `src/lib.rs`
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Modify: `docs/REFERENCE.md`
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Test: `tests/config_cli.rs`

- [ ] Add failing integration tests for `codexify config`, `path`, `get`, `set`, `unset`, and `edit` command parsing and behavior.
- [ ] Verify missing config prints `{}`, scalar/object retrieval is deterministic, set/unset preserve unrelated keys, malformed paths fail without writes, and editor selection follows `VISUAL`, `EDITOR`, then `nano`/`vi` on Unix or Notepad on Windows.
- [ ] Implement dotted paths with backslash escaping, JSON-or-string value parsing, atomic replacement beside the target, symlink refusal, permission preservation, and private permissions for new user config files.
- [ ] Make `edit` create a minimal object when missing, wait for the editor, and validate that the resulting document is a JSON object.
- [ ] Print the selected path and remind users that a running service must be restarted to apply changes.
- [ ] Run focused tests, formatting, and Clippy.

### Task 3: Service lifecycle and hidden Windows execution

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Modify: `src/service.rs`
- Modify: `tests/service_status_cli.rs`
- Modify: `docs/REFERENCE.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] Add failing parser and backend tests for `service start`, `stop`, and `restart`, verifying they do not change enablement state.
- [ ] Implement native start/stop/restart operations for systemd, launchd, and Task Scheduler while retaining enable/disable semantics.
- [ ] Replace the Windows scheduled-task action with a hidden PowerShell launcher using an encoded command and set `CREATE_NO_WINDOW` for the supervised server child.
- [ ] Verify generated task definitions contain `-WindowStyle Hidden`, preserve Interactive logon for tunnel/network access, and correctly quote executable/config paths.
- [ ] Run focused service tests plus Windows script-generation tests, formatting, and Clippy.

### Task 4: Quickstart flow and presentation

**Files:**
- Modify: `src/quickstart.rs`
- Modify: `src/main.rs`
- Modify: `docs/REFERENCE.md`
- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] Add failing wizard tests requiring mode selection before path entry, multi-project as the new-install default, mode-specific path wording, no intermediate Enter before tunnel ID, and `Codexify` as the default connector/tunnel suggestion.
- [ ] Preserve an existing `multiProject` value as the rerun default without rewriting old configs solely for naming.
- [ ] Add a colored production writer while retaining a plain deterministic test path.
- [ ] Apply consistent headings, prompts, links, success, warning, path, and command styling.
- [ ] Make a new installation default to installing/starting the background service from quickstart; retain an explicit foreground fallback when service installation is declined.
- [ ] Run all quickstart tests, formatting, and Clippy.

### Task 5: Installer behavior and next-step emphasis

**Files:**
- Modify: `install.sh`
- Modify: `install.ps1`
- Modify: `scripts/test-posix-installer.sh`
- Modify: `scripts/test-windows-installer.ps1`
- Modify: `README.md`
- Modify: `docs/REFERENCE.md`
- Modify: `CHANGELOG.md`

- [ ] Extend installer tests so missing config skips service installation, existing config still installs/restarts it, `CODEXIFY_SKIP_SERVICE=1` remains authoritative, and the next-step block has a preceding blank line plus terminal emphasis.
- [ ] Run both script test harnesses where supported and confirm the new expectations fail first.
- [ ] Gate service installation on the selected config file existing (`--config`/`CODEXIFY_CONFIG` semantics, otherwise the user config path).
- [ ] Emit a clear deferred-service message when quickstart must create the config.
- [ ] Render the final restart/quickstart block bold green on ANSI-capable POSIX terminals and green via `Write-Host` on PowerShell, while remaining plain under redirection/`NO_COLOR`.
- [ ] Run shell syntax checks, installer tests, formatting, and repository tests.

### Task 6: Final verification and local integration

**Files:** all changed files.

- [ ] Run `cargo fmt --all --check`.
- [ ] Run `cargo clippy --all-targets -- -D warnings`.
- [ ] Run `cargo test --all`.
- [ ] Run setup/diff widget JavaScript tests and installer syntax/integration tests.
- [ ] Run representative CLI smoke tests for plain and colored Doctor/log/config/service output.
- [ ] Call `show_diff` and inspect the aggregate patch for unrelated changes, secret leakage, accidental migrations, or platform-specific regressions.
- [ ] Create the six thematic commits with future author/committer dates, fast-forward the local `main` checkout to them, and verify `origin/main` remains unchanged and local `main` is ahead with a clean worktree.
