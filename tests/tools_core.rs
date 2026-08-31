// Integration tests for the core tool set, ported from the Bun/TypeScript
// suite `src/tools/__tests__/codex-tools.test.ts`.
//
// Assertions are adapted to the actual Rust behavior (exact user-facing strings
// were confirmed by reading the source of each tool under test). exec_command /
// write_stdin from the TS file are intentionally NOT ported here (see the note
// at the bottom of this file).

use std::io::Cursor;
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use tempfile::TempDir;

use codexify::artifact_egress::ArtifactEgressStore;
use codexify::config::default_config;
use codexify::conversation_auth::ConversationAuthorizationStore;
use codexify::diff::DiffCheckpointManager;
use codexify::exec_sessions::SessionState;
use codexify::tool::{Tool, ToolRequestContext};
use codexify::tools::apply_patch::ApplyPatch;
use codexify::tools::clock_curr_time::ClockCurrTime;
use codexify::tools::clock_sleep::ClockSleep;
use codexify::tools::read_file::ReadFile;
use codexify::tools::update_plan::UpdatePlan;
use codexify::tools::view_image::ViewImage;
use codexify::tools::write_file::WriteFile;
use codexify::types::{ArtifactEgressConfig, ToolContent};
use tokio_util::sync::CancellationToken;

/// A config rooted at `dir` with the memory (plan-persistence) directory pinned
/// inside a temp path, so update_plan never writes to the real state directory.
fn config_in(dir: &TempDir) -> codexify::types::AppConfig {
    let mut config = default_config(dir.path().to_path_buf());
    config.memory.dir = Some(dir.path().join(".state").to_string_lossy().into_owned());
    config
}

// --- apply_patch ------------------------------------------------------------

#[tokio::test]
async fn apply_patch_adds_a_new_file() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(
            json!({ "input": "*** Begin Patch\n*** Add File: added.txt\n+hello\n+world\n*** End Patch\n" }),
            &config,
            &session,
        )
        .await;

    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    assert!(r.joined_text().contains("A added.txt"));
    let written = std::fs::read_to_string(dir.path().join("added.txt")).unwrap();
    assert_eq!(written, "hello\nworld\n");
}

#[tokio::test]
async fn apply_patch_updates_a_file_in_place() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("update.txt"), "one\ntwo\nthree\n").unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(
            json!({ "input": "*** Begin Patch\n*** Update File: update.txt\n@@\n-two\n+TWO\n*** End Patch\n" }),
            &config,
            &session,
        )
        .await;

    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    let written = std::fs::read_to_string(dir.path().join("update.txt")).unwrap();
    assert_eq!(written, "one\nTWO\nthree\n");
}

#[tokio::test]
async fn apply_patch_moves_a_file() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("src.txt"), "body\n").unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(
            json!({ "input": "*** Begin Patch\n*** Update File: src.txt\n*** Move to: moved.txt\n@@\n-body\n+moved body\n*** End Patch\n" }),
            &config,
            &session,
        )
        .await;

    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    assert!(r.joined_text().contains("R src.txt -> moved.txt"));
    let written = std::fs::read_to_string(dir.path().join("moved.txt")).unwrap();
    assert_eq!(written, "moved body\n");
    assert!(!dir.path().join("src.txt").exists());
}

#[tokio::test]
async fn apply_patch_deletes_a_file() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("doomed.txt"), "x\n").unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(
            json!({ "input": "*** Begin Patch\n*** Delete File: doomed.txt\n*** End Patch\n" }),
            &config,
            &session,
        )
        .await;

    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    assert!(r.joined_text().contains("D doomed.txt"));
    assert!(!dir.path().join("doomed.txt").exists());
}

#[tokio::test]
async fn apply_patch_writes_nothing_when_a_later_hunk_fails() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("first.txt"), "keep\n").unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(
            json!({ "input":
                "*** Begin Patch\n*** Update File: first.txt\n@@\n-keep\n+changed\n\
                 *** Update File: second.txt\n@@\n-nope\n+never\n*** End Patch\n" }),
            &config,
            &session,
        )
        .await;

    assert!(r.is_error);
    // second.txt does not exist, so planning fails before any write happens.
    assert!(r.joined_text().contains("Patch does not apply"));
    // The first (valid) hunk must not have been written.
    let first = std::fs::read_to_string(dir.path().join("first.txt")).unwrap();
    assert_eq!(first, "keep\n");
}

