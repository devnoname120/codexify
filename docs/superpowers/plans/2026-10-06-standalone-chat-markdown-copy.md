# Standalone Chat Markdown Copy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add standalone-only controls that copy one message's raw Markdown or the complete clean Markdown transcript without changing embedded ChatGPT widget behavior.

**Architecture:** Extend `ChatFile` with one read-only clean-transcript operation and expose it through an authenticated owner-chat endpoint. Keep the existing shared chat component, but render copy controls only when the standalone bridge supplies clipboard callbacks; the embedded widget supplies no such capability.

**Tech Stack:** Rust 2024, Axum, embedded HTML/CSS/JavaScript, Node.js test runner, Playwright Chromium/WebKit.

---

## File map

- `src/markdown_chat/widget.rs`: parse every stored message into clean transcript sections while preserving message Markdown.
- `src/owner_chat.rs`: expose the complete transcript through the authenticated owner-chat API.
- `src/owner_chat.html`: provide standalone clipboard callbacks and fetch the complete transcript.
- `src/markdown_chat_ui.html`: render capability-gated per-message and whole-chat copy controls.
- `scripts/test-owner-chat.mjs`: exercise standalone clipboard behavior in Chromium and WebKit.
- `scripts/test-markdown-chat-widget.mjs`: prove embedded widgets expose no copy controls.
- `docs/REFERENCE.md`, `docs/ARCHITECTURE.md`, `CHANGELOG.md`: document the standalone-only feature and API boundary.

### Task 1: Generate and serve the complete clean transcript

**Files:**
- Modify: `src/markdown_chat/widget.rs:107-196,301-389,530-604`
- Modify: `src/owner_chat.rs:282-448,605-622,788-905`

- [ ] **Step 1: Add failing transcript-format tests**

Add a `clean_markdown_transcript_preserves_source_and_removes_storage_markers` test in `src/markdown_chat/widget.rs` that creates a persistent chat, appends:

```rust
chat.append_user("source-user".into(), "Paragraph  \n\n```rust\nlet x = 1;\n```".into()).await.unwrap();
chat.append("Agent *source*".into()).await.unwrap();
chat.warn_ticket_rejection().await.unwrap();
```

Then assert:

```rust
let transcript = chat.clean_markdown_transcript().await.unwrap();
assert_eq!(
    transcript,
    format!(
        "## You\n\nParagraph  \n\n```rust\nlet x = 1;\n```\n\n## Agent\n\nAgent *source*\n\n## Warning\n\n{}",
        crate::agent_tickets::WARNING
    )
);
assert!(!transcript.contains("codexify-user-message"));
assert!(!transcript.contains("# Codexify Chat"));
```

Add `empty_clean_markdown_transcript_is_empty` and assert a newly ensured chat returns `""`.

Add `clean_markdown_transcript_rejects_the_copy_ceiling` using a test-only limit helper or a small explicit limit, and assert the operation returns `"Chat transcript exceeds the 64 MiB copy limit."` before returning partial text.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```sh
cargo test --lib clean_markdown_transcript
```

Expected: compilation fails because `ChatFile::clean_markdown_transcript` does not exist.

- [ ] **Step 3: Implement one shared message conversion path**

Extract the existing span-to-message conversion from `widget_page` into a helper equivalent to:

```rust
fn widget_message(file: &mut File, span: Span) -> Result<Option<WidgetMessage>, String> {
    let bytes = read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)?;
    let markdown = String::from_utf8(bytes).map_err(|_| "CHAT.md contains incomplete UTF-8")?;
    if markdown.trim().is_empty() {
        return Ok(None);
    }
    let legacy_warning = (span.role == "agent"
        && markdown.starts_with("**Possible duplicate agent detected and blocked.**"))
        || (span.role == "warning"
            && markdown.trim() == "Duplicate agent detected. Its tool call was terminated.");
    let (role, markdown) = if legacy_warning {
        ("warning", crate::agent_tickets::WARNING.to_string())
    } else {
        (span.role, markdown)
    };
    Ok(Some(WidgetMessage {
        id: span.id,
        role: role.into(),
        markdown,
        start: span.start,
        end: span.end,
        created_at_ms: span.created_at_ms,
        tool_call_count: span.tool_call_count,
    }))
}
```

Use it from `widget_page`, then add:

```rust
pub async fn clean_markdown_transcript(self: &Arc<Self>) -> Result<String, String> {
    self.run(|chat| {
        chat.with_cursor(|cursor, file| {
            check_cursor(file, cursor)?;
            let mut sections = Vec::new();
            let mut total_bytes = 0usize;
            for span in spans(file)? {
                let Some(message) = widget_message(file, span)? else { continue };
                let label = match message.role.as_str() {
                    "user" => "You",
                    "agent" => "Agent",
                    "warning" => "Warning",
                    _ => return Err("CHAT.md contains an unsupported message role".into()),
                };
                let section = format!("## {label}\n\n{}", message.markdown);
                let separator = usize::from(!sections.is_empty()) * 2;
                total_bytes = total_bytes
                    .checked_add(separator + section.len())
                    .ok_or("Chat transcript exceeds the 64 MiB copy limit.")?;
                if total_bytes > 64 * 1024 * 1024 {
                    return Err("Chat transcript exceeds the 64 MiB copy limit.".into());
                }
                sections.push(section);
            }
            Ok(sections.join("\n\n"))
        })
    }).await
}
```

