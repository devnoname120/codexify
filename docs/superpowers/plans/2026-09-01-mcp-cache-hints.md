# MCP Cache-Hint Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every Codexify-owned MCP 2026-07-28 cacheable response include the required `ttlMs` and `cacheScope` fields while preserving legacy response shapes and bridged-resource cache policy.

**Architecture:** Keep the policy at the `CodexHandler` protocol boundary. A small helper detects MCP 2026-07-28+ from `RequestContext::protocol_version()` and fills absent hints with `ttlMs: 0` and `cacheScope: private`; each typed list/read result passes through it before return. Bridged resources retain their existing bounded TTL and private scope because the helper only fills missing values.

**Tech Stack:** Rust, RMCP 3.1.3, Tokio duplex integration tests, Cargo fmt/Clippy/test.

---

### Task 1: Add protocol-level regression tests

**Files:**
- Modify: `src/server.rs` test module

- [ ] **Step 1: Import the modern lifecycle and cache types in the test module**

Add `ClientLifecycleMode`, `ClientServiceExt`, `CacheScope`, `ProtocolVersion`, and `ReadResourceRequestParams` to the existing test imports.

- [ ] **Step 2: Add a modern per-request integration test**

Start `CodexHandler` over a Tokio duplex transport and connect with:

```rust
ClientLifecycleMode::Discover {
    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
}
```

Call `list_tools`, `list_prompts`, `list_resources`, `list_resource_templates`, and `read_resource` for `diff_ui::DIFF_UI_URI`. Assert each result has:

```rust
assert_eq!(result.ttl_ms, Some(0));
assert_eq!(result.cache_scope, Some(CacheScope::Private));
```

Also assert the resource read still returns the diff URI and MCP Apps MIME type.

- [ ] **Step 3: Add a legacy integration test**

Connect with the existing legacy `ServiceExt::serve` path, call the same direct list/read operations, and assert their `ttl_ms` and `cache_scope` fields remain `None`.

- [ ] **Step 4: Run the focused tests and verify RED**

Run:

```bash
cargo test server::tests::modern_cacheable_responses_include_required_hints -- --exact
cargo test server::tests::legacy_cacheable_responses_preserve_the_old_wire_shape -- --exact
```

Expected: the modern test fails because the direct results omit both fields; the legacy test passes or remains ready to guard the implementation.

### Task 2: Implement version-gated cache hints

**Files:**
- Modify: `src/server.rs:17-30`
- Modify: `src/server.rs:246-349`

- [ ] **Step 1: Add the required RMCP result imports**

Import `CacheScope`, `ListPromptsResult`, `ListResourceTemplatesResult`, and `ProtocolVersion`.

- [ ] **Step 2: Add the cache-hint helper**

Add a private helper near the other server-boundary helpers:

```rust
const DIRECT_RESPONSE_TTL_MS: u64 = 0;

fn ensure_modern_cache_hints(
    protocol_version: Option<&ProtocolVersion>,
    ttl_ms: &mut Option<u64>,
    cache_scope: &mut Option<CacheScope>,
) {
    if protocol_version.is_some_and(|version| version >= &ProtocolVersion::V_2026_07_28) {
        ttl_ms.get_or_insert(DIRECT_RESPONSE_TTL_MS);
        cache_scope.get_or_insert(CacheScope::Private);
    }
}
```

The `get_or_insert` behavior is required so bridged resources keep their existing remaining-lifetime TTL.

- [ ] **Step 3: Apply the helper to list responses**

In `list_tools` and `list_resources`, retain the request context, build the typed result, and apply the helper before return. Override `list_prompts` and `list_resource_templates` with explicit empty results that use the same helper.

- [ ] **Step 4: Apply the helper to resource reads**

For bridged, exported-artifact, and built-in UI reads, place the `ReadResourceResult` in a mutable local, apply the helper using the current request protocol, and then convert it into `ReadResourceResponse`.

- [ ] **Step 5: Run the focused tests and verify GREEN**

Run the two exact tests from Task 1. Expected: both pass.

### Task 3: Document the compatibility fix

**Files:**
- Modify: `CHANGELOG.md:7-9`
- Modify: `docs/ARCHITECTURE.md:119-127`

- [ ] **Step 1: Add an Unreleased fixed entry**

Document that MCP 2026-07-28 per-request list/read results now include required private no-cache hints, restoring ChatGPT widget ingestion and connector refresh while preserving legacy responses.

- [ ] **Step 2: Update the protocol flow documentation**

Record that cacheable responses are version-gated, direct responses use `0/private`, and bridged resource reads preserve their shorter capability-bounded TTL.

### Task 4: Verify and publish

**Files:**
- Review all modified files

- [ ] **Step 1: Run formatting and focused checks**

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test server::tests::modern_cacheable_responses_include_required_hints -- --exact
cargo test server::tests::legacy_cacheable_responses_preserve_the_old_wire_shape -- --exact
```

- [ ] **Step 2: Run the full test suite**

```bash
cargo test --all-targets --all-features
```

Expected: all tests pass with no warnings.

- [ ] **Step 3: Inspect the aggregate diff**

Call `show_diff` and verify that only the cache-hint compatibility code, tests, design/plan docs, changelog, and architecture notes changed.

- [ ] **Step 4: Reconcile with `origin/main`**

Fetch `origin`, confirm no unexpected divergence, and rebase or merge cleanly if main advanced. Re-run affected tests after reconciliation.

- [ ] **Step 5: Commit and push to main**

```bash
git add src/server.rs CHANGELOG.md docs/ARCHITECTURE.md docs/superpowers/plans/2026-09-01-mcp-cache-hints.md
git commit -m "fix: add MCP cache hints to resource responses"
git push origin HEAD:main
```

Normal hooks remain enabled.