#[tokio::test]
async fn apply_patch_reports_partial_application_after_a_filesystem_failure() {
    let dir = tempfile::tempdir().unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let patch = "*** Begin Patch\n*** Add File: blocker\n+created before failure\n*** Add File: blocker/child.txt\n+cannot create beneath a file\n*** End Patch";

    let result = ApplyPatch
        .call(json!({ "input": patch }), &config, &session)
        .await;

    assert!(result.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("blocker")).unwrap(),
        "created before failure\n"
    );
    let message = result.joined_text();
    assert!(message.contains("Completed before failure:\nA blocker"));
    assert!(message.contains("failing operation may also have modified its target"));
}

#[tokio::test]
async fn apply_patch_rechecks_context_after_an_earlier_move() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.txt"), "source\n").unwrap();
    std::fs::write(dir.path().join("destination.txt"), "destination\n").unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let patch = "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination.txt\n@@\n-source\n+moved\n*** Update File: destination.txt\n@@\n-destination\n+updated\n*** End Patch";

    let result = ApplyPatch
        .call(json!({ "input": patch }), &config, &session)
        .await;

    assert!(result.is_error);
    assert!(!dir.path().join("source.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("destination.txt")).unwrap(),
        "moved\n"
    );
    assert!(
        result
            .joined_text()
            .contains("Completed before failure:\nR source.txt -> destination.txt")
    );
}

#[tokio::test]
async fn apply_patch_rejects_a_malformed_patch() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(json!({ "input": "just some text" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("Invalid patch"));
}

#[tokio::test]
async fn apply_patch_rejects_a_path_outside_the_work_directory() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ApplyPatch
        .call(
            json!({ "input": "*** Begin Patch\n*** Add File: ../escape.txt\n+bad\n*** End Patch\n" }),
            &config,
            &session,
        )
        .await;

    assert!(r.is_error);
    assert!(
        r.joined_text()
            .contains("Path must be within work directory")
    );
}

// --- view_image -------------------------------------------------------------

/// Raw bytes of the smallest valid 1x1 PNG (decoded from PNG_BASE64 below).
const PNG_BYTES: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xcf, 0xc0, 0x50,
    0x0f, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xa9, 0x8c, 0x21, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Canonical STANDARD base64 of PNG_BYTES - what view_image should emit.
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn returned_png_dimensions(result: &codexify::types::ToolResult) -> (u32, u32) {
    let ToolContent::Image { data, mime_type } = &result.content[0] else {
        panic!("expected image content block, got {:?}", result.content[0]);
    };
    assert_eq!(mime_type, "image/png");
    let bytes = STANDARD.decode(data).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    (
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    )
}

#[tokio::test]
async fn view_image_returns_an_image_content_block() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("pixel.png"), PNG_BYTES).unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ViewImage
        .call(json!({ "path": "pixel.png" }), &config, &session)
        .await;

    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    match &r.content[0] {
        ToolContent::Image { data, mime_type } => {
            assert_eq!(mime_type, "image/png");
            assert_eq!(data, PNG_BASE64);
        }
        other => panic!("expected image content block, got {other:?}"),
    }
}

#[test]
fn view_image_exposes_only_codex_high_and_original_detail_values() {
    let schema = ViewImage.input_schema();
    assert_eq!(
        schema["properties"]["detail"]["enum"],
        json!(["high", "original"])
    );
}

#[tokio::test]
async fn view_image_rejects_low_detail_like_codex() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("pixel.png"), PNG_BYTES).unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ViewImage
        .call(
            json!({ "path": "pixel.png", "detail": "low" }),
            &config,
            &session,
        )
        .await;

    assert!(r.is_error);
    assert!(
        r.joined_text()
            .contains("only supports `high` or `original`")
    );
}

