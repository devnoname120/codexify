use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use crate::redaction::SecretRedactor;
use crate::tool::ToolCallIdentity;
use crate::types::{AppConfig, ToolContent, ToolLogMode, ToolResult};

pub(crate) struct ToolCallLogger {
    mode: ToolLogMode,
    max_request_bytes: usize,
    max_response_bytes: usize,
    redactor: SecretRedactor,
    next_call_id: AtomicU64,
}

pub(crate) struct ToolLogCall {
    id: u64,
}

struct PayloadPreview {
    text: String,
    source_bytes: usize,
    truncated: bool,
}

impl ToolCallLogger {
    pub(crate) fn new(config: &AppConfig) -> Option<Self> {
        let mode = config.tool_logging.mode;
        if mode == ToolLogMode::Off {
            return None;
        }
        Some(Self {
            mode,
            max_request_bytes: config.tool_logging.max_request_bytes,
            max_response_bytes: config.tool_logging.max_response_bytes,
            redactor: SecretRedactor::from_config(config, &config.tool_logging.redact_env),
            next_call_id: AtomicU64::new(1),
        })
    }

    pub(crate) fn mode(&self) -> ToolLogMode {
        self.mode
    }

    pub(crate) fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }

    pub(crate) fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    pub(crate) fn begin(&self, identity: &ToolCallIdentity, arguments: &Value) -> ToolLogCall {
        let call = ToolLogCall {
            id: self.next_call_id.fetch_add(1, Ordering::Relaxed),
        };
        if self.mode.logs_requests() {
            let preview = self.preview_value(arguments, self.max_request_bytes);
            let resolved_tool = identity.resolved_tool();
            tracing::info!(
                target: "codexify::tool_payload",
                call_id = call.id,
                tool = %identity.downstream_tool,
                resolved_tool = %resolved_tool,
                mcp_server = identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = identity.mcp_tool.as_deref().unwrap_or("-"),
                request_bytes = preview.source_bytes,
                request_truncated = preview.truncated,
                request = %preview.text,
                "tool request"
            );
        }
        call
    }

    pub(crate) fn finish(
        &self,
        call: &ToolLogCall,
        identity: &ToolCallIdentity,
        result: &ToolResult,
        duration_ms: u64,
    ) {
        if !self.mode.logs_responses() {
            return;
        }
        let response = tool_result_value(result);
        let preview = self.preview_value(&response, self.max_response_bytes);
        let resolved_tool = identity.resolved_tool();
        tracing::info!(
            target: "codexify::tool_payload",
            call_id = call.id,
            tool = %identity.downstream_tool,
            resolved_tool = %resolved_tool,
            mcp_server = identity.mcp_server.as_deref().unwrap_or("-"),
            mcp_tool = identity.mcp_tool.as_deref().unwrap_or("-"),
            status = if result.is_error { "error" } else { "ok" },
            duration_ms,
            response_bytes = preview.source_bytes,
            response_truncated = preview.truncated,
            response = %preview.text,
            "tool response"
        );
    }

    fn preview_value(&self, value: &Value, max_bytes: usize) -> PayloadPreview {
        let redacted = self.redactor.redact_json(value);
        let serialized = serde_json::to_string(&redacted)
            .unwrap_or_else(|_| "\"[unserializable payload]\"".to_string());
        let source_bytes = serialized.len();
        let (text, truncated) = bound_utf8_head_tail(&serialized, max_bytes);
        PayloadPreview {
            text,
            source_bytes,
            truncated,
        }
    }
}

fn tool_result_value(result: &ToolResult) -> Value {
    let content = result
        .content
        .iter()
        .map(|content| match content {
            ToolContent::Text(text) => json!({
                "type": "text",
                "text": text,
            }),
            ToolContent::Image { data, mime_type } => json!({
                "type": "image",
                "mimeType": mime_type,
                "base64Bytes": data.len(),
                "data": "[omitted from tool logs]",
            }),
            ToolContent::ResourceLink(resource) => json!({
                "type": "resource_link",
                "name": resource.name,
                "title": resource.title,
                "description": resource.description,
                "mimeType": resource.mime_type,
                "size": resource.size,
                "uriBytes": resource.uri.len(),
                "uri": "[omitted from tool logs]",
            }),
        })
        .collect::<Vec<_>>();
    let mut response = Map::from_iter([
        ("content".to_string(), Value::Array(content)),
        ("isError".to_string(), Value::Bool(result.is_error)),
    ]);
    if let Some(structured) = result.structured_content.as_ref() {
        response.insert("structuredContent".to_string(), structured.clone());
    }
    if let Some(meta) = result.meta.as_ref() {
        response.insert(
            "_meta".to_string(),
            serde_json::to_value(meta).unwrap_or(Value::Null),
        );
    }
    Value::Object(response)
}

