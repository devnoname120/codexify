# Codexify Doctor Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a read-only `codexify doctor` command with deterministic human/JSON reports, meaningful exit status, and diagnostics for configuration, service, self-update, health, and native tunnel prerequisites.

**Architecture:** A new `doctor.rs` module owns report construction and rendering. Existing modules expose narrow read-only inspection APIs; config loading gains a quiet path so JSON output remains one document. Native service and tunnel checks reuse existing platform/runtime logic rather than duplicating mutation paths.

**Tech Stack:** Rust 2024, clap, serde/serde_json, reqwest, Tokio, native systemd/launchd/Task Scheduler commands, Cargo integration tests.

---

## File Map

- Create `src/doctor.rs`: report model, checks, rendering, loopback health probe.
- Create `tests/doctor_cli.rs`: black-box command and exit-status coverage.
- Modify `src/config.rs`: `doctor` arguments, public config-selection metadata, quiet loading.
- Modify `src/service.rs`: read-only native-service status.
- Modify `src/openai_tunnel.rs`: read-only credential/runtime inspection.
- Modify `src/self_update.rs`: retained update-lock inspection.
- Modify `src/lib.rs` and `src/main.rs`: module export and command dispatch.
- Modify `README.md`, `docs/ARCHITECTURE.md`, and `CHANGELOG.md`: command contract and architecture.

### Task 1: Lock down the CLI and report contract

**Files:**
- Modify: `src/config.rs`
- Create: `src/doctor.rs`
- Test: `src/config.rs`
- Test: `src/doctor.rs`

- [ ] **Step 1: Add failing clap tests**

Add tests that parse:

```rust
let parsed = Cli::try_parse_from([
    "codexify",
    "doctor",
    "--json",
    "--config",
    "/tmp/codexify.config.json",
]);
```

Assert `parsed.config` is preserved and `parsed.command` is
`Some(CliCommand::Doctor(DoctorArgs { json: true }))`.

- [ ] **Step 2: Run the parser test and verify RED**

Run:

```bash
cargo test config::tests::doctor_cli_accepts_json_and_global_options
```

Expected: compilation fails because `CliCommand::Doctor` and `DoctorArgs` do not exist.

- [ ] **Step 3: Add failing report-model tests**

Create tests for the wished-for API:

```rust
let report = DoctorReport::new(vec![
    DoctorCheck::pass("runtime", "Runtime is usable"),
    DoctorCheck::warning("config_path", "Config file is absent"),
    DoctorCheck::failure("configuration", "Configuration is invalid"),
    DoctorCheck::skipped("service", "Service is not installed"),
]);
assert!(!report.ok);
assert_eq!(report.summary.failures, 1);
assert_eq!(report.summary.warnings, 1);
assert!(report.render_human().contains("FAIL configuration"));
```

Also serialize the report and assert snake-case status values and a stable summary.

- [ ] **Step 4: Run report tests and verify RED**

Run:

```bash
cargo test doctor::tests
```

Expected: compilation fails because the report types do not exist.

- [ ] **Step 5: Implement the minimal parser and report types**

Add:

```rust
#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
}
```

and `CliCommand::Doctor(DoctorArgs)`. In `doctor.rs`, add serializable
`DoctorStatus`, `DoctorCheck`, `DoctorSummary`, `DoctorPlatform`, and
`DoctorReport` types plus deterministic human rendering and summary counting.

- [ ] **Step 6: Run the focused tests and verify GREEN**

Run:

```bash
cargo test config::tests::doctor_cli_accepts_json_and_global_options
cargo test doctor::tests
```

Expected: both pass.

### Task 2: Make configuration diagnostics quiet and structured

**Files:**
- Modify: `src/config.rs`
- Test: `src/config.rs`
- Test: `tests/doctor_cli.rs`

- [ ] **Step 1: Add failing config-selection and JSON integration tests**

Expose the wished-for selection API in a unit test:

```rust
let selection = config_path_selection_with(
    Some("config.json"),
    None,
    Path::new("/tmp/cwd"),
    Some(Path::new("/tmp/home")),
);
assert_eq!(selection.source, ConfigPathSource::CommandLine);
assert_eq!(selection.path, Some(PathBuf::from("/tmp/cwd/config.json")));
```

Create `tests/doctor_cli.rs` with an isolated `HOME`, `CODEX_HOME`, existing
`workDir`, and config containing `"codexMcp": { "enabled": false }`. Run
`codexify doctor --json --config <path>` and assert stdout parses as exactly one
JSON value with passing `config_path` and `configuration` checks.

- [ ] **Step 2: Run the tests and verify RED**

Run:

```bash
cargo test config::tests::doctor_config_selection_is_public_and_stable
cargo test --test doctor_cli valid_json_report_is_clean_and_successful
```

Expected: the unit test fails to compile for missing public APIs and the binary
integration test fails because `doctor` is not dispatched.

- [ ] **Step 3: Implement quiet configuration loading**

Make `ConfigPathSource` and `ConfigPathSelection` public, expose
`config_path_selection(&Cli)`, and add `load_config_quiet(Cli)`. Refactor the
existing loader into:

```rust
fn load_config_with_announcements(cli: Cli, announce: bool) -> Result<AppConfig, String>;

pub fn load_config(cli: Cli) -> Result<AppConfig, String> {
    load_config_with_announcements(cli, true)
}

pub fn load_config_quiet(cli: Cli) -> Result<AppConfig, String> {
    load_config_with_announcements(cli, false)
}
```

Thread `announce` through MCP discovery and native-worktree diagnostics. Existing
server mode must retain its current stdout/stderr announcements.

- [ ] **Step 4: Add initial doctor orchestration and dispatch**