Implement the body through a private helper that accepts the byte limit, so the public method passes `64 * 1024 * 1024` while the ceiling test uses a small limit without allocating 64 MiB. This keeps legacy-warning normalization identical between the widget and copied transcript.

- [ ] **Step 4: Run transcript tests and verify GREEN**

Run:

```sh
cargo test --lib clean_markdown_transcript
```

Expected: all transcript tests pass.

- [ ] **Step 5: Add a failing authenticated endpoint test**

Extend `owner_api_lists_sends_and_reads_the_same_persisted_chat` in `src/owner_chat.rs`:

```rust
for index in 0..55 {
    chat.append(format!("Agent answer {index}")).await.unwrap();
}
let copied = client
    .get(format!("{base}/api/chats/{id}/markdown"))
    .bearer_auth("test-token")
    .send()
    .await
    .unwrap();
assert_eq!(copied.status(), StatusCode::OK);
assert_eq!(
    copied.headers()[header::CONTENT_TYPE],
    "text/markdown; charset=utf-8"
);
let copied = copied.text().await.unwrap();
assert!(copied.starts_with("## You\n\nCheck the build\n\n## Agent\n\nAgent answer 0"));
assert!(copied.ends_with("## Agent\n\nAgent answer 54"));
assert_eq!(copied.matches("## Agent\n\n").count(), 55);
```

Also request the same path without authorization and assert `StatusCode::UNAUTHORIZED`.

- [ ] **Step 6: Run the endpoint test and verify RED**

Run:

```sh
cargo test --lib owner_api_lists_sends_and_reads_the_same_persisted_chat
```

Expected: the endpoint request returns 404.

- [ ] **Step 7: Implement the endpoint**

Add an owner handler:

```rust
async fn chat_markdown(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let located = selected(&state.config, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let transcript = chat.clean_markdown_transcript().await.map_err(internal_error)?;
    let mut response = transcript.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    Ok(response)
}
```

Register it as:

```rust
.route("/api/chats/{id}/markdown", get(chat_markdown))
```

- [ ] **Step 8: Run server tests and commit**

Run:

```sh
cargo test --lib clean_markdown_transcript
cargo test --lib owner_api_lists_sends_and_reads_the_same_persisted_chat
```

Expected: all selected tests pass.

Commit:

```sh
git add src/markdown_chat/widget.rs src/owner_chat.rs
git commit -m "feat: expose clean standalone chat transcripts"
```

### Task 2: Add capability-gated copy controls

**Files:**
- Modify: `scripts/test-owner-chat.mjs:21-129`
- Modify: `scripts/test-markdown-chat-widget.mjs`
- Modify: `src/markdown_chat_ui.html:19-79,83-104,110-150,371-437,539-665`
- Modify: `src/owner_chat.html:103-214`

- [ ] **Step 1: Add failing standalone browser assertions**

In `scripts/test-owner-chat.mjs`:

1. Grant clipboard permissions for `http://127.0.0.1:43210` and instrument clipboard writes:

```javascript
const context = await browser.newContext({ permissions:["clipboard-read", "clipboard-write"] });
const page = await context.newPage();
const copied = [];
await page.exposeFunction("recordClipboard", text => copied.push(text));
await page.addInitScript(() => {
  Object.defineProperty(navigator, "clipboard", {
    configurable:true,
    value:{ writeText:text => window.recordClipboard(text) }
  });
});
```

2. Seed the first chat with a message containing source syntax:

```javascript
messages.get(chats[0].id).push({
  id:"raw-one", role:"agent", markdown:"**Rendered**  \n\n```js\nconst x = 1;\n```",
  start:1, end:100, created_at_ms:now, tool_call_count:3
});
```

3. Route `/api/chats/{id}/markdown` before the generic chat-state route and return a transcript containing an older message absent from `messages`.

4. After selecting the chat, click `Copy message Markdown` and assert `copied.at(-1)` exactly equals the raw source string.

5. Click `Copy whole chat as Markdown` and assert the request path was `/api/chats/{id}/markdown` and the copied value contains the server-only older entry.

6. Replace `navigator.clipboard.writeText` with a rejection, click again, and assert the component exposes text matching `Could not copy` through its status region without a page error.

- [ ] **Step 2: Add a failing embedded-widget absence test**

In `scripts/test-markdown-chat-widget.mjs`, after mounting the ordinary bridge-less widget with at least one message, assert:

```javascript
assert.equal(await frame.getByRole("button", { name:"Copy message Markdown" }).count(), 0);
assert.equal(await frame.getByRole("button", { name:"Copy whole chat as Markdown" }).count(), 0);
```

