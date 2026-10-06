# Standalone Chat Markdown Copy Design

**Date:** 2026-10-06

## Goal

Add raw-Markdown copy controls to Codexify's standalone owner-chat page without exposing clipboard controls in the embedded ChatGPT widget.

The standalone page will support:

- copying one displayed message's original Markdown;
- copying the complete conversation as a clean Markdown transcript, including messages that are not currently loaded in the paginated UI.

## Scope

The feature applies only when the shared Markdown chat component is mounted by `owner_chat.html`. The embedded ChatGPT widget keeps its existing appearance and behavior.

No message, cursor, receipt, waiting state, or conversation file is modified by a copy operation.

## User interface

### Per-message copy

Every displayed user, agent, and warning message receives a small copy icon. The control copies the exact `markdown` string carried by that message record. It therefore preserves source whitespace, links, code fences, lists, and any other Markdown syntax rather than copying rendered text.

The control is keyboard accessible and has an explicit accessible name. Success is acknowledged briefly without replacing the message body. Clipboard failure produces a short accessible error and leaves the chat unchanged.

### Whole-chat copy

The standalone chat header receives a **Copy whole chat** icon beside the existing Collapse control.

The result is a clean chronological transcript. Each entry is represented as a Markdown heading followed by the unmodified message source:

```markdown
## You

<original Markdown>

## Agent

<original Markdown>

## Warning

<original Markdown>
```

Entries are separated by one blank line. Codexify's `CHAT.md` header, internal HTML comments, cursor state, delivery receipts, timestamps, and tool-call markers are omitted.

## Architecture

### Shared component capability

`mountCodexifyChat` continues to render both the embedded widget and standalone page. Clipboard UI is capability-driven rather than host-name-driven.

The standalone bridge supplies two optional callbacks:

- `copyMessage(markdown)` copies one message;
- `copyChat()` obtains and copies the complete clean transcript.

The component renders clipboard controls only when the relevant callback exists. The embedded ChatGPT widget supplies neither callback, so no clipboard buttons appear there and no assumption is made about ChatGPT sandbox clipboard permissions.

### Complete transcript endpoint

The owner-chat server adds an authenticated endpoint for one selected conversation's clean Markdown transcript. It reads the same validated persisted chat selected by the other owner-chat APIs and converts all parsed message spans in chronological order.

Generating the transcript server-side avoids coupling copy behavior to UI pagination and guarantees that older, currently unloaded messages are included. The endpoint returns UTF-8 plain text with a Markdown content type and the same bearer-token protection as existing owner-chat routes.

Because the browser clipboard API requires one complete string, Codexify applies a 64 MiB transcript ceiling before allocating the result. Larger chats fail with a clear error and are not partially copied.

### Message-source preservation

Per-message copy uses the already-returned `WidgetMessage.markdown` value. Whole-chat generation uses the same message parser and body ranges as widget pagination, so both paths preserve the source body and classify user, agent, and warning records consistently.

Manual text outside reserved message blocks retains the parser's current user-message classification.

## Error handling

- Missing or invalid owner-chat credentials continue to fail through the existing authentication middleware.
- A missing conversation returns the existing not-found behavior.
- An incomplete or invalid chat file returns a bounded server error and copies nothing.
- A clean transcript above 64 MiB is rejected before the complete string is allocated.
- Clipboard rejection or absence is reported inside the selected chat without changing server state.
- Repeated clicks are safe and do not affect read or delivery cursors.

## Testing

### Rust

Add transcript-format tests proving:

- user, agent, and warning headings are emitted in chronological order;
- original Markdown, including whitespace and fenced code, is preserved;
- storage headers and internal marker comments are excluded;
- the authenticated owner endpoint returns the entire conversation rather than one widget page;
- empty conversations return an empty transcript.

### Browser

Extend Chromium and WebKit owner-chat tests to prove:

- per-message copy controls appear in the standalone page;
- the copied value is the exact raw Markdown, not rendered text;
- the whole-chat action requests the complete transcript and copies it;
- success and clipboard-failure feedback are accessible;
- copy controls do not appear in the embedded ChatGPT widget harness.

## Non-goals

- Copying rendered HTML or rich text.
- Exporting a file instead of placing text on the clipboard.
- Adding clipboard permissions or fallbacks inside the ChatGPT sandbox.
- Copying timestamps, receipts, tool-call markers, internal storage comments, or pending unsent drafts.
