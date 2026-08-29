use std::cell::Cell;
use std::io::{self, Write};

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::redaction::SecretRedactor;
use crate::tool::ToolCallIdentity;
use crate::types::{AppConfig, ToolContent, ToolLogLevel, ToolLogMode, ToolResult};

const TRUNCATION_MARKER: &str = "...[truncated]...";
const SERIALIZATION_FAILURE: &str = "[unserializable payload]";

macro_rules! tool_payload_event {
    ($level:expr, $($fields:tt)*) => {
        match $level {
            ToolLogLevel::Trace => tracing::event!(
                target: "codexify::tool_payload",
                tracing::Level::TRACE,
                $($fields)*
            ),
            ToolLogLevel::Debug => tracing::event!(
                target: "codexify::tool_payload",
                tracing::Level::DEBUG,
                $($fields)*
            ),
            ToolLogLevel::Info => tracing::event!(
                target: "codexify::tool_payload",
                tracing::Level::INFO,
                $($fields)*
            ),
            ToolLogLevel::Warn => tracing::event!(
                target: "codexify::tool_payload",
                tracing::Level::WARN,
                $($fields)*
            ),
            ToolLogLevel::Error => tracing::event!(
                target: "codexify::tool_payload",
                tracing::Level::ERROR,
                $($fields)*
            ),
        }
    };
}

pub(crate) struct ToolCallLogger {
    mode: ToolLogMode,
    level: ToolLogLevel,
    max_request_bytes: usize,
    max_response_bytes: usize,
    redactor: SecretRedactor,
}

pub(crate) struct ToolLogCall {
    id: u64,
    enabled: bool,
}

impl ToolLogCall {
    pub(crate) fn events_enabled(&self) -> bool {
        self.enabled
    }
}

struct PayloadPreview {
    text: String,
    observed_bytes: usize,
    bytes_exact: bool,
    omitted_bytes: Option<usize>,
    truncated: bool,
    serialization_failed: bool,
}

impl ToolCallLogger {
    pub(crate) fn new(config: &AppConfig) -> Option<Self> {
        let mode = config.tool_logging.mode;
        if mode == ToolLogMode::Off {
            return None;
        }
        Some(Self {
            mode,
            level: config.tool_logging.level,
            max_request_bytes: config.tool_logging.max_request_bytes,
            max_response_bytes: config.tool_logging.max_response_bytes,
            redactor: SecretRedactor::for_tool_logging(config),
        })
    }

    pub(crate) fn mode(&self) -> ToolLogMode {
        self.mode
    }

    pub(crate) fn logs_requests(&self) -> bool {
        self.mode.logs_requests() && self.events_enabled()
    }

    pub(crate) fn level(&self) -> ToolLogLevel {
        self.level
    }

