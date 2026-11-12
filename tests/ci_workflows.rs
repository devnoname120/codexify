use std::{fs, path::Path};

fn workflow(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
        .replace("\r\n", "\n")
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
fn workflow_contracts_accept_windows_checkout_line_endings() {
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), "jobs:\r\n  check:\r\n    runs-on: ubuntu-latest\r\n  service-platforms:\r\n    runs-on: windows-latest\r\n").unwrap();
    let contents = workflow(file.path().to_str().unwrap());
    assert!(job_section(&contents, "check", "service-platforms").contains("ubuntu-latest"));
    assert!(!job_section(&contents, "check", "service-platforms").contains("windows-latest"));
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
fn legacy_migration_regressions_run_on_the_service_platforms() {
    let ci = workflow(".github/workflows/ci.yml");
    let platforms = job_section(&ci, "service-platforms", "unused");
    assert!(platforms.contains("macos-15"));
    assert!(platforms.contains("windows-latest"));
    assert!(platforms.contains("cargo test --test legacy_migration"));
    assert!(job_section(&ci, "check", "diff-widget").contains("cargo test --all"));
}

#[test]
fn notification_integration_is_not_silently_skipped_in_ci() {
    let ci = workflow(".github/workflows/ci.yml");
    for section in [
        job_section(&ci, "check", "diff-widget"),
        job_section(&ci, "service-platforms", "unused"),
    ] {
        assert!(section.contains("scripts/requirements-notifications.txt"));
        assert!(section.contains("python -m unittest scripts.test_apprise_notifications"));
        assert!(section.contains("CODEXIFY_TEST_APPRISE_PYTHON:"));
        assert!(
            section.contains("cargo test --test markdown_chat_notifications --test markdown_chat_tools -- --include-ignored")
        );
    }
}

#[test]
fn browser_companion_contract_is_built_and_exercised_in_ci() {
    let ci = workflow(".github/workflows/ci.yml");
    let check = job_section(&ci, "check", "diff-widget");

    assert!(check.contains("oven-sh/setup-bun@v2.2.0"));
    assert!(check.contains("bun-version: 1.4.0"));
    assert!(check.contains("bun install --frozen-lockfile"));
    assert!(check.contains("node scripts/generate-browser-companion-protocol-fixtures.mjs"));
    assert!(check.contains("node scripts/update-browser-companion-upstream.mjs --check"));
    assert!(check.contains("bun test"));
    assert!(check.contains("bun run typecheck"));
    assert!(check.contains("bun run build"));
    assert!(check.contains("node --test scripts/test-browser-companion-boundary.mjs"));
    assert!(check.contains("CODEXIFY_TEST_BROWSER_COMPANION_BUN:"));
    assert!(
        check.contains("cargo test --test browser_companion_contract -- --ignored --nocapture")
    );
}

#[test]
fn browser_companion_packages_are_smoked_and_published_with_each_release() {
    let ci = workflow(".github/workflows/ci.yml");
    let check = job_section(&ci, "check", "diff-widget");
    assert!(check.contains("bun run package"));
    assert!(check.contains("bun run smoke:package"));
    assert!(check.contains("patchright install-deps chromium"));
    assert!(check.contains("bun run install:chrome"));
    assert!(check.contains("CODEXIFY_TEST_BROWSER_COMPANION_CHROME=1 xvfb-run -a bun test"));

    let release = workflow(".github/workflows/release.yml");
    let check = job_section(&release, "check", "build");
    assert!(check.contains("CODEXIFY_TEST_BROWSER_COMPANION_BUN:"));
    assert!(
        check.contains("cargo test --test browser_companion_contract -- --ignored --nocapture")
    );
    let browser = job_section(&release, "browser-build", "deploy-installers");
    for platform in [
        "linux-x64",
        "linux-arm64",
        "darwin-x64",
        "darwin-arm64",
        "windows-x64",
    ] {
        assert!(
            browser.contains(platform),
            "browser release matrix missing {platform}"
        );
    }
    assert!(browser.contains("oven-sh/setup-bun@v2.2.0"));
    assert!(browser.contains("bun install --frozen-lockfile"));
    assert!(browser.contains("bun run package --"));
    assert!(browser.contains("bun run smoke:package"));
    assert!(browser.contains("patchright install-deps chromium"));
    assert!(browser.contains("actions/upload-artifact@v5"));

    let stage = job_section(&release, "stage-release", "unused");
    assert!(stage.contains("needs: [check, build, browser-build, deploy-installers]"));
    assert!(stage.contains("pattern: codexify-*"));
}

#[test]
fn release_builds_run_parallel_to_validation_and_publish_only_after_both() {
    let release = workflow(".github/workflows/release.yml");
    let build = job_section(&release, "build", "deploy-installers");
    let deploy = job_section(&release, "deploy-installers", "release");

    assert!(!build.contains("needs: check"));
    assert!(build.contains("codexify.config.example.json"));
    assert!(deploy.contains("needs: [check, build]"));
}

#[test]
fn release_tag_jobs_only_restore_rust_caches() {
    let release = workflow(".github/workflows/release.yml");
    let check = job_section(&release, "check", "build");
    let build = job_section(&release, "build", "deploy-installers");

    assert!(check.contains("save-if: false"));
    assert!(build.contains("shared-key: release-${{ matrix.target }}"));
    assert!(build.contains("cache-bin: false"));
    assert!(build.contains("save-if: false"));
}

#[test]
fn default_branch_warms_target_specific_release_dependency_caches() {
    let warmer = workflow(".github/workflows/release-cache.yml");

    assert!(warmer.contains("branches: [main]"));
    assert!(warmer.contains("schedule:"));
    assert!(warmer.contains("workflow_dispatch:"));
    assert!(warmer.contains("lookup-only: true"));
    assert!(warmer.contains("shared-key: release-${{ matrix.target }}"));
    assert!(warmer.contains("cache-bin: false"));
    assert!(warmer.contains("steps.cache.outputs.cache-hit != 'true'"));

    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ] {
        assert!(warmer.contains(target), "warmer missing target {target}");
    }

    let browser = job_section(&warmer, "browser", "unused");
    assert!(browser.contains("actions/cache@v4"));
    assert!(browser.contains("browser-companion/bun.lock"));
    assert!(browser.contains("~/.bun/install/cache"));
    assert!(browser.contains("electron-builder"));
    assert!(browser.contains("bun run package --"));

    let release = workflow(".github/workflows/release.yml");
    let browser_release = job_section(&release, "browser-build", "deploy-installers");
    assert!(browser_release.contains("actions/cache/restore@v4"));
    assert!(browser_release.contains("browser-companion/bun.lock"));
}
