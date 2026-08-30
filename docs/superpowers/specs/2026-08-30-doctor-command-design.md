# Codexify Doctor Command

## Problem

Codexify currently reports configuration, native-service, tunnel-runtime, and
self-update failures through separate startup paths and logs. A failed service
restart can therefore leave the installation unusable while the CLI has no single,
read-only command that identifies the broken layer or returns machine-readable
state for support tooling.

## Goals

- Add `codexify doctor` as a side-effect-free local diagnostic command.
- Validate the same effective configuration that server mode would use, including
  global CLI overrides.
- Distinguish optional unconfigured features from configured features that are
  broken.
- Inspect the native service without changing its enabled or running state.
- Detect a retained self-update lock and direct the user to the service log.
- Validate configured OpenAI tunnel credentials and the installed tunnel runtime
  without downloading, installing, or starting it.
- Probe only Codexify's loopback `/health` endpoint, and only when the native
  service reports that it is running.
- Provide deterministic human output and stable JSON output.

## Non-goals

- `doctor` does not repair configuration, remove update locks, install or restart
  services, download the tunnel runtime, connect to the OpenAI control plane, or
  launch bridged MCP servers.
- It does not attempt to prove that every configured upstream MCP will connect.
  That remains a server-startup responsibility because such probes can execute
  arbitrary configured child programs or contact remote services.
- It does not check for newer Codexify releases.

## Command Contract

```text
codexify doctor [GLOBAL OPTIONS] [--json]
```

Global options such as `--config`, `--work-dir`, `--port`, and tunnel overrides
participate exactly as they do in server mode. `--json` suppresses all incidental
configuration-discovery output and emits one JSON document.

Each check has a stable identifier, one of `pass`, `warning`, `failure`, or
`skipped`, a summary, and optional detail and remediation strings. Human output
uses the same records without terminal colour so it remains deterministic when
redirected.

The command exits with status 0 when there are no `failure` records. Warnings and
skipped optional features do not make the command fail. It exits with status 1
after printing the complete report when one or more checks fail. Ordinary CLI
errors that prevent the command itself from running retain the existing error
format.

The JSON shape is:

```json
{
  "ok": true,
  "version": "1.1.0",
  "platform": { "os": "macos", "arch": "aarch64" },
  "checks": [
    {
      "id": "configuration",
      "status": "pass",
      "summary": "Effective configuration is valid",
      "detail": "workDir=/path; port=3000; mode=multi-project",
      "remediation": null
    }
  ],
  "summary": { "passed": 1, "warnings": 0, "failures": 0, "skipped": 0 }
}
```

## Checks

1. **Runtime** verifies that the running executable can be located and that its
   path names a regular executable file on Unix. Platform and package version are
   reported without invoking another process.
2. **Configuration path** reports whether selection came from `--config`,
   `CODEXIFY_CONFIG`, the user-level default, or built-in defaults. A missing
   selected file is a warning because current server semantics intentionally fall
   back to defaults.
3. **Effective configuration** uses the ordinary resolver in quiet mode. Invalid
   JSON, an absent or invalid work directory, invalid secret references, and all
   other startup validation errors are failures. A successful record reports the
   resolved work directory, port, and single- versus multi-project mode without
   exposing secrets.
4. **Self-update state** passes when no update lock exists. A present lock is a
   warning because it can represent either an active detached updater or residue
   from a failed update; remediation points to `codexify service logs -f` and does
   not remove anything.
5. **Native service** is skipped when no service definition is installed. An
   installed and running service passes. An installed service that is disabled,
   unloaded, stopped, or failed is a failure with `codexify service enable` as the
   remediation. Native-manager query errors are failures.
6. **Local health** is skipped unless the service is running and effective
   configuration is valid. It performs a bounded HTTP request to
   `http://127.0.0.1:<port>/health`, rejects redirects, and requires Codexify's
   `status: "ok"` JSON response. Failure suggests inspecting service logs and
   verifying that `doctor` selected the service's config.
7. **OpenAI tunnel credential** is skipped when native tunnel mode is not
   configured. Otherwise it validates the referenced environment variable or
   private key file using the runtime's existing secret checks and never includes
   the value in the report.
8. **OpenAI tunnel runtime** is skipped when native tunnel mode is not configured.
   An explicit client must pass the existing version and compatibility probes. A
   complete managed installation must pass its manifest, hash, and compatibility
   checks. A not-yet-installed managed runtime is a warning because normal startup
   will install the pinned verified build; a partial or corrupt installation is a
   failure.

## Architecture

A focused `doctor` module owns report types, check orchestration, rendering, and
loopback health probing. Configuration path selection becomes a small public
read-only API, and config loading gains a quiet entry point rather than redirecting
process stdout. Existing server loading keeps its current announcements.

The service module exposes a read-only cross-platform status record. Linux uses
`systemctl --user is-active` and `is-enabled`; macOS uses `launchctl print`; Windows
queries the scheduled task through PowerShell and parses a small JSON result. The
module does not query the native manager when the platform's service definition is
absent, keeping "not installed" cheap and deterministic.

The OpenAI tunnel and self-update modules expose narrow diagnostic helpers that
reuse their existing path, integrity, permission, and compatibility logic. They do
not make network requests or mutate state.

## Testing

- Clap parsing covers `doctor`, `--json`, and global options after the subcommand.
- Unit tests cover report counting, exit semantics, deterministic rendering,
  service-manager output parsing, update-lock inspection, and managed-tunnel state.
- Binary integration tests use isolated home/config directories to verify valid
  human and JSON reports, missing configuration warnings, invalid-configuration
  failure with complete JSON output, and absence of secret leakage.
- The full Rust test suite and formatter must pass on the development platform;
  platform-specific command construction and parsers provide coverage for paths
  that cannot execute natively in that run.