#[tokio::test]
async fn view_image_high_resizes_while_original_preserves_resolution() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("wide.png"), png_bytes(3000, 100)).unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let high = ViewImage
        .call(json!({ "path": "wide.png" }), &config, &session)
        .await;
    assert!(!high.is_error, "unexpected error: {}", high.joined_text());
    assert_eq!(returned_png_dimensions(&high), (2048, 68));

    let original = ViewImage
        .call(
            json!({ "path": "wide.png", "detail": "original" }),
            &config,
            &session,
        )
        .await;
    assert!(
        !original.is_error,
        "unexpected error: {}",
        original.joined_text()
    );
    assert_eq!(returned_png_dimensions(&original), (3000, 100));
}

#[tokio::test]
async fn view_image_rejects_a_non_image_file() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("notimage.png"), "plain text").unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ViewImage
        .call(json!({ "path": "notimage.png" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("Not a recognised image file"));
}

#[tokio::test]
async fn view_image_reports_a_missing_file() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ViewImage
        .call(json!({ "path": "absent.png" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("File not found"));
}

#[tokio::test]
async fn view_image_rejects_path_traversal() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ViewImage
        .call(json!({ "path": "../../secret.png" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(
        r.joined_text()
            .contains("Path must be within work directory")
    );
}

// --- clock tools ------------------------------------------------------------

#[tokio::test]
async fn clock_curr_time_returns_a_utc_timestamp() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ClockCurrTime.call(json!({}), &config, &session).await;

    assert!(!r.is_error);
    let text = r.joined_text();
    // YYYY-MM-DD HH:MM:SS UTC
    let bytes = text.as_bytes();
    assert_eq!(text.len(), 23, "unexpected timestamp shape: {text:?}");
    assert!(text.ends_with(" UTC"));
    assert_eq!(bytes[4], b'-');
    assert_eq!(bytes[7], b'-');
    assert_eq!(bytes[10], b' ');
    assert_eq!(bytes[13], b':');
    assert_eq!(bytes[16], b':');
    assert!(text[0..4].chars().all(|c| c.is_ascii_digit()));
    // Its output schema requires `current_time`, so the structured content is the
    // typed form, not the generic `{ content }` fallback.
    assert_eq!(r.structured_content, Some(json!({ "current_time": text })));
}

#[tokio::test]
async fn clock_sleep_waits_and_reports_elapsed_time() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let started = std::time::Instant::now();
    let r = ClockSleep
        .call(json!({ "duration_ms": 60 }), &config, &session)
        .await;
    assert!(started.elapsed().as_millis() >= 50);
    assert!(!r.is_error);
    let text = r.joined_text();
    assert!(text.contains("Sleep completed."));
    assert!(text.contains("Wall time:"));
}

#[tokio::test]
async fn clock_sleep_rejects_a_duration_outside_the_supported_range() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let too_long = ClockSleep
        .call(json!({ "duration_ms": 60 * 60 * 1000 }), &config, &session)
        .await;
    assert!(too_long.is_error);
    assert!(
        too_long
            .joined_text()
            .contains("duration_ms must be between")
    );

    let too_short = ClockSleep
        .call(json!({ "duration_ms": 0 }), &config, &session)
        .await;
    assert!(too_short.is_error);
}

#[tokio::test]
async fn clock_sleep_rejects_a_non_numeric_duration() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ClockSleep
        .call(json!({ "duration_ms": "60" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("Invalid tool arguments"));
}

#[tokio::test]
async fn clock_sleep_rejects_fractional_milliseconds_like_codex() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ClockSleep
        .call(json!({ "duration_ms": 60.5 }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("Invalid tool arguments"));
}

#[tokio::test]
async fn clock_sleep_ends_early_when_the_mcp_request_is_cancelled() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();
    let cancellation = CancellationToken::new();
    let context = ToolRequestContext {
        conversation: None,
        conversation_authorizations: Arc::new(ConversationAuthorizationStore::new()),
        project_bindings: Arc::new(codexify::project_bindings::ProjectBindingStore::new(
            dir.path().join("project-bindings"),
        )),
        diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
        artifact_egress: Arc::new(ArtifactEgressStore::new_at(
            ArtifactEgressConfig::default(),
            dir.path().join("artifact-state"),
        )),
        cancellation: cancellation.clone(),
    };

    let future = ClockSleep.call_with_context(
        json!({ "duration_ms": 60_000 }),
        &config,
        &session,
        &context,
    );
    tokio::pin!(future);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    cancellation.cancel();

    let r = tokio::time::timeout(std::time::Duration::from_secs(1), &mut future)
        .await
        .expect("cancelled sleep should finish promptly");
    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    assert!(r.joined_text().contains("Sleep interrupted by new input."));
}

// --- update_plan ------------------------------------------------------------

#[tokio::test]
async fn update_plan_stores_and_renders_the_plan() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = UpdatePlan
        .call(
            json!({
                "explanation": "Getting started",
                "plan": [
                    { "step": "Read the code", "status": "completed" },
                    { "step": "Write the fix", "status": "in_progress" },
                    { "step": "Run tests", "status": "pending" }
                ]
            }),
            &config,
            &session,
        )
        .await;

    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    let text = r.joined_text();
    assert!(text.contains("Getting started"));
    assert!(text.contains("[x] Read the code"));
    assert!(text.contains("[~] Write the fix"));
    assert!(text.contains("[ ] Run tests"));
    assert!(text.contains("1/3 steps completed"));

    let plan = session.plan.lock().unwrap();
    assert_eq!(plan.as_ref().map(|p| p.plan.len()), Some(3));
}