fn bound_utf8_head_tail(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let marker = "...[truncated]...";
    if max_bytes <= marker.len() {
        return (marker[..max_bytes].to_string(), true);
    }

    let retained = max_bytes - marker.len();
    let mut head_end = retained.div_ceil(2);
    while !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len() - retained / 2;
    while !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    (
        format!("{}{}{}", &value[..head_end], marker, &value[tail_start..]),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    use super::*;
    use crate::config::default_config;

    #[derive(Clone, Default)]
    struct CaptureLayer {
        events: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber,
    {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let mut fields = HashMap::new();
            fields.insert("target".to_string(), event.metadata().target().to_string());
            event.record(&mut FieldVisitor(&mut fields));
            self.events.lock().unwrap().push(fields);
        }
    }

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    #[test]
    fn preview_is_byte_bounded_and_keeps_both_ends() {
        let value = format!("start-{}-finish", "é".repeat(100));
        let (preview, truncated) = bound_utf8_head_tail(&value, 41);
        assert!(truncated);
        assert!(preview.len() <= 41);
        assert!(preview.starts_with("start-"));
        assert!(preview.ends_with("-finish"));
        assert!(preview.contains("[truncated]"));
    }

    #[test]
    fn response_preview_omits_images_and_redacts_payload_secrets() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::All;
        config.api_key = Some("known-tool-log-secret".to_string());
        let logger = ToolCallLogger::new(&config).unwrap();
        let result = ToolResult {
            content: vec![
                ToolContent::Text("known-tool-log-secret visible tail".to_string()),
                ToolContent::Image {
                    data: "base64-secret-data".to_string(),
                    mime_type: "image/png".to_string(),
                },
            ],
            is_error: false,
            structured_content: Some(json!({
                "accessToken": "nested-secret",
                "original_token_count": 12,
            })),
            meta: None,
            audit: Default::default(),
        };

        let preview = logger.preview_value(&tool_result_value(&result), 4096);
        assert!(!preview.text.contains("known-tool-log-secret"));
        assert!(!preview.text.contains("base64-secret-data"));
        assert!(!preview.text.contains("nested-secret"));
        assert!(preview.text.contains("base64Bytes"));
        assert!(preview.text.contains("original_token_count"));
        assert!(preview.text.contains("12"));
    }

    #[test]
    fn response_preview_omits_resource_capability_uris() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::Responses;
        let logger = ToolCallLogger::new(&config).unwrap();
        let resource = rmcp::model::Resource::new(
            "codexify://artifact/abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE",
            "report.bin",
        )
        .with_title("Report")
        .with_mime_type("application/octet-stream")
        .with_size(42);
        let result = ToolResult {
            content: vec![ToolContent::ResourceLink(resource)],
            is_error: false,
            structured_content: None,
            meta: None,
            audit: Default::default(),
        };

        let preview = logger.preview_value(&tool_result_value(&result), 4096);
        assert!(preview.text.contains("resource_link"));
        assert!(preview.text.contains("report.bin"));
        assert!(preview.text.contains("application/octet-stream"));
        assert!(preview.text.contains("\"size\":42"));
        assert!(!preview.text.contains("codexify://artifact/"));
        assert!(
            !preview
                .text
                .contains("abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE")
        );
    }

    #[test]
    fn emitted_events_pair_request_and_response_with_resolved_mcp_identity() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::All;
        config.api_key = Some("known-emitted-secret".to_string());
        let logger = ToolCallLogger::new(&config).unwrap();
        let identity = ToolCallIdentity::mcp(
            "mcp_call_tool",
            "IDA MCP!",
            Some("decompile_function".to_string()),
        );
        let capture = CaptureLayer::default();
        let events = capture.events.clone();
        let subscriber = Registry::default().with(capture);

        tracing::subscriber::with_default(subscriber, || {
            let call = logger.begin(
                &identity,
                &json!({ "arguments": { "token": "known-emitted-secret", "address": "0x81000000" } }),
            );
            logger.finish(&call, &identity, &ToolResult::text("decompiled output"), 17);
        });

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["target"], "codexify::tool_payload");
        assert_eq!(events[0]["call_id"], events[1]["call_id"]);
        assert_eq!(events[0]["tool"], "mcp_call_tool");
        assert_eq!(
            events[0]["resolved_tool"],
            "mcp:IDA MCP!/decompile_function"
        );
        assert_eq!(events[0]["mcp_server"], "IDA MCP!");
        assert_eq!(events[0]["mcp_tool"], "decompile_function");
        assert!(events[0]["request"].contains("0x81000000"));
        assert!(!events[0]["request"].contains("known-emitted-secret"));
        assert!(events[1]["response"].contains("decompiled output"));
        assert_eq!(events[1]["duration_ms"], "17");
    }
}
