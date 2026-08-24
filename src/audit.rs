//! Structured tool activity records that deliberately exclude raw arguments and output.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use chrono::{SecondsFormat, Utc};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::exec_sessions::approx_token_count;
use crate::project_bindings::ConversationIdentity;
use crate::types::{AppConfig, ToolContent, ToolResult};

const SCHEMA_VERSION: u64 = 1;
const HASH_HEX_CHARS: usize = 24;
const MAX_ARGUMENT_FIELDS: usize = 64;
const MAX_ARGUMENT_DEPTH: usize = 3;
const MAX_REFERENCED_SECRET_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct AuditScope {
    pub(crate) transport_session_id: u64,
    pub(crate) conversation_id: Option<String>,
    pub(crate) access_root_id: String,
    pub(crate) project_id: Option<String>,
}

impl AuditScope {
    pub(crate) fn new(
        transport_session_id: u64,
        conversation: Option<&ConversationIdentity>,
        access_root: &Path,
        project_root: Option<&Path>,
    ) -> Self {
        Self {
            transport_session_id,
            conversation_id: conversation.map(|identity| identity.audit_hash().to_string()),
            access_root_id: hash_path(access_root),
            project_id: project_root.map(hash_path),
        }
    }
}

pub(crate) struct AuditCall {
    id: u64,
}