#[tokio::test]
async fn update_plan_rejects_more_than_one_in_progress_step() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = UpdatePlan
        .call(
            json!({
                "plan": [
                    { "step": "a", "status": "in_progress" },
                    { "step": "b", "status": "in_progress" }
                ]
            }),
            &config,
            &session,
        )
        .await;

    assert!(r.is_error);
    assert!(
        r.joined_text()
            .contains("At most one step can be in_progress")
    );
}

#[tokio::test]
async fn update_plan_rejects_an_unknown_status() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = UpdatePlan
        .call(
            json!({ "plan": [{ "step": "a", "status": "doing" }] }),
            &config,
            &session,
        )
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("Invalid tool arguments"));
}

#[tokio::test]
async fn update_plan_rejects_a_plan_that_is_not_an_array() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = UpdatePlan
        .call(json!({ "plan": "nope" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("Invalid tool arguments"));
}

// --- read_file / write_file round trip --------------------------------------
// Not covered by the two TS files, but these modules are under test in this
// batch, so a basic round trip guards their user-facing behavior.

#[tokio::test]
async fn write_file_then_read_file_round_trip() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let content = "hello\nworld\n";
    let w = WriteFile
        .call(
            json!({ "path": "sub/greeting.txt", "content": content }),
            &config,
            &session,
        )
        .await;
    assert!(!w.is_error, "unexpected error: {}", w.joined_text());
    // Byte count is the UTF-8 byte length of the content (12 bytes here).
    assert!(
        w.joined_text()
            .contains(&format!("Written {} bytes", content.len()))
    );
    assert!(w.joined_text().contains("sub/greeting.txt"));

    let r = ReadFile
        .call(json!({ "path": "sub/greeting.txt" }), &config, &session)
        .await;
    assert!(!r.is_error, "unexpected error: {}", r.joined_text());
    let text = r.joined_text();
    // Output is line-number prefixed with tabs.
    assert!(text.contains("1\thello"));
    assert!(text.contains("2\tworld"));
}

#[tokio::test]
async fn read_file_reports_a_missing_file() {
    let dir = TempDir::new().unwrap();
    let config = config_in(&dir);
    let session = SessionState::new();

    let r = ReadFile
        .call(json!({ "path": "nope.txt" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(r.joined_text().contains("File not found"));
}

// --- Intentionally NOT ported -----------------------------------------------
// The TS `exec_command` and `write_stdin` describe blocks are not ported in this
// file: those tools (`exec_command`, `write_stdin`) are outside this batch's
// module set, and their tests spawn long-lived shell sessions with timing-based
// yields that belong with the exec-sessions batch. Keeping them out avoids
// duplicating coverage and flaky, timing-dependent process management here.