- [ ] **Step 3: Run browser tests and verify RED**

Run:

```sh
node --test scripts/test-owner-chat.mjs
node --test scripts/test-markdown-chat-widget.mjs
```

Expected: owner tests fail because copy controls do not exist; embedded absence assertions already pass.

- [ ] **Step 4: Add capability-gated controls to the shared component**

In `src/markdown_chat_ui.html`:

- Add `#copy-chat` beside `#collapse`, initially hidden.
- Add `.message-tools` and `.message-copy` styles. Keep controls compact, visible on hover/focus, and always visible on coarse-pointer devices.
- Define:

```javascript
const canCopyMessage = typeof bridge?.copyMessage === "function";
const canCopyChat = typeof bridge?.copyChat === "function";
```

- Show `#copy-chat` only when `canCopyChat`.
- When creating a message node, insert a button with accessible name `Copy message Markdown` only when `canCopyMessage`. Its click handler must read the latest `records.get(message.id)?.markdown`, call `bridge.copyMessage`, and report `Message Markdown copied.` or `Could not copy message Markdown.` through the existing `status()` helper.
- Add a `#copy-chat` click handler that calls `bridge.copyChat()` and reports `Whole chat Markdown copied.` or `Could not copy whole chat Markdown.`
- Do not add any direct `navigator.clipboard` access to the shared component.

- [ ] **Step 5: Supply clipboard callbacks only from the owner page**

In the `owner_chat.html` bridge:

```javascript
async copyMessage(markdown) {
  if (!navigator.clipboard?.writeText) throw new Error("Clipboard unavailable");
  await navigator.clipboard.writeText(markdown);
},
async copyChat() {
  if (!navigator.clipboard?.writeText) throw new Error("Clipboard unavailable");
  const response = await api(`/api/chats/${id}/markdown`);
  await navigator.clipboard.writeText(await response.text());
}
```

The embedded setup/widget bridges remain unchanged, so they expose no copy capability.

- [ ] **Step 6: Run browser tests and commit**

Run:

```sh
node --test scripts/test-owner-chat.mjs
node --test scripts/test-markdown-chat-widget.mjs
```

Expected: all Chromium and WebKit tests pass.

Commit:

```sh
git add src/markdown_chat_ui.html src/owner_chat.html scripts/test-owner-chat.mjs scripts/test-markdown-chat-widget.mjs
git commit -m "feat: copy standalone chat markdown"
```

### Task 3: Documentation and complete verification

**Files:**
- Modify: `docs/REFERENCE.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Document exact scope and format**

Under the standalone owner-chat documentation, state that:

```markdown
The standalone page shows a copy control on each displayed message and a header action for the complete chat. Per-message copy uses the original Markdown source. Whole-chat copy requests the complete persisted conversation and emits chronological `## You`, `## Agent`, and `## Warning` sections without internal message markers, timestamps, receipts, or tool-call counters. These clipboard actions are not advertised in the embedded ChatGPT widget.
```

Document the owner-only `/api/chats/{id}/markdown` endpoint in `docs/ARCHITECTURE.md`, including bearer protection and server-side pagination independence.

Add an `[Unreleased]` entry in `CHANGELOG.md`.

- [ ] **Step 2: Format and run focused verification**

Run:

```sh
cargo fmt --all --check
cargo test --lib clean_markdown_transcript
cargo test --lib owner_api_lists_sends_and_reads_the_same_persisted_chat
cargo test --lib embedded_owner_page_reuses_chat_component_without_exposing_an_initial_chat
node --test scripts/test-owner-chat.mjs
node --test scripts/test-markdown-chat-widget.mjs
```

Expected: exit 0 with no failures.

- [ ] **Step 3: Run complete verification**

Run:

```sh
cargo clippy --all-targets -- -D warnings
cargo test --all
node --test scripts/test-setup-widget.mjs
node --test scripts/test-markdown-chat-widget.mjs
node --test --test-concurrency=1 scripts/test-workspace-widget.mjs scripts/test-workspace-picker.mjs scripts/test-owner-chat.mjs scripts/test-chat-workspace-switch.mjs
```

Expected: all non-environment-dependent tests pass in Rust, Chromium, and WebKit.

- [ ] **Step 4: Review, commit, and push**

Call `show_diff` and verify:

- copy controls are capability-gated;
- the embedded bridge has no clipboard callback;
- whole-chat copy cannot omit paginated messages;
- no transcript or local credential was added to fixtures;
- the working tree contains only intended changes.

Commit:

```sh
git add CHANGELOG.md docs/REFERENCE.md docs/ARCHITECTURE.md
git commit -m "docs: describe standalone chat markdown copying"
```

Push the complete worktree chain after verifying upstream `main` is still an ancestor of the local result:

```sh
git fetch origin main
test "$(git merge-base origin/main HEAD)" = "$(git rev-parse origin/main)"
git push origin HEAD:main
```