pub(crate) struct AuditLogger {
    path: PathBuf,
    run_id: String,
    next_call_id: AtomicU64,
    file: Mutex<File>,
    include_command_preview: bool,
    command_preview_max_bytes: usize,
    secrets: Zeroizing<Vec<String>>,
    secret_patterns: Vec<(Regex, &'static str)>,
}

impl AuditLogger {
    pub(crate) fn open(config: &AppConfig) -> anyhow::Result<Option<Self>> {
        let Some(path) = config.audit.log_file.clone() else {
            return Ok(None);
        };
        let file = open_private_append(&path)?;
        let include_command_preview = config.audit.include_command_preview;
        let (secrets, secret_patterns) = if include_command_preview {
            (collect_secret_values(config), secret_patterns())
        } else {
            (Zeroizing::new(Vec::new()), Vec::new())
        };
        let logger = Self {
            path,
            run_id: random_id()?,
            next_call_id: AtomicU64::new(1),
            file: Mutex::new(file),
            include_command_preview,
            command_preview_max_bytes: config.audit.command_preview_max_bytes,
            secrets,
            secret_patterns,
        };
        logger.write_event(&json!({
            "schema_version": SCHEMA_VERSION,
            "timestamp": timestamp(),
            "event": "audit_started",
            "run_id": logger.run_id,
            "server_version": env!("CARGO_PKG_VERSION"),
            "server_process_id": std::process::id(),
            "command_previews": logger.include_command_preview,
            "command_preview_max_bytes": logger.command_preview_max_bytes,
        }))?;
        Ok(Some(logger))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn command_previews_enabled(&self) -> bool {
        self.include_command_preview
    }

    pub(crate) fn begin_tool(
        &self,
        tool: &str,
        arguments: &Value,
        input_schema: Option<&Value>,
        scope: &AuditScope,
    ) -> AuditCall {
        let call = AuditCall {
            id: self.next_call_id.fetch_add(1, Ordering::Relaxed),
        };
        let mut event = Map::from_iter([
            ("schema_version".to_string(), json!(SCHEMA_VERSION)),
            ("timestamp".to_string(), json!(timestamp())),
            ("event".to_string(), json!("tool_start")),
            ("run_id".to_string(), json!(self.run_id)),
            ("call_id".to_string(), json!(call.id)),
            (
                "transport_session_id".to_string(),
                json!(scope.transport_session_id),
            ),
            ("conversation_id".to_string(), json!(scope.conversation_id)),
            ("access_root_id".to_string(), json!(scope.access_root_id)),
            ("project_id".to_string(), json!(scope.project_id)),
            ("tool".to_string(), json!(tool)),
            (
                "argument_summary".to_string(),
                summarize_arguments(arguments, input_schema),
            ),
        ]);
        if let Some(preview) = self.command_preview(tool, arguments) {
            event.insert("command_preview".to_string(), json!(preview));
        }
        self.record(Value::Object(event));
        call
    }

    pub(crate) fn finish_tool(
        &self,
        call: &AuditCall,
        tool: &str,
        result: &ToolResult,
        duration_ms: u64,
        scope: &AuditScope,
    ) {
        self.record(json!({
            "schema_version": SCHEMA_VERSION,
            "timestamp": timestamp(),
            "event": "tool_finish",
            "run_id": self.run_id,
            "call_id": call.id,
            "transport_session_id": scope.transport_session_id,
            "conversation_id": scope.conversation_id,
            "access_root_id": scope.access_root_id,
            "project_id": scope.project_id,
            "tool": tool,
            "duration_ms": duration_ms,
            "status": if result.is_error { "error" } else { "ok" },
            "output": summarize_output(result),
        }));
    }

    fn command_preview(&self, tool: &str, arguments: &Value) -> Option<String> {
        if !self.include_command_preview {
            return None;
        }
        match tool {
            "exec_command" => {
                let raw = arguments.get("cmd")?.as_str()?.replace('\0', "\u{fffd}");
                Some(bound_utf8(
                    &self.redact_command(&raw),
                    self.command_preview_max_bytes,
                ))
            }
            "run_command" => {
                let command = arguments.get("command")?.as_str()?;
                let mut argv = vec![command.to_string()];
                if let Some(values) = arguments.get("args").and_then(Value::as_array) {
                    argv.extend(values.iter().filter_map(Value::as_str).map(str::to_string));
                }
                let redacted = self.redact_argv(&argv);
                let preview = serde_json::to_string(&redacted).ok()?;
                Some(bound_utf8(&preview, self.command_preview_max_bytes))
            }
            _ => None,
        }
    }

    fn redact_command(&self, command: &str) -> String {
        let mut redacted = command.to_string();
        for secret in self.secrets.iter() {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
        for (pattern, replacement) in &self.secret_patterns {
            redacted = pattern.replace_all(&redacted, *replacement).into_owned();
        }
        redacted
    }

    fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        let mut redact_next = false;
        argv.iter()
            .map(|argument| {
                if redact_next {
                    redact_next = false;
                    return "[REDACTED]".to_string();
                }
                redact_next = secret_flag_takes_next(argument);
                self.redact_command(&argument.replace('\0', "\u{fffd}"))
            })
            .collect()
    }

    fn record(&self, event: Value) {
        if let Err(error) = self.write_event(&event) {
            tracing::error!(
                target: "codexify::audit",
                path = %self.path.display(),
                error = %error,
                "failed to append audit event"
            );
        }
    }

    fn write_event(&self, event: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(event).context("serialize audit event")?;
        line.push(b'\n');
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("audit log lock poisoned"))?;
        file.write_all(&line)
            .with_context(|| format!("append audit event to {}", self.path.display()))?;
        file.flush()
            .with_context(|| format!("flush audit log {}", self.path.display()))?;
        Ok(())
    }
}

pub(crate) fn summarize_arguments(arguments: &Value, input_schema: Option<&Value>) -> Value {
    summarize_value(arguments, None, 0, input_schema)
}

pub(crate) fn argument_field_names(arguments: &Value, input_schema: Option<&Value>) -> String {
    let Some(object) = arguments.as_object() else {
        return value_type(arguments).to_string();
    };
    let Some(properties) = schema_properties(input_schema) else {
        return format!("{} redacted field(s)", object.len());
    };
    let mut names: Vec<&str> = object
        .keys()
        .filter(|name| properties.contains_key(*name))
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    let redacted = object.len().saturating_sub(names.len());
    if redacted > 0 {
        names.push("<additional fields redacted>");
    }
    names.join(",")
}

pub(crate) fn summarize_output(result: &ToolResult) -> Value {
    let mut text_bytes = 0u64;
    let mut text_tokens = 0u64;
    let mut image_base64_bytes = 0u64;
    let mut image_count = 0u64;
    for content in &result.content {
        match content {
            ToolContent::Text(text) => {
                text_bytes = text_bytes.saturating_add(text.len() as u64);
                text_tokens = text_tokens.saturating_add(approx_token_count(text));
            }
            ToolContent::Image { data, .. } => {
                image_count = image_count.saturating_add(1);
                image_base64_bytes = image_base64_bytes.saturating_add(data.len() as u64);
            }
        }
    }
    let structured_bytes = result
        .structured_content
        .as_ref()
        .map_or(0, serialized_json_bytes);
    json!({
        "content_bytes": text_bytes.saturating_add(image_base64_bytes),
        "text_bytes": text_bytes,
        "approx_text_tokens": text_tokens,
        "image_count": image_count,
        "image_base64_bytes": image_base64_bytes,
        "structured_bytes": structured_bytes,
        "truncated": result.audit.truncated,
        "original_output_tokens": result.audit.original_output_tokens,
        "exec_session_id": result.audit.exec_session_id,
        "process_id": result.audit.process_id,
        "resident": result.audit.resident,
    })
}

#[derive(Default)]
struct CountingWriter(u64);

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_json_bytes(value: &Value) -> u64 {
    let mut writer = CountingWriter::default();
    if serde_json::to_writer(&mut writer, value).is_ok() {
        writer.0
    } else {
        0
    }
}

fn summarize_value(
    value: &Value,
    key: Option<&str>,
    depth: usize,
    schema: Option<&Value>,
) -> Value {
    let sensitive = key.is_some_and(sensitive_key);
    if sensitive {
        return redacted_value_summary(value);
    }
    match value {
        Value::Null => json!({ "type": "null" }),
        Value::Bool(_) => json!({ "type": "boolean", "redacted": true }),
        Value::Number(_) => json!({ "type": "number", "redacted": true }),
        Value::String(value) => json!({
            "type": "string",
            "bytes": value.len(),
            "redacted": true,
        }),
        Value::Array(values) => json!({
            "type": "array",
            "items": values.len(),
            "redacted": true,
        }),
        Value::Object(values)
            if depth >= MAX_ARGUMENT_DEPTH || schema_properties(schema).is_none() =>
        {
            json!({
                "type": "object",
                "fields": values.len(),
                "redacted": true,
            })
        }
        Value::Object(values) => {
            let properties = schema_properties(schema).expect("checked above");
            let mut names: Vec<&String> = values
                .keys()
                .filter(|name| properties.contains_key(*name))
                .collect();
            names.sort_unstable();
            names.truncate(MAX_ARGUMENT_FIELDS);
            let mut fields = Map::new();
            for name in names {
                let value = values.get(name).expect("name came from the object");
                fields.insert(
                    name.clone(),
                    summarize_value(
                        value,
                        Some(name),
                        depth.saturating_add(1),
                        properties.get(name),
                    ),
                );
            }
            let fields_omitted = values.len().saturating_sub(fields.len());
            json!({
                "type": "object",
                "field_count": values.len(),
                "fields": fields,
                "fields_omitted": fields_omitted,
            })
        }
    }
}

fn redacted_value_summary(value: &Value) -> Value {
    match value {
        Value::Null => json!({ "type": "null", "redacted": true }),
        Value::Bool(_) => json!({ "type": "boolean", "redacted": true }),
        Value::Number(_) => json!({ "type": "number", "redacted": true }),
        Value::String(value) => json!({
            "type": "string",
            "bytes": value.len(),
            "redacted": true,
        }),
        Value::Array(values) => json!({
            "type": "array",
            "items": values.len(),
            "redacted": true,
        }),
        Value::Object(values) => json!({
            "type": "object",
            "fields": values.len(),
            "redacted": true,
        }),
    }
}

fn schema_properties(schema: Option<&Value>) -> Option<&Map<String, Value>> {
    schema?.get("properties").and_then(Value::as_object)
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "secret",
        "token",
        "password",
        "passphrase",
        "credential",
        "authorization",
        "cookie",
        "apikey",
        "privatekey",
        "content",
        "input",
        "patch",
        "chars",
        "data",
        "env",
        "header",
        "file",
        "path",
        "workdir",
        "command",
        "cmd",
        "args",
        "message",
        "value",
    ]
    .iter()
    .any(|fragment| normalized.contains(fragment))
}

