# MCP 2026-07-28 cache-hint compatibility design

## Problem

ChatGPT's current per-request MCP ingestion path validates `resources/read` results against the MCP 2026-07-28 cacheable-result schema. Codexify returns `resultType: "complete"` through RMCP 3.1.3, but its direct `List*Result` and `ReadResourceResult` constructors leave `ttlMs` and `cacheScope` unset. ChatGPT therefore rejects otherwise valid widget HTML with `424 Failed Dependency`, drops the templates during connector refresh, and later reports `HTML asset not found` when rendering a tool result.

The widget HTML, tool-result metadata, and resource URI are downstream of this failure and must not be changed as part of the fix.

## Protocol policy

Codexify will attach cache hints to every cacheable response it owns when the request negotiated MCP 2026-07-28 or newer:

- `tools/list`
- `prompts/list`
- `resources/list`
- `resources/templates/list`
- direct `resources/read` results for built-in UI and exported artifacts

The initial policy is:

```json
{
  "ttlMs": 0,
  "cacheScope": "private"
}
```

`ttlMs: 0` is deliberately conservative. Codexify's tool descriptors depend on configuration, and its versioned-looking widget URIs have historically received compatible HTML changes without every binary release changing the URI. Reusing these responses across a connector update could therefore preserve stale schemas or HTML. `cacheScope: "private"` prevents sharing across authorization contexts and is also required for capability-bearing exported resources.

Bridged upstream resources already clamp the upstream TTL to the remaining lifetime of Codexify's opaque capability and force `cacheScope: "private"`. That stricter policy remains intact rather than being replaced with zero TTL.

For protocol versions older than 2026-07-28, Codexify will preserve the historical wire shape and omit the new cache-hint fields on direct responses. This matches RMCP's existing version-gated handling of `resultType` and avoids imposing new fields on strict legacy clients.

## Implementation boundary

A small server-local helper will determine whether cache hints are required from `RequestContext::protocol_version()`. Typed result constructors will then add the selected policy before returning from each handler. Empty prompt and resource-template lists will be overridden explicitly because RMCP's default handlers also omit the new fields.

No configuration option is needed. Cache correctness is a protocol invariant rather than a user preference.

## Verification

Regression tests will prove that:

1. Every Codexify-owned cacheable result serializes `ttlMs` and `cacheScope` for MCP 2026-07-28.
2. Direct legacy responses omit those fields.
3. Bridged-resource TTL and private scope remain preserved.
4. Built-in widget resource reads still return the same HTML, MIME type, URI, and metadata.
5. The full formatting, Clippy, and test suites pass.

A live connector refresh remains the final integration check because ChatGPT's private ingestion endpoint cannot be reproduced by the repository test suite. A successful refresh should restore the Templates list and eliminate the downstream `HTML asset not found` response.