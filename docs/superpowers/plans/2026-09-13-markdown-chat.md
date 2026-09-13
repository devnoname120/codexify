# Markdown Chat Implementation Plan

**Goal:** Opt-in, per-conversation Markdown communication without changing the installed service or configuration during implementation.

**Architecture:** `markdown_chat` owns per-workspace/per-conversation files, cursors, waiting and optional ntfy delivery. Chat tools consume messages; the common MCP dispatch boundary peeks without consuming and attaches the optional unbudgeted `new_chat_message_from_user`. Existing tool output validation runs before augmentation. Explicit schema revision labels include the enabled flag rather than hashing schemas.

**Tech stack:** Rust, Tokio, notify, reqwest, existing conversation identity and project metadata paths, Rust integration tests and setup-widget Node tests.

## Requirements and decisions

- JSON section: `markdownChat`, disabled by default; default `maxWaitMs: 270000`, configurable from 1000 through 300000. The tools do not accept a timeout argument.
- `ntfy` is optional and holds `url` and optional `token` directly in local JSON. Secrets and chat messages must not appear in payload diagnostics.
- Store `CHAT.md` and cursor state under `memory_dir(config)/chats/<conversation-key>/`; separate chats remain isolated even in one checkout. Generic transports use a unique transport channel and an in-memory cursor.
- Only `chat_write` appends agent messages. It returns unread user text that preceded its append. Reads/awaits consume; passive result delivery never consumes.
- Preserve complete UTF-8 text, including whitespace. Reject oversized unread segments without consuming, rather than truncating. Detect broken append-only cursor boundaries and report a recoverable error.
- Use a native directory watcher with polling fallback and cancellation. Avoid lost wakeups around watcher registration and check once more at deadline.
- Timeout guidance requests another await while the channel remains active. Report notification acceptance, not an unverified human read receipt; respect explicit user stop and host cancellation.
- Augment all advertised top-level output schemas, not private widget metadata. Preserve native object shapes and wrap incompatible upstream schemas/results without overwriting their fields.
- Initialize a conversation channel after authorization and workspace resolution, including setup, selection, resumption, and legacy bindings. Never create or disclose another conversation's channel before authorization.
- Brief supplies the current chat path and directs live reads/writes through chat tools; historical read_file/grep access is allowed only to this conversation's file. The unrestricted shell is not a filesystem sandbox.
- Schema status distinguishes current server configuration from the schema advertised to a connector/conversation. Warn on either toggle with the same binary version, including missing discovery identity; do not claim an unobserved refresh succeeded.

## Execution

1. Add failing storage/config tests; implement configuration, private creation, conversation isolation, persistent cursor, append race handling and non-truncation. Run the targeted tests and commit the coherent storage/config unit.
2. Add tools, notification and watcher tests; implement the three tools and per-request context, registry and brief integration. Test deadline/cancel/atomic editor save/notification failures and commit.
3. Add dispatch/output-schema and widget toggle regression tests; implement passive messages after budgeting/logging, schema revision awareness, and current-channel historical reads. Test native/bridge/catalog/widget/error outputs with tiny budgets and commit.
4. Update README, reference, architecture, example config and changelog. Run formatting, Clippy, all Rust tests, release tests and widget/browser tests. Inspect the aggregate diff, commit documentation, fast-forward and push main normally, then inspect GitHub CI.

## Timeout evidence

A first-hand report describes approximately five-minute failures, and later shorter failures: https://community.openai.com/t/agentsdk-and-chatgpt-ui-fails-running-time-consuming-mcp-tool-with-typeerror-fetch-failed/1366562

OpenAI support says there is no documented fixed ChatGPT web MCP timeout: https://community.openai.com/t/progress-notifications-not-working-in-chatgpt-mcp-ts-sdk-1-20-0/1367559/5

270000 ms is the user's requested default, not a guaranteed platform maximum. Shorter configurable waits remain the fallback.