    pub(crate) fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }

    pub(crate) fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    pub(crate) fn begin(
        &self,
        call_id: u64,
        identity: &ToolCallIdentity,
        arguments: &Value,
        input_schema: Option<&Value>,
    ) -> ToolLogCall {
        let enabled = self.events_enabled();
        let call = ToolLogCall {
            id: call_id,
            enabled,
        };
        if !enabled {
            return call;
        }
        let resolved_tool = identity.resolved_tool();
        if self.mode.logs_requests() {
            let preview = self.preview_request(arguments, input_schema);
            tool_payload_event!(
                self.level,
                call_id = call.id,
                phase = "start",
                tool = %identity.downstream_tool,
                resolved_tool = %resolved_tool,
                mcp_server = identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = identity.mcp_tool.as_deref().unwrap_or("-"),
                status = "started",
                request_bytes = preview.observed_bytes,
                request_bytes_exact = preview.bytes_exact,
                request_omitted_bytes = preview.omitted_bytes.unwrap_or(0),
                request_omitted_bytes_exact = preview.omitted_bytes.is_some(),
                request_truncated = preview.truncated,
                request_serialization_failed = preview.serialization_failed,
                request = %preview.text,
                "tool invocation started"
            );
        } else {
            tool_payload_event!(
                self.level,
                call_id = call.id,
                phase = "start",
                tool = %identity.downstream_tool,
                resolved_tool = %resolved_tool,
                mcp_server = identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = identity.mcp_tool.as_deref().unwrap_or("-"),
                status = "started",
                "tool invocation started"
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
        if !call.enabled {
            return;
        }
        let resolved_tool = identity.resolved_tool();
        if self.mode.logs_responses() {
            let preview = self.preview_response(result);
            tool_payload_event!(
                self.level,
                call_id = call.id,
                phase = "finish",
                tool = %identity.downstream_tool,
                resolved_tool = %resolved_tool,
                mcp_server = identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = identity.mcp_tool.as_deref().unwrap_or("-"),
                status = if result.is_error { "error" } else { "ok" },
                duration_ms,
                response_bytes = preview.observed_bytes,
                response_bytes_exact = preview.bytes_exact,
                response_omitted_bytes = preview.omitted_bytes.unwrap_or(0),
                response_omitted_bytes_exact = preview.omitted_bytes.is_some(),
                response_truncated = preview.truncated,
                response_serialization_failed = preview.serialization_failed,
                response = %preview.text,
                "tool invocation completed"
            );
        } else {
            tool_payload_event!(
                self.level,
                call_id = call.id,
                phase = "finish",
                tool = %identity.downstream_tool,
                resolved_tool = %resolved_tool,
                mcp_server = identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = identity.mcp_tool.as_deref().unwrap_or("-"),
                status = if result.is_error { "error" } else { "ok" },
                duration_ms,
                "tool invocation completed"
            );
        }
    }

    fn preview_request(&self, value: &Value, schema: Option<&Value>) -> PayloadPreview {
        let traversal_truncated = Cell::new(false);
        let redacted = self.redactor.redacted_json(
            value,
            schema,
            self.max_request_bytes,
            &traversal_truncated,
        );
        preview_serializable(&redacted, self.max_request_bytes, &traversal_truncated)
    }

    fn preview_response(&self, result: &ToolResult) -> PayloadPreview {
        let traversal_truncated = Cell::new(false);
        let response = LoggableToolResult {
            result,
            redactor: &self.redactor,
            max_payload_bytes: self.max_response_bytes,
            traversal_truncated: &traversal_truncated,
        };
        preview_serializable(&response, self.max_response_bytes, &traversal_truncated)
    }

    fn events_enabled(&self) -> bool {
        match self.level {
            ToolLogLevel::Trace => tracing::event_enabled!(
                target: "codexify::tool_payload",
                tracing::Level::TRACE
            ),
            ToolLogLevel::Debug => tracing::event_enabled!(
                target: "codexify::tool_payload",
                tracing::Level::DEBUG
            ),
            ToolLogLevel::Info => tracing::event_enabled!(
                target: "codexify::tool_payload",
                tracing::Level::INFO
            ),
            ToolLogLevel::Warn => tracing::event_enabled!(
                target: "codexify::tool_payload",
                tracing::Level::WARN
            ),
            ToolLogLevel::Error => tracing::event_enabled!(
                target: "codexify::tool_payload",
                tracing::Level::ERROR
            ),
        }
    }
}

struct LoggableToolResult<'a> {
    result: &'a ToolResult,
    redactor: &'a SecretRedactor,
    max_payload_bytes: usize,
    traversal_truncated: &'a Cell<bool>,
}

impl Serialize for LoggableToolResult<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry(
            "content",
            &LoggableContentSequence {
                content: &self.result.content,
                redactor: self.redactor,
                max_payload_bytes: self.max_payload_bytes,
                traversal_truncated: self.traversal_truncated,
            },
        )?;
        map.serialize_entry("isError", &self.result.is_error)?;
        if let Some(structured) = self.result.structured_content.as_ref() {
            map.serialize_entry(
                "structuredContent",
                &self.redactor.redacted_json(
                    structured,
                    None,
                    self.max_payload_bytes,
                    self.traversal_truncated,
                ),
            )?;
        }
        if let Some(meta) = self.result.meta.as_ref() {
            map.serialize_entry(
                "_meta",
                &self.redactor.redacted_json_map(
                    &meta.0,
                    self.max_payload_bytes,
                    self.traversal_truncated,
                ),
            )?;
        }
        map.end()
    }
}

struct LoggableContentSequence<'a> {
    content: &'a [ToolContent],
    redactor: &'a SecretRedactor,
    max_payload_bytes: usize,
    traversal_truncated: &'a Cell<bool>,
}

impl Serialize for LoggableContentSequence<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.content.len()))?;
        for content in self.content {
            sequence.serialize_element(&LoggableContent {
                content,
                redactor: self.redactor,
                max_payload_bytes: self.max_payload_bytes,
                traversal_truncated: self.traversal_truncated,
            })?;
        }
        sequence.end()
    }
}

struct LoggableContent<'a> {
    content: &'a ToolContent,
    redactor: &'a SecretRedactor,
    max_payload_bytes: usize,
    traversal_truncated: &'a Cell<bool>,
}

