# Durable Artifact Egress Design

Date: 2026-08-30

## Problem

`export_host_file` currently creates an immutable in-memory snapshot and an opaque `codexify://artifact/<token>` resource capability. Both the token metadata and the snapshot bytes live only in the running Codexify process and expire after `artifactEgress.referenceTtlMs` (5 minutes by default). A service restart, normal TTL expiry, or cache eviction therefore makes a file link already present in a ChatGPT conversation unreadable.

That is a poor fit for ChatGPT conversation history. The conversation can outlive an MCP transport session and a Codexify process by days or months, while the old `resource_link` remains visible to the user. A durable file attachment should keep working for as long as practical.

The current implementation also duplicates the operating system's file cache by retaining full payloads in application RAM. Large exported files can consume substantial memory even when a disk-backed immutable snapshot would provide the same durable semantics and benefit from the OS page cache on repeated reads.

## Goals

1. Make native `export_host_file` resource capabilities survive MCP reconnects and Codexify process restarts.
2. Retain the originally exported immutable bytes for as long as practical under a bounded global per-user disk budget.
3. Prefer a current-source fallback over a dead attachment when the immutable snapshot has been evicted.
4. Keep capability URIs opaque and unguessable.
5. Preserve project confinement and symlink/traversal protections when the source fallback is used.
6. Bound durable snapshot storage globally per user rather than per project or service instance.
7. Remove the native artifact payload cache from application RAM.
8. Keep bridged upstream MCP resource links separate because their routing depends on live upstream peers and cannot be made durable through local file persistence.

## Non-goals

- Making bridged upstream resource links survive an upstream disconnect or Codexify restart.
- Retaining every exported snapshot forever.
- Turning `export_host_file` into a general arbitrary-path file server.
- Replacing ChatGPT's own attachment storage or assuming that ChatGPT eagerly caches resource contents.
- Content-addressed deduplication in the first implementation. Each export has its own immutable snapshot and resource identity.

## User-visible semantics

A native exported resource follows this resolution order:

```text
resource URI
    |
    v
persistent record exists?
    | no
    +------------------------> resource_not_found
    |
   yes
    |
    v
immutable snapshot exists?
    | yes
    +------------------------> serve original exported bytes
    |
   no
    |
    v
recorded source path still resolves to a safe regular file?
    | yes
    +------------------------> serve current source bytes
    |
   no
    +------------------------> resource_not_found
```

The original immutable version is therefore preferred while it remains in the snapshot store. After LRU eviction, the old resource URI becomes a live view of the recorded source path. This behavior is intentional: returning the latest version is considered more useful than failing merely because the historical snapshot was evicted.

Once a snapshot has been evicted, a later fallback read does not recreate an immutable snapshot for that old resource token. Re-freezing an arbitrary later version under an old conversation link would make its semantics depend on which read happened to occur after eviction. The resource remains source-backed after snapshot loss.

If the source file was changed, the fallback may therefore return bytes whose SHA-256 differs from the digest reported by the original `export_host_file` call. The server must not present those bytes as the original snapshot. This distinction should be visible in resource-read metadata/logging where the protocol permits it, while the file itself remains usable.

## Global per-user artifact store

The store is shared by every Codexify process running as the same operating-system user:

```text
~/.codexify/artifacts/
    records/
        <token>.json
    snapshots/
        <token>.blob
    locks/
        store.lock
```

All store directories and files use private permissions where the platform supports them. The existing opaque 256-bit random capability token remains the filename key and URI suffix. The token continues to be the bearer capability; `resources/list` must not enumerate exported artifacts.

The store is deliberately outside project checkouts so Git operations, worktree cleanup, repository deletion, and project switching cannot remove durable resource metadata or retained immutable snapshots accidentally.

### Persistent record

Each record is versioned and contains at least:

```json
{
  "version": 1,
  "token": "opaque-token",
  "sourceRoot": "/canonical/project/root",
  "sourcePath": "relative/path/report.pdf",
  "name": "report.pdf",
  "mimeType": "application/pdf",
  "originalBytes": 12345,
  "originalSha256": "...",
  "createdAtUnixMs": 0,
  "lastAccessedAtUnixMs": 0,
  "snapshotStored": true
}
```

