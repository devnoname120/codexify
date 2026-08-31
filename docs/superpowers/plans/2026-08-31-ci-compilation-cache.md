# CI Compilation Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce Codexify CI and release compilation latency by reusing release-profile dependency artifacts from the default branch, eliminating redundant compilation, and running independent release validation and builds concurrently.

**Architecture:** Keep `Swatinem/rust-cache@v2`, because the pinned v2 implementation already normalizes workspace package versions and hashes the actual dependency graph. Add a default-branch cache-warmer workflow that performs lookup-only checks and builds release targets only on cache misses. Release-tag jobs restore those stable target-specific caches without writing tag-scoped entries. Ordinary CI uses reduced debug information and relies on `cargo test` instead of a redundant preceding `cargo check` in platform jobs.

**Tech Stack:** GitHub Actions, Cargo, Rust integration tests, `Swatinem/rust-cache@v2`.

---

### Task 1: Add workflow regression tests

**Files:**
- Create: `tests/ci_workflows.rs`

- [ ] **Step 1: Add tests for the intended workflow invariants**

Create tests that read the workflow files from `CARGO_MANIFEST_DIR` and verify:

```rust
use std::{fs, path::Path};

fn workflow(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

fn job_section<'a>(workflow: &'a str, job: &str, next_job: &str) -> &'a str {
    let start = workflow
        .find(&format!("  {job}:\n"))
        .unwrap_or_else(|| panic!("missing {job} job"));
    let rest = &workflow[start..];
    let end = rest
        .find(&format!("\n  {next_job}:\n"))
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn ordinary_ci_uses_lean_debug_info_without_redundant_platform_check() {
    let ci = workflow(".github/workflows/ci.yml");
    assert!(ci.contains("CARGO_PROFILE_DEV_DEBUG: line-tables-only"));
    assert!(ci.contains("CARGO_PROFILE_TEST_DEBUG: line-tables-only"));
    assert!(!ci.contains("cargo check --all-targets"));
    assert!(ci.contains("run: cargo test --all-targets service"));
}

#[test]
fn release_builds_run_parallel_to_validation_and_publish_only_after_both() {
    let release = workflow(".github/workflows/release.yml");
    let build = job_section(&release, "build", "deploy-installers");
    let deploy = job_section(&release, "deploy-installers", "release");
    assert!(!build.contains("needs: check"));
    assert!(deploy.contains("needs: [check, build]"));
}

#[test]
fn release_tag_jobs_only_restore_rust_caches() {
    let release = workflow(".github/workflows/release.yml");
    let build = job_section(&release, "build", "deploy-installers");
    assert!(release.matches("save-if: false").count() >= 2);
    assert!(build.contains("shared-key: release-${{ matrix.target }}"));
}

#[test]
fn default_branch_warms_target_specific_release_dependency_caches() {
    let warmer = workflow(".github/workflows/release-cache.yml");
    assert!(warmer.contains("branches: [main]"));
    assert!(warmer.contains("lookup-only: true"));
    assert!(warmer.contains("shared-key: release-${{ matrix.target }}"));
    assert!(warmer.contains("steps.cache.outputs.cache-hit != 'true'"));
}
```

- [ ] **Step 2: Run the tests and verify the expected failure**

Run:

```bash
cargo test --test ci_workflows
```

Expected: FAIL because the debug-profile settings and release-cache workflow are absent, the platform `cargo check` still exists, and release jobs are still serialized and save tag caches.

### Task 2: Remove redundant ordinary-CI compilation

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Configure smaller CI debug information**

Add these workflow-level environment variables:

```yaml
  CARGO_PROFILE_DEV_DEBUG: line-tables-only
  CARGO_PROFILE_TEST_DEBUG: line-tables-only
```

- [ ] **Step 2: Remove the redundant platform check**

Delete the step that runs:

```yaml
      - name: Compile platform service integration
        run: cargo check --all-targets
```

The replacement `cargo test --all-targets service` invocation compiles every target covered by the removed check before running the matching service tests.

- [ ] **Step 3: Run the focused ordinary-CI regression test**

Run:

```bash
cargo test --test ci_workflows ordinary_ci_uses_lean_debug_info_without_redundant_platform_check
```

Expected: PASS.

### Task 3: Add default-branch release dependency warming

**Files:**
- Create: `.github/workflows/release-cache.yml`

- [ ] **Step 1: Add the cache-warmer workflow**

Create a workflow triggered by relevant pushes to `main`, a twice-weekly schedule, and manual dispatch. Use the same five release targets and runner assignments as `release.yml`. For each target:

```yaml
      - uses: Swatinem/rust-cache@v2
        id: cache
        with:
          shared-key: release-${{ matrix.target }}
          cache-bin: false
          lookup-only: true

      - name: Build release dependencies on cache miss
        if: steps.cache.outputs.cache-hit != 'true'
        shell: bash
        env:
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER: ${{ matrix.cross_linker }}
        run: cargo build --release --locked --target ${{ matrix.target }}
```

Install the Linux ARM64 cross-linker only when that target misses its cache. Keep the workflow independent from required CI so cache warming does not extend the validation critical path.

- [ ] **Step 2: Run the cache-warmer regression test**

Run:

```bash
cargo test --test ci_workflows default_branch_warms_target_specific_release_dependency_caches
```

Expected: PASS.

### Task 4: Reuse main-scoped caches in releases and parallelize independent work

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Match release validation to ordinary CI caching**

Add job-level `CARGO_PROFILE_DEV_DEBUG` and `CARGO_PROFILE_TEST_DEBUG` settings to `check`, and configure its Rust cache with:

```yaml
        with:
          save-if: false
```

- [ ] **Step 2: Restore stable target-specific release caches**

Configure each release build cache with:

```yaml
        with:
          shared-key: release-${{ matrix.target }}
          cache-bin: false
          save-if: false
```

- [ ] **Step 3: Run validation and builds concurrently**

Remove `needs: check` from `build`. Change `deploy-installers` to:

```yaml
    needs: [check, build]
```

This permits the build matrix to start with validation while still preventing installer deployment and release publication unless both succeed.

- [ ] **Step 4: Run the release workflow regression tests**

Run:

```bash
cargo test --test ci_workflows release_builds_run_parallel_to_validation_and_publish_only_after_both
cargo test --test ci_workflows release_tag_jobs_only_restore_rust_caches
```

Expected: PASS.

### Task 5: Validate the complete change

**Files:**
- Verify: `.github/workflows/ci.yml`
- Verify: `.github/workflows/release.yml`
- Verify: `.github/workflows/release-cache.yml`
- Verify: `tests/ci_workflows.rs`

- [ ] **Step 1: Lint GitHub Actions workflows**

Run:

```bash
actionlint .github/workflows/*.yml
```

Expected: exit code 0 with no diagnostics. If `actionlint` is not installed, run it from a temporary downloaded release without modifying repository files.

- [ ] **Step 2: Run formatting and focused tests**

Run:

```bash
cargo fmt --all --check
cargo test --test ci_workflows
```

Expected: both commands exit 0.

- [ ] **Step 3: Run the full project checks**

Run:

```bash
cargo clippy --all-targets -- -D warnings
cargo test --all
```

Expected: both commands exit 0.

- [ ] **Step 4: Review the aggregate diff**

Inspect the final diff for cache-key parity between the warmer and release jobs, correct job dependencies, and absence of unrelated changes. Do not create a commit unless the user explicitly requests one.