fn hash_path(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(b"codexify/audit-project/v1\0");
    hasher.update(path.to_string_lossy().as_bytes());
    encode_hex(&hasher.finalize(), HASH_HEX_CHARS)
}

fn encode_hex(bytes: &[u8], chars: usize) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded.truncate(chars);
    encoded
}

fn random_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate audit run id: {error}"))?;
    Ok(encode_hex(&bytes, bytes.len() * 2))
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn collect_secret_values(config: &AppConfig) -> Zeroizing<Vec<String>> {
    let mut values = Vec::new();
    if let Some(api_key) = config.api_key.as_deref() {
        push_secret(&mut values, api_key, false);
    }
    for server in config.mcp_servers.values() {
        for value in server.env.values() {
            push_secret(&mut values, value, false);
        }
    }
    for name in &config.audit.redact_env {
        if let Ok(value) = std::env::var(name) {
            push_secret(&mut values, &value, false);
        }
    }
    for (name, value) in std::env::vars_os() {
        if let (Some(name), Some(value)) = (name.to_str(), value.to_str())
            && secret_env_name(name)
        {
            push_secret(&mut values, value, true);
        }
    }
    if let Some(tunnel) = config.openai_tunnel.as_ref() {
        if let Some(name) = tunnel.api_key_ref.strip_prefix("env:") {
            if let Ok(value) = std::env::var(name) {
                push_secret(&mut values, &value, false);
            }
        } else if let Some(path) = tunnel.api_key_ref.strip_prefix("file:") {
            let path = Path::new(path);
            if std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.len() <= MAX_REFERENCED_SECRET_BYTES
            }) && let Ok(value) = std::fs::read_to_string(path)
            {
                push_secret(&mut values, value.trim(), false);
            }
        }
    }
    values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    values.dedup();
    Zeroizing::new(values)
}