impl Serialize for LoggableContent<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        match self.content {
            ToolContent::Text(text) => {
                map.serialize_entry("type", "text")?;
                map.serialize_entry(
                    "text",
                    &self.redactor.redacted_text(
                        text,
                        self.max_payload_bytes,
                        self.traversal_truncated,
                    ),
                )?;
            }
            ToolContent::Image { data, mime_type } => {
                map.serialize_entry("type", "image")?;
                map.serialize_entry(
                    "mimeType",
                    &self.redactor.redacted_text(
                        mime_type,
                        self.max_payload_bytes,
                        self.traversal_truncated,
                    ),
                )?;
                map.serialize_entry("base64Bytes", &data.len())?;
                map.serialize_entry("data", "[omitted from tool logs]")?;
            }
            ToolContent::ResourceLink(resource) => {
                map.serialize_entry("type", "resource_link")?;
                map.serialize_entry(
                    "name",
                    &self.redactor.redacted_text(
                        &resource.name,
                        self.max_payload_bytes,
                        self.traversal_truncated,
                    ),
                )?;
                if let Some(title) = resource.title.as_deref() {
                    map.serialize_entry(
                        "title",
                        &self.redactor.redacted_text(
                            title,
                            self.max_payload_bytes,
                            self.traversal_truncated,
                        ),
                    )?;
                }
                if let Some(description) = resource.description.as_deref() {
                    map.serialize_entry(
                        "description",
                        &self.redactor.redacted_text(
                            description,
                            self.max_payload_bytes,
                            self.traversal_truncated,
                        ),
                    )?;
                }
                if let Some(mime_type) = resource.mime_type.as_deref() {
                    map.serialize_entry(
                        "mimeType",
                        &self.redactor.redacted_text(
                            mime_type,
                            self.max_payload_bytes,
                            self.traversal_truncated,
                        ),
                    )?;
                }
                if let Some(size) = resource.size {
                    map.serialize_entry("size", &size)?;
                }
                map.serialize_entry("uriBytes", &resource.uri.len())?;
                map.serialize_entry("uri", "[omitted from tool logs]")?;
            }
        }
        map.end()
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    limit_reached: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit),
            limit,
            limit_reached: false,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() <= remaining {
            self.bytes.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        self.bytes.extend_from_slice(&bytes[..remaining]);
        self.limit_reached = true;
        Err(io::Error::other("tool log payload limit reached"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn preview_serializable<T>(
    value: &T,
    max_bytes: usize,
    traversal_truncated: &Cell<bool>,
) -> PayloadPreview
where
    T: Serialize + ?Sized,
{
    let mut writer = BoundedWriter::new(max_bytes.saturating_add(1));
    let serialized = serde_json::to_writer(&mut writer, value);
    if serialized.is_err() && !writer.limit_reached {
        return PayloadPreview {
            text: fit_utf8_prefix(SERIALIZATION_FAILURE, max_bytes),
            observed_bytes: 0,
            bytes_exact: false,
            omitted_bytes: None,
            truncated: true,
            serialization_failed: true,
        };
    }

    let traversal_was_truncated = traversal_truncated.get();
    let output_was_truncated = writer.limit_reached || writer.bytes.len() > max_bytes;
    let bytes_exact = serialized.is_ok() && !traversal_was_truncated;
    if !output_was_truncated {
        let observed_bytes = writer.bytes.len();
        return PayloadPreview {
            text: String::from_utf8(writer.bytes)
                .unwrap_or_else(|_| SERIALIZATION_FAILURE.to_string()),
            observed_bytes,
            bytes_exact,
            omitted_bytes: (!traversal_was_truncated).then_some(0),
            truncated: traversal_was_truncated,
            serialization_failed: false,
        };
    }

    let observed_bytes = writer.bytes.len();
    let (text, retained_source_bytes) = prefix_with_marker(&writer.bytes, max_bytes);
    let omitted_bytes = bytes_exact.then(|| observed_bytes.saturating_sub(retained_source_bytes));
    PayloadPreview {
        text,
        observed_bytes,
        bytes_exact,
        omitted_bytes,
        truncated: true,
        serialization_failed: false,
    }
}

fn prefix_with_marker(value: &[u8], max_bytes: usize) -> (String, usize) {
    if max_bytes <= TRUNCATION_MARKER.len() {
        return (TRUNCATION_MARKER[..max_bytes].to_string(), 0);
    }
    let target = max_bytes - TRUNCATION_MARKER.len();
    let retained = utf8_prefix_len(value, target);
    let prefix = std::str::from_utf8(&value[..retained]).unwrap_or_default();
    (format!("{prefix}{TRUNCATION_MARKER}"), retained)
}

fn fit_utf8_prefix(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn utf8_prefix_len(value: &[u8], max_bytes: usize) -> usize {
    let mut end = max_bytes.min(value.len());
    while std::str::from_utf8(&value[..end]).is_err() {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};

    use serde::ser::Error as _;
    use serde_json::json;
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
            fields.insert("level".to_string(), event.metadata().level().to_string());
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
    fn exact_boundary_is_complete_and_over_boundary_is_unicode_safe() {
        let traversal = Cell::new(false);
        let exact = preview_serializable(&"a".repeat(62), 64, &traversal);
        assert_eq!(exact.text.len(), 64);
        assert!(!exact.truncated);
        assert!(exact.bytes_exact);
        assert_eq!(exact.omitted_bytes, Some(0));

        let traversal = Cell::new(false);
        let one_byte_over = preview_serializable(&"a".repeat(63), 64, &traversal);
        assert_eq!(one_byte_over.observed_bytes, 65);
        assert!(one_byte_over.bytes_exact);
        assert!(one_byte_over.truncated);
        assert_eq!(one_byte_over.text.len(), 64);
        assert!(one_byte_over.text.ends_with(TRUNCATION_MARKER));
        assert_eq!(one_byte_over.omitted_bytes, Some(18));

        let traversal = Cell::new(false);
        let unicode = preview_serializable(&format!("start-{}", "é".repeat(100)), 41, &traversal);
        assert!(unicode.truncated);
        assert!(unicode.text.len() <= 41);
        assert!(unicode.text.starts_with("\"start-"));
        assert!(unicode.text.ends_with(TRUNCATION_MARKER));
        assert!(std::str::from_utf8(unicode.text.as_bytes()).is_ok());
    }

    struct CountingSequence<'a> {
        visits: &'a AtomicUsize,
    }

    impl Serialize for CountingSequence<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut sequence = serializer.serialize_seq(Some(1_000_000))?;
            for _ in 0..1_000_000 {
                self.visits.fetch_add(1, AtomicOrdering::Relaxed);
                sequence.serialize_element("0123456789")?;
            }
            sequence.end()
        }
    }

    #[test]
    fn serializer_stops_traversing_when_the_preview_is_full() {
        let visits = AtomicUsize::new(0);
        let traversal = Cell::new(false);
        let preview = preview_serializable(&CountingSequence { visits: &visits }, 64, &traversal);

        assert!(preview.truncated);
        assert!(preview.text.len() <= 64);
        assert!(visits.load(AtomicOrdering::Relaxed) < 10);
    }

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("fixture serializer failure"))
        }
    }

    #[test]
    fn serialization_failure_is_bounded_and_non_panicking() {
        let traversal = Cell::new(false);
        let preview = preview_serializable(&FailingSerialize, 64, &traversal);

        assert_eq!(preview.text, SERIALIZATION_FAILURE);
        assert!(preview.serialization_failed);
        assert!(preview.truncated);
        assert!(!preview.bytes_exact);
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

        let preview = logger.preview_response(&result);
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

        let preview = logger.preview_response(&result);
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
    fn request_and_response_toggles_keep_paired_lifecycle_events() {
        for (mode, request_present, response_present) in [
            (ToolLogMode::Requests, true, false),
            (ToolLogMode::Responses, false, true),
            (ToolLogMode::All, true, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut config = default_config(root.path().to_path_buf());
            config.tool_logging.mode = mode;
            let logger = ToolCallLogger::new(&config).unwrap();
            let capture = CaptureLayer::default();
            let events = capture.events.clone();
            let subscriber = Registry::default().with(capture);

            tracing::subscriber::with_default(subscriber, || {
                let identity = ToolCallIdentity::native("fixture");
                let call = logger.begin(1, &identity, &json!({ "value": 7 }), None);
                logger.finish(&call, &identity, &ToolResult::text("answer"), 3);
            });

            let events = events.lock().unwrap();
            assert_eq!(events.len(), 2, "mode={mode:?}");
            assert_eq!(events[0]["call_id"], events[1]["call_id"]);
            assert_eq!(events[0]["phase"], "start");
            assert_eq!(events[1]["phase"], "finish");
            assert_eq!(events[0].contains_key("request"), request_present);
            assert_eq!(events[1].contains_key("response"), response_present);
        }
    }

    #[test]
    fn disabled_mode_constructs_no_payload_logger() {
        let root = tempfile::tempdir().unwrap();
        let config = default_config(root.path().to_path_buf());
        assert!(ToolCallLogger::new(&config).is_none());
    }

    #[test]
    fn emitted_events_pair_payloads_with_resolved_mcp_identity_and_level() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::All;
        config.tool_logging.level = ToolLogLevel::Warn;
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
                1,
                &identity,
                &json!({ "arguments": { "token": "known-emitted-secret", "address": "0x81000000" } }),
                None,
            );
            logger.finish(&call, &identity, &ToolResult::text("decompiled output"), 17);
        });

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["target"], "codexify::tool_payload");
        assert_eq!(events[0]["level"], "WARN");
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
        assert_eq!(events[1]["status"], "ok");
    }

    #[test]
    fn direct_gateway_and_catalog_modes_log_the_upstream_target() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::All;
        let logger = ToolCallLogger::new(&config).unwrap();
        let cases = [
            ToolCallIdentity::mcp(
                "IDA_MCP___decompile_function",
                "IDA MCP!",
                Some("decompile_function".to_string()),
            ),
            ToolCallIdentity::mcp("IDA_MCP_", "IDA MCP!", Some("rename-function".to_string())),
            ToolCallIdentity::mcp("mcp_call_tool", "IDA MCP!", Some("set_comment".to_string())),
        ];
        let capture = CaptureLayer::default();
        let events = capture.events.clone();
        let subscriber = Registry::default().with(capture);

        tracing::subscriber::with_default(subscriber, || {
            for (index, identity) in cases.iter().enumerate() {
                let call = logger.begin(
                    index as u64 + 1,
                    identity,
                    &json!({ "address": "0x81000000" }),
                    None,
                );
                logger.finish(&call, identity, &ToolResult::text("upstream answer"), 1);
            }
        });

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 6);
        for (pair, identity) in events.as_chunks::<2>().0.iter().zip(cases) {
            assert_eq!(pair[0]["tool"], identity.downstream_tool);
            assert_eq!(pair[0]["resolved_tool"], identity.resolved_tool());
            assert_eq!(pair[0]["mcp_server"], "IDA MCP!");
            assert_eq!(pair[0]["mcp_tool"], identity.mcp_tool.unwrap());
            assert_eq!(pair[0]["resolved_tool"], pair[1]["resolved_tool"]);
        }
    }

    #[test]
    fn error_logging_does_not_change_the_tool_result() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::All;
        let logger = ToolCallLogger::new(&config).unwrap();
        let identity = ToolCallIdentity::native("failing_fixture");
        let result = ToolResult::error("original transport failure");
        let before = result.joined_text();
        let capture = CaptureLayer::default();
        let events = capture.events.clone();
        let subscriber = Registry::default().with(capture);

        tracing::subscriber::with_default(subscriber, || {
            let call = logger.begin(1, &identity, &Value::Null, None);
            logger.finish(&call, &identity, &result, 9);
        });

        assert!(result.is_error);
        assert_eq!(result.joined_text(), before);
        let events = events.lock().unwrap();
        assert_eq!(events[1]["status"], "error");
        assert!(events[1]["response"].contains("original transport failure"));
    }

    #[test]
    fn concurrent_invocations_have_unique_paired_call_ids() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.tool_logging.mode = ToolLogMode::All;
        let logger = Arc::new(ToolCallLogger::new(&config).unwrap());
        let next_call_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let capture = CaptureLayer::default();
        let events = capture.events.clone();

        std::thread::scope(|scope| {
            for value in 0..8 {
                let logger = logger.clone();
                let next_call_id = next_call_id.clone();
                let capture = capture.clone();
                scope.spawn(move || {
                    let subscriber = Registry::default().with(capture);
                    tracing::subscriber::with_default(subscriber, || {
                        let identity = ToolCallIdentity::native("concurrent_fixture");
                        let call_id = next_call_id.fetch_add(1, AtomicOrdering::Relaxed);
                        let call =
                            logger.begin(call_id, &identity, &json!({ "value": value }), None);
                        logger.finish(
                            &call,
                            &identity,
                            &ToolResult::text(format!("answer-{value}")),
                            value,
                        );
                    });
                });
            }
        });

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 16);
        let mut counts = HashMap::<String, usize>::new();
        for event in events.iter() {
            *counts.entry(event["call_id"].clone()).or_default() += 1;
        }
        assert_eq!(counts.len(), 8);
        assert!(counts.values().all(|count| *count == 2));
    }
}
