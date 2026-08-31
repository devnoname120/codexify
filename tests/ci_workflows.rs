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
}