fn push_secret(values: &mut Vec<String>, value: &str, automatic: bool) {
    if !value.is_empty() && (!automatic || value.len() >= 8) {
        values.push(value.to_string());
    }
}

fn secret_env_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "passphrase",
        "credential",
        "authorization",
        "cookie",
        "api_key",
        "apikey",
        "private_key",
    ]
    .iter()
    .any(|fragment| normalized.contains(fragment))
}

fn secret_flag_takes_next(argument: &str) -> bool {
    if !argument.starts_with('-') || argument.contains('=') {
        return false;
    }
    let name = argument.trim_start_matches('-').to_ascii_lowercase();
    matches!(name.as_str(), "u" | "user" | "proxy-user") || secret_env_name(&name)
}

fn secret_patterns() -> Vec<(Regex, &'static str)> {
    [
        (
            r#"(?i)([\"']?(?:authorization|proxy-authorization)[\"']?\s*:\s*[\"']?(?:bearer|basic)\s+)[^\s'\";]+"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)(\bbearer\s+)[A-Za-z0-9._~+/-]+=*"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)((?:^|\s)--?[A-Za-z0-9_-]*(?:api[-_]?key|token|secret|password|passphrase|authorization|credential)[A-Za-z0-9_-]*(?:=|\s+))(?:\"[^\"]*\"|'[^']*'|[^\s;]+)"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)(\b[A-Za-z0-9_]*(?:api[_-]?key|token|secret|password|passphrase|authorization|credential)[A-Za-z0-9_]*\s*=\s*)(?:\"[^\"]*\"|'[^']*'|[^\s;]+)"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)([\"']?[A-Za-z0-9_-]*(?:api[-_]?key|token|secret|password|passphrase|credential)[A-Za-z0-9_-]*[\"']?\s*:\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;}]+)"#,
            "$1[REDACTED]",
        ),
    ]
    .into_iter()
    .map(|(pattern, replacement)| {
        (
            Regex::new(pattern).expect("static audit redaction regex"),
            replacement,
        )
    })
    .collect()
}

fn bound_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let marker = "...[truncated]";
    if max_bytes <= marker.len() {
        return marker[..max_bytes].to_string();
    }
    let mut end = max_bytes - marker.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], marker)
}