`doctor::run(cli).await` must add runtime, config-path, and effective-config
checks. `main.rs` must print human or JSON output and use a dedicated already-
reported failure marker so failed reports exit 1 without appending a generic
`Error:` line.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```bash
cargo test config::tests
cargo test --test doctor_cli valid_json_report_is_clean_and_successful
```

Expected: all pass and JSON stdout contains no discovery banner.

### Task 3: Diagnose self-update and native service state

**Files:**
- Modify: `src/self_update.rs`
- Modify: `src/service.rs`
- Modify: `src/doctor.rs`
- Test: `src/self_update.rs`
- Test: `src/service.rs`
- Test: `tests/doctor_cli.rs`

- [ ] **Step 1: Add failing update-lock tests**

Test a helper that accepts a home directory and returns `None` without a lock,
then returns a record containing the lock path and trimmed update id when
`~/.codexify/update/update.lock` exists. Empty and oversized lock contents must
not be echoed as arbitrary text.

- [ ] **Step 2: Add failing native-status parser tests**

Add pure tests for launchd output (`state = running`, `pid = 123`, and stopped
state), systemd `active/enabled` combinations, and Windows scheduled-task JSON.
The public result is:

```rust
pub struct ServiceStatus {
    pub installed: bool,
    pub running: bool,
    pub enabled: Option<bool>,
    pub definition_path: Option<PathBuf>,
    pub detail: String,
}
```

- [ ] **Step 3: Run tests and verify RED**

Run:

```bash
cargo test self_update::tests::inspect_update_lock
cargo test service::tests::service_status
```

Expected: compilation fails for missing inspection APIs.

- [ ] **Step 4: Implement read-only inspection**

Add `self_update::inspect_update_lock()` and a test-only/path-parameterized helper.
Add `service::status()` with platform implementations:

- Linux: require the user-unit file, then query `systemctl --user is-active` and
  `is-enabled`.
- macOS: require the LaunchAgent plist, then parse bounded `launchctl print` output.
- Windows: query `Get-ScheduledTask` and parse compact JSON.

No code path may enable, start, stop, install, or remove the service.

- [ ] **Step 5: Add doctor checks and verify GREEN**

Map no service to `skipped`, healthy running service to `pass`, and installed but
stopped/disabled/query-failed state to `failure` with `codexify service enable` as
remediation. Map a retained update lock to `warning` with service-log guidance.
Run the focused tests and isolated no-service integration test.

### Task 4: Diagnose health and native tunnel prerequisites

**Files:**
- Modify: `src/openai_tunnel.rs`
- Modify: `src/doctor.rs`
- Test: `src/openai_tunnel.rs`
- Test: `src/doctor.rs`
- Test: `tests/doctor_cli.rs`

- [ ] **Step 1: Add failing tunnel-inspection tests**

Define the wished-for APIs:

```rust
pub(crate) fn validate_key_reference(reference: &str) -> anyhow::Result<()>;

pub(crate) enum TunnelRuntimeInspection {
    Ready(PathBuf),
    MissingManaged(PathBuf),
}

pub(crate) async fn inspect_runtime(
    settings: &OpenAiTunnelConfig,
) -> anyhow::Result<TunnelRuntimeInspection>;
```

Test valid/malformed environment-backed keys, absent managed runtime, partial
managed installation, and an explicit incompatible executable. Secret values must
not appear in errors.

- [ ] **Step 2: Add failing health-probe tests**

Start bounded loopback test servers returning valid Codexify health JSON, malformed
JSON, and redirects. Assert only the first passes and that the detail includes the
reported tool count.

- [ ] **Step 3: Run tests and verify RED**

Run:

```bash
cargo test openai_tunnel::tests::doctor_
cargo test doctor::tests::health_
```

Expected: compilation fails for missing diagnostic APIs.

- [ ] **Step 4: Implement tunnel inspection and health probing**

Wrap the existing key resolver so the resolved secret is immediately dropped.
For explicit clients, reuse `validate_client`. For managed clients, compute the
existing pinned paths without downloading: both absent means `MissingManaged`,
both present means `validate_managed_install`, and partial state is an error.

Build the Codexify health client with no proxy, no redirects, a three-second
timeout, a fixed loopback URL, bounded response bytes, and a required
`{"status":"ok"}` response.

- [ ] **Step 5: Add orchestration and verify GREEN**

Configured valid keys and runtimes pass; missing managed runtime warns; malformed,
partial, or incompatible configured state fails. Unconfigured tunnel mode skips
both checks. Probe local health only when service status is running and config
loading succeeded.

### Task 5: Complete binary behavior, documentation, and verification

**Files:**
- Modify: `tests/doctor_cli.rs`
- Modify: `README.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add failing end-to-end cases**

Cover:

- invalid config returns status 1 but still emits complete parseable JSON;
- missing selected config is a warning and becomes a configuration failure only
  when no effective `workDir` exists;
- malformed tunnel credentials return failure without leaking the environment
  value;
- human output uses `PASS`, `WARN`, `FAIL`, and `SKIP` records and a final summary.

- [ ] **Step 2: Run end-to-end tests and verify RED**

Run:

```bash
cargo test --test doctor_cli
```

Expected: newly added cases fail for missing behavior.

- [ ] **Step 3: Finish minimal implementation and documentation**

Add the command to the README command table, document checks/exit semantics and
`--json`, add the module to the architecture map/startup diagnostics section, and
add an Unreleased changelog entry.

- [ ] **Step 4: Verify formatting and all tests**

Run:

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Expected: all commands exit 0 with no warnings.

- [ ] **Step 5: Inspect and publish**

Run `git diff --check`, call `show_diff` for the project-open aggregate, inspect the
result, commit the implementation and documentation, then push
`add-doctor-command` to `origin`.