The record must never contain ChatGPT credentials, tunnel credentials, conversation IDs, or MCP session IDs.

`sourceRoot` is the canonical active project root at export time, not merely the server's current root at read time. This is required because the resource can be resolved after a restart and from a replacement MCP transport. `sourcePath` remains relative and must pass the same path-component rules as normal native egress.

The record is written atomically before the resource link is returned. A resource URI is never exposed unless its durable record can be recovered after a crash/restart.

## Immutable snapshots

### No application-level RAM cache

Native artifact payloads are no longer retained in an `Arc<[u8]>` cache. The durable snapshot file is the canonical immutable payload. Repeated reads naturally benefit from the operating system page cache without Codexify keeping a duplicate application-managed copy.

`resources/read` should encode directly from the snapshot file into the response representation without first keeping a second complete raw copy in Codexify-managed state. The MCP response still necessarily materializes its returned blob string according to the SDK/protocol, but the store itself does not retain those bytes in RAM after the call.

### Snapshot eligibility

The default maximum file size eligible for an immutable snapshot is 100 MiB (`104857600` bytes). Files above the snapshot threshold are represented by a durable record but start in source-backed mode: no immutable disk copy is made, and `resources/read` reads the current recorded source path.

The snapshot threshold is distinct from any hard resource-read safety ceiling. The implementation must retain a bounded maximum for bytes returned by one `resources/read` so a huge local file cannot cause unbounded allocation/base64 output. Existing `artifactEgress.maxFileBytes` remains the hard per-read/export safety ceiling for compatibility; a new `snapshotMaxFileBytes` controls immutable-copy eligibility and defaults to the smaller of 100 MiB and `maxFileBytes`. Operators that intentionally allow files above 100 MiB can raise `maxFileBytes` while leaving `snapshotMaxFileBytes` at 100 MiB, causing those larger files to use source-backed mode from the beginning.

This preserves the existing safety bound by default while making snapshot eligibility an independent policy.

### Snapshot creation

For an eligible file:

1. Open the source through the existing capability-confined project access path.
2. Verify it is a regular file and enforce the hard read limit.
3. Stream/hash the exact bytes into a private temporary file inside the global snapshot directory.
4. Flush and sync the temporary file.
5. Atomically publish it as `snapshots/<token>.blob` without overwrite.
6. Persist/update the durable record to indicate that the snapshot is present.
7. Return the resource link only after the record and, when eligible, snapshot are durable.

The snapshot file is never modified in place after publication.

If snapshot creation fails for a reason that does not compromise source safety or record durability, export may degrade to source-backed mode rather than failing the whole export. Failures that indicate unsafe path resolution, source-read failure, size-limit violation, or inability to persist the capability record remain errors.

## Global LRU storage budget

The default immutable snapshot budget is 5 GiB (`5368709120` bytes) globally per operating-system user.

A new configuration field controls the budget:

```json
{
  "artifactEgress": {
    "maxSnapshotBytes": 5368709120,
    "snapshotMaxFileBytes": 104857600
  }
}
```

The store is shared globally even if multiple Codexify processes use different projects. Eviction therefore requires a cross-process lock. Before publishing a new immutable snapshot that would take the store above the active process's configured `maxSnapshotBytes`, Codexify evicts least-recently-used snapshots until the new snapshot fits.

Eviction removes only `snapshots/<token>.blob`. It does not remove the corresponding record. The record is updated to `snapshotStored: false` (or the implementation treats snapshot existence as authoritative) so the old resource URI immediately falls back to its recorded source path.

A successful read from an immutable snapshot updates its LRU access timestamp. Source-backed reads do not make an evicted snapshot re-enter the cache.

The implementation should tolerate stale/corrupt bookkeeping conservatively by deriving actual snapshot occupancy from files on disk while holding the global store lock rather than trusting one unverified aggregate byte counter.

If one eligible snapshot is itself larger than `maxSnapshotBytes`, it is not stored; the export succeeds in source-backed mode subject to the hard per-read limit.

## Record lifetime

Durable resource records have no short default TTL. The purpose of the record is to let an old ChatGPT conversation resolve its attachment after service restarts and snapshot eviction.