fn open_private_append(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create audit log directory {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "audit log path must not be a symbolic link: {}",
            path.display()
        );
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open audit log {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect audit log {}", path.display()))?;
    if !metadata.is_file() {
        bail!("audit log is not a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "audit log is readable by other users; run chmod 600 {}",
                path.display()
            );
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    #[test]
    fn argument_summary_exposes_shape_but_not_values() {
        let arguments = json!({
            "path": "/private/source.rs",
            "timeout": 1200,
            "flag": true,
            "nested": { "token": "top-secret", "count": 3 },
            "items": ["a", "b"]
        });
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "timeout": { "type": "number" },
                "flag": { "type": "boolean" },
                "nested": {
                    "type": "object",
                    "properties": {
                        "token": { "type": "string" },
                        "count": { "type": "number" }
                    }
                },
                "items": { "type": "array" }
            }
        });
        let summary = summarize_arguments(&arguments, Some(&schema)).to_string();
        assert!(!summary.contains("/private/source.rs"));
        assert!(!summary.contains("top-secret"));
        assert!(!summary.contains("\"a\""));
        assert!(!summary.contains("1200"));
        assert!(summary.contains("field_count"));
    }

    #[test]
    fn argument_summary_omits_unknown_and_sensitive_nested_field_names() {
        let arguments = json!({
            "known": true,
            "secret-as-an-unknown-key": "value",
            "env": { "SECRET_AS_A_DYNAMIC_KEY": "value" }
        });
        let schema = json!({
            "type": "object",
            "properties": {
                "known": { "type": "boolean" },
                "env": { "type": "object" }
            }
        });

        let summary = summarize_arguments(&arguments, Some(&schema)).to_string();
        assert!(summary.contains("known"));
        assert!(summary.contains("env"));
        assert!(!summary.contains("secret-as-an-unknown-key"));
        assert!(!summary.contains("SECRET_AS_A_DYNAMIC_KEY"));
    }

    #[test]
    fn command_redaction_covers_known_values_and_secret_syntax() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.audit.log_file = Some(root.path().join("audit.jsonl"));
        config.audit.include_command_preview = true;
        config.api_key = Some("literal-known-secret".to_string());
        let logger = AuditLogger::open(&config).unwrap().unwrap();
        let redacted = logger.redact_command(
            "tool --github-token prefixed-token OPENAI_API_KEY=assigned-key literal-known-secret \"api_key\": \"json-secret\" Authorization: Basic basic-token Bearer abc.def",
        );
        assert!(!redacted.contains("prefixed-token"));
        assert!(!redacted.contains("assigned-key"));
        assert!(!redacted.contains("literal-known-secret"));
        assert!(!redacted.contains("json-secret"), "{redacted}");
        assert!(!redacted.contains("basic-token"));
        assert!(!redacted.contains("abc.def"));
        assert!(redacted.matches("[REDACTED]").count() >= 6);
    }

    #[test]
    fn run_command_preview_redacts_separate_secret_arguments() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.audit.log_file = Some(root.path().join("audit.jsonl"));
        config.audit.include_command_preview = true;
        let logger = AuditLogger::open(&config).unwrap().unwrap();
        let arguments = json!({
            "command": "tool",
            "args": [
                "--github-token",
                "separate-token-value",
                "--header",
                "Authorization: Bearer header-token-value",
                "--user",
                "name:password"
            ]
        });

        let preview = logger.command_preview("run_command", &arguments).unwrap();
        assert!(!preview.contains("separate-token-value"), "{preview}");
        assert!(!preview.contains("header-token-value"), "{preview}");
        assert!(!preview.contains("name:password"), "{preview}");
        assert!(preview.matches("[REDACTED]").count() >= 3, "{preview}");
    }

    #[test]
    fn jsonl_records_metadata_without_arguments_or_output() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("audit.jsonl");
        let mut config = default_config(root.path().to_path_buf());
        config.audit.log_file = Some(path.clone());
        config.audit.include_command_preview = true;
        config.api_key = Some("known-secret-value".to_string());
        let logger = AuditLogger::open(&config).unwrap().unwrap();
        let conversation =
            ConversationIdentity::from_openai_session("raw-conversation-id").unwrap();
        let scope = AuditScope::new(7, Some(&conversation), root.path(), Some(root.path()));
        let arguments = json!({ "cmd": "echo known-secret-value", "workdir": "/private" });
        let schema = json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string" },
                "workdir": { "type": "string" }
            }
        });
        let call = logger.begin_tool("exec_command", &arguments, Some(&schema), &scope);
        let result = ToolResult::text("returned sensitive output").with_truncation(true);
        logger.finish_tool(&call, "exec_command", &result, 17, &scope);
        drop(logger);

        let contents = std::fs::read_to_string(path).unwrap();
        assert!(!contents.contains("known-secret-value"));
        assert!(!contents.contains("returned sensitive output"));
        assert!(!contents.contains("raw-conversation-id"));
        assert!(!contents.contains("/private"));
        let events: Vec<Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["event"], "audit_started");
        assert_eq!(events[0]["server_version"], env!("CARGO_PKG_VERSION"));
        assert!(events[0]["server_process_id"].as_u64().is_some());
        assert_eq!(events[1]["event"], "tool_start");
        assert_eq!(events[2]["event"], "tool_finish");
        assert_eq!(events[2]["duration_ms"], 17);
        assert_eq!(events[2]["output"]["truncated"], true);
        assert_eq!(events[2]["output"]["text_bytes"], 25);
    }

    #[test]
    fn project_hashes_are_stable_and_do_not_contain_paths() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert_eq!(hash_path(first.path()), hash_path(first.path()));
        assert_ne!(hash_path(first.path()), hash_path(second.path()));
        assert_eq!(hash_path(first.path()).len(), HASH_HEX_CHARS);
    }

    #[test]
    fn utf8_preview_bound_is_byte_strict() {
        let bounded = bound_utf8(&"é".repeat(100), 31);
        assert!(bounded.len() <= 31);
        assert!(bounded.ends_with("...[truncated]"));
    }

    #[cfg(unix)]
    #[test]
    fn audit_log_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("audit.jsonl");
        let mut config = default_config(root.path().to_path_buf());
        config.audit.log_file = Some(path.clone());
        drop(AuditLogger::open(&config).unwrap());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
