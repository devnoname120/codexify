use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

/// Images are base64-inlined into the response, which roughly grows them by a
/// third. 5 MB of source keeps a single tool result within what an MCP client
/// will accept.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Sniffs the format from magic bytes rather than trusting the extension, so a
/// mislabelled file is reported instead of being sent with the wrong mime type.
fn detect_mime_type(bytes: &[u8]) -> Option<&'static str> {
    let starts_with = |sig: &[u8], offset: usize| -> bool {
        sig.iter()
            .enumerate()
            .all(|(i, &byte)| bytes.get(offset + i) == Some(&byte))
    };

    if starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 0) {
        return Some("image/png");
    }
    if starts_with(&[0xff, 0xd8, 0xff], 0) {
        return Some("image/jpeg");
    }
    if starts_with(&[0x47, 0x49, 0x46, 0x38], 0) {
        return Some("image/gif");
    }
    if starts_with(&[0x42, 0x4d], 0) {
        return Some("image/bmp");
    }
    // RIFF....WEBP
    if starts_with(&[0x52, 0x49, 0x46, 0x46], 0) && starts_with(&[0x57, 0x45, 0x42, 0x50], 8) {
        return Some("image/webp");
    }
    None
}

pub struct ViewImage;

#[async_trait]
impl Tool for ViewImage {
    fn name(&self) -> &'static str {
        "view_image"
    }

    fn description(&self) -> String {
        "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk — screenshots, diagrams, design mockups, rendered charts. Path is relative to the project root (work-dir). Supports PNG, JPEG, GIF, BMP and WebP up to 5 MB.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Image file path relative to work-dir" },
                "detail": { "type": "string", "description": "Optional detail hint (e.g. auto, low, high). Accepted for compatibility with Codex clients and currently ignored — the full image is always sent." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(input_path) = arg_str(&args, "path") else {
            return ToolResult::error("path must be a string");
        };

        let file_path = match resolve_safe_path(input_path, &config.work_dir, false) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let metadata = match tokio::fs::metadata(&file_path).await {
            Ok(m) if m.is_file() => m,
            _ => return ToolResult::error(format!("File not found: {input_path}")),
        };

        if metadata.len() > MAX_IMAGE_BYTES {
            return ToolResult::error(format!(
                "Image too large: {} bytes (limit {}). Resize it before viewing.",
                metadata.len(),
                MAX_IMAGE_BYTES
            ));
        }

        let bytes = match tokio::fs::read(&file_path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let Some(mime_type) = detect_mime_type(&bytes) else {
            return ToolResult::error(format!(
                "Not a recognised image file: {input_path}. Supported: PNG, JPEG, GIF, BMP, WebP."
            ));
        };

        ToolResult::image(STANDARD.encode(&bytes), mime_type)
    }
}