The existing native five-minute `referenceTtlMs` behavior is removed. `referenceTtlMs` and `maxReferences` remain applicable to bridged upstream-resource capabilities, which still depend on live upstream peers. Native exported-file records are not subject to those bridged-resource limits.

Records are small and may accumulate. The first implementation does not automatically delete valid native records merely because of age. A future explicit maintenance/doctor command may prune records whose snapshot is absent and whose source has also been gone for a conservative period, but automatic record expiry is outside this change because it would recreate the broken old-conversation behavior we are fixing.

## Source fallback safety

Source fallback must not be implemented as a raw `std::fs::read(record.sourceRoot.join(record.sourcePath))`.

Every fallback read must recreate the capability-confined source boundary:

1. Validate the stored record schema/version and bounded strings.
2. Canonicalize/open the recorded source root safely.
3. Resolve the recorded relative path without absolute components or `..` traversal.
4. Reject symlink/reparse-point escape using the same no-follow/capability checks as native export.
5. Require an existing regular file.
6. Reapply the configured hard resource-read size limit before and during reading.
7. Derive current MIME/name metadata from the durable record rather than trusting a changed filename outside that recorded relative path.

The fallback intentionally does not require the current SHA-256 to equal `originalSha256`. A changed file is served as the latest version because this design explicitly prioritizes usability over historical-byte identity after immutable snapshot eviction.

The implementation should still compute or otherwise make available the current digest when practical for diagnostics/audit, but must not overwrite `originalSha256` in the durable record.

## Concurrency and crash consistency

The global store may be accessed by multiple Codexify processes concurrently. Native registration, snapshot publication, LRU eviction, record updates, and record reads that can race with eviction therefore need cross-process-safe behavior.

Design rules:

- Use one short-held global mutation lock for record/snapshot publication and LRU eviction.
- Do not hold the global lock while base64-encoding/returning a large resource body.
- Open the snapshot file while under the relevant consistency check, then release the lock; an open file handle remains valid if another process unlinks the snapshot during LRU eviction on Unix. On Windows, use a compatible sharing strategy or delay deletion when the file is actively open.
- Record files use atomic temporary-write + sync + rename semantics.
- Snapshot files use private temporary files plus atomic no-overwrite publication.
- An orphan temporary file after a crash is ignored and cleaned opportunistically.
- A snapshot without a valid record is not a resolvable capability and may be reclaimed.
- A record that says a snapshot exists while the file is absent simply falls back to source; snapshot-file existence is ultimately authoritative.

## Configuration evolution

Native and bridged resource policies currently share `ArtifactEgressConfig`. The implementation should preserve existing public configuration where possible while separating their semantics internally.

Proposed resolved native fields:

| Field | Default | Meaning |
|---|---:|---|
| `enabled` | `true` | Enables native egress and bridged-resource proxying as today |
| `maxFileBytes` | `104857600` | Hard maximum bytes returned by one native or bridged resource read |
| `snapshotMaxFileBytes` | `104857600` | Native files at or below this size are eligible for immutable snapshots |
| `maxSnapshotBytes` | `5368709120` | Global per-user native immutable snapshot budget |
| `fallbackToSource` | `true` | When no immutable snapshot exists, serve the recorded current source file |
| `maxReferences` | `64` | Bridged upstream resource references only |
| `referenceTtlMs` | `300000` | Bridged upstream resource capabilities only |

The old `maxCachedBytes` native RAM-cache setting becomes obsolete. For compatibility, the parser may continue accepting it during a deprecation period, but the live config/documentation should no longer describe a native in-memory payload cache. It must not silently override the new 5 GiB durable snapshot default unless an explicit migration rule is documented and tested.

`fallbackToSource` defaults to `true` per the desired product semantics. Setting it to `false` gives operators a strict historical-only mode: after snapshot eviction, the resource returns not found instead of current source bytes.

## Tool result contract

`export_host_file` should stop telling the model/user that the resource expires in five minutes.

The structured receipt should retain:

- project-relative `path`
- `name`
- original `bytes`
- original `sha256`
- `mimeType`

and add enough non-sensitive status for the caller to understand durability, for example:

```json
{
  "path": "report.pdf",
  "name": "report.pdf",
  "bytes": 12345,
  "sha256": "...",
  "mimeType": "application/pdf",
  "snapshotStored": true,
  "fallbackToSource": true
}
```

The opaque capability URI remains only in the `resource_link` content block and is not duplicated into ordinary structured content.

The human-readable tool result should say whether the immutable snapshot was retained or the export is source-backed, without exposing the absolute source root.

## Bridged upstream resources

`codexify://upstream-resource/<token>` behavior remains short-lived and process-local:

- it holds a live `Peer<RoleClient>`;
- a restart necessarily destroys that peer;
- the upstream URI may itself be session-scoped or authorization-bearing;
- blindly persisting/replaying it could widen upstream capability lifetime incorrectly.

Therefore bridged resources continue using `maxReferences` and `referenceTtlMs` and are not stored under `~/.codexify/artifacts/`.

The native and bridged resource stores should stop sharing implementation assumptions even if some configuration fields remain in one block for backward compatibility.

## Startup and maintenance

Codexify should initialize the global artifact store once per process and make it available to every MCP transport session, matching the current process-wide native egress ownership model.

Startup must not scan or load all snapshot bytes. At most it validates/creates the private store directories. Occupancy/LRU scans happen when needed for insertion/eviction, keeping normal startup bounded.

Opportunistic maintenance may remove:

- stale temporary files;
- orphan snapshots with no valid record;
- malformed records that cannot safely resolve a capability, after logging a sanitized warning.

Maintenance must never follow symlinks out of the global store.

## Testing

The implementation needs deterministic tests covering at least:

1. Export creates a durable record and immutable snapshot.
2. A resource remains readable after constructing a new `ArtifactEgressStore`, simulating process restart.
3. Snapshot bytes remain the original version after the project source file changes.
4. LRU eviction removes the least-recently-used snapshot but leaves its durable record.
5. Reading an evicted resource returns the latest safe source bytes when `fallbackToSource=true`.
6. Reading an evicted resource fails when `fallbackToSource=false`.
7. A deleted source plus evicted snapshot returns not found.
8. A file above `snapshotMaxFileBytes` is source-backed from the start.
9. A snapshot larger than the global snapshot budget degrades to source-backed mode.
10. Multiple store instances/process-style actors cannot exceed the global budget through concurrent insertion.
11. Accessing a retained snapshot updates its LRU position.
12. Malformed/corrupt record files fail closed without path escape.
13. Symlink/reparse-point replacement of the recorded source path cannot escape its recorded project root.
14. Replacing the recorded project root with a symlink/reparse-point escape is rejected; an ordinary replacement directory at the same recorded path remains eligible for latest-file fallback.
15. Snapshot/record temporary files are cleaned after failed publication.
16. Old in-memory TTL semantics no longer make native resources disappear after five minutes.
17. Bridged upstream resources retain their existing TTL/reference-count behavior.
18. `export_host_file` output schema and user text no longer advertise `expiresInMs` and correctly report snapshot/source-backed status.
19. Unix private permissions and Windows-safe path behavior remain covered.
20. A server restart does not require an MCP conversation/session identifier to resolve an existing native artifact token.

## Documentation changes

Update README and architecture documentation to describe:

- persistent global per-user native artifact storage;
- 5 GiB default LRU snapshot budget;
- 100 MiB default immutable-snapshot eligibility threshold;
- no native five-minute capability expiry;
- source fallback returning the latest file after snapshot loss;
- distinction between native durable artifact resources and short-lived bridged upstream resources;
- the fact that resource records and snapshots are private bearer-capability state and must not be copied to untrusted users.

The checked-in example/default config should include the new durable snapshot defaults and stop presenting `maxCachedBytes` as an in-memory cache setting.

## Expected result

After this change, an exported attachment shown in an old ChatGPT conversation should normally continue to work across MCP transport replacement and Codexify service restarts. While its immutable snapshot remains under the 5 GiB global LRU budget, the user receives the exact originally exported bytes. After that snapshot is evicted, the same resource link serves the latest safe version at the original project-relative path rather than immediately becoming a dead attachment.

The design spends disk instead of application RAM for historical fidelity, bounds that disk globally per user, and preserves a useful fallback path when historical bytes can no longer be retained.
