use std::cell::Cell;
use std::path::Path;

use regex::Regex;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::types::AppConfig;

const MAX_REFERENCED_SECRET_BYTES: u64 = 64 * 1024;
const MAX_SECRET_MATCH_BYTES: usize = 64 * 1024;
const REDACTION_LOOKAHEAD_BYTES: usize = 256;
const MAX_SCHEMA_COMPOSITION_BRANCHES: usize = 32;
const MAX_SCHEMA_COMPOSITION_DEPTH: usize = 16;
const REDACTED: &str = "[REDACTED]";
const VALUE_TRUNCATED: &str = "...[value truncated]...";

pub(crate) struct SecretRedactor {
    secrets: Zeroizing<Vec<String>>,
    secret_patterns: Vec<(Regex, &'static str)>,
    max_secret_bytes: usize,
}

pub(crate) struct RedactedJson<'a> {
    redactor: &'a SecretRedactor,
    value: &'a Value,
    schema: Option<&'a Value>,
    max_text_bytes: usize,
    traversal_truncated: &'a Cell<bool>,
}

pub(crate) struct RedactedJsonMap<'a> {
    redactor: &'a SecretRedactor,
    values: &'a Map<String, Value>,
    max_text_bytes: usize,
    traversal_truncated: &'a Cell<bool>,
}

pub(crate) struct RedactedText<'a> {
    redactor: &'a SecretRedactor,
    text: &'a str,
    max_text_bytes: usize,
    traversal_truncated: &'a Cell<bool>,
}

impl SecretRedactor {
    pub(crate) fn for_audit(config: &AppConfig) -> Self {
        Self::from_config(config, &config.audit.redact_env, true)
    }

    pub(crate) fn for_tool_logging(config: &AppConfig) -> Self {
        Self::from_config(config, &config.tool_logging.redact_env, false)
    }

    fn from_config(config: &AppConfig, redact_env: &[String], redact_all_mcp_values: bool) -> Self {
        let secrets = collect_secret_values(config, redact_env, redact_all_mcp_values);
        let max_secret_bytes = secrets.iter().map(String::len).max().unwrap_or(0);
        Self {
            secrets,
            secret_patterns: secret_patterns(),
            max_secret_bytes,
        }
    }

    pub(crate) fn redact_text(&self, text: &str) -> String {
        let mut redacted = text.replace('\0', "\u{fffd}");
        for secret in self.secrets.iter() {
            redacted = redacted.replace(secret, REDACTED);
        }
        for (pattern, replacement) in &self.secret_patterns {
            redacted = pattern.replace_all(&redacted, *replacement).into_owned();
        }
        redacted
    }

    pub(crate) fn redact_text_preview(&self, text: &str, max_payload_bytes: usize) -> String {
        let traversal_truncated = Cell::new(false);
        self.redact_text_bounded(
            text,
            self.max_text_bytes(max_payload_bytes),
            &traversal_truncated,
        )
    }

    pub(crate) fn redact_argv_preview<'a>(
        &self,
        argv: impl IntoIterator<Item = &'a str>,
        max_payload_bytes: usize,
    ) -> String {
        let traversal_truncated = Cell::new(false);
        let mut output = String::from("[");
        let mut redact_next = false;
        let mut first = true;
        for argument in argv {
            let remaining = max_payload_bytes.saturating_sub(output.len());
            if remaining == 0 {
                output.push_str(VALUE_TRUNCATED);
                break;
            }
            let redacted = if redact_next {
                redact_next = false;
                REDACTED.to_string()
            } else {
                redact_next = secret_flag_takes_next(argument);
                self.redact_text_bounded(
                    argument,
                    self.max_text_bytes(remaining),
                    &traversal_truncated,
                )
            };
            let encoded =
                serde_json::to_string(&redacted).unwrap_or_else(|_| format!("\"{REDACTED}\""));
            if !first {
                output.push(',');
            }
            output.push_str(&encoded);
            first = false;
            if output.len() >= max_payload_bytes {
                output.push_str(VALUE_TRUNCATED);
                break;
            }
        }
        output.push(']');
        output
    }

    pub(crate) fn redacted_json<'a>(
        &'a self,
        value: &'a Value,
        schema: Option<&'a Value>,
        max_payload_bytes: usize,
        traversal_truncated: &'a Cell<bool>,
    ) -> RedactedJson<'a> {
        RedactedJson {
            redactor: self,
            value,
            schema,
            max_text_bytes: self.max_text_bytes(max_payload_bytes),
            traversal_truncated,
        }
    }

    pub(crate) fn redacted_json_map<'a>(
        &'a self,
        values: &'a Map<String, Value>,
        max_payload_bytes: usize,
        traversal_truncated: &'a Cell<bool>,
    ) -> RedactedJsonMap<'a> {
        RedactedJsonMap {
            redactor: self,
            values,
            max_text_bytes: self.max_text_bytes(max_payload_bytes),
            traversal_truncated,
        }
    }

    pub(crate) fn redacted_text<'a>(
        &'a self,
        text: &'a str,
        max_payload_bytes: usize,
        traversal_truncated: &'a Cell<bool>,
    ) -> RedactedText<'a> {
        RedactedText {
            redactor: self,
            text,
            max_text_bytes: self.max_text_bytes(max_payload_bytes),
            traversal_truncated,
        }
    }

    fn max_text_bytes(&self, max_payload_bytes: usize) -> usize {
        max_payload_bytes
            .saturating_add(self.max_secret_bytes)
            .saturating_add(REDACTION_LOOKAHEAD_BYTES)
    }

    fn redact_text_bounded(
        &self,
        text: &str,
        max_text_bytes: usize,
        traversal_truncated: &Cell<bool>,
    ) -> String {
        let (prefix, prefix_truncated) = utf8_prefix(text, max_text_bytes);
        let mut redacted = self.redact_text(prefix);
        if prefix_truncated {
            traversal_truncated.set(true);
            redacted.push_str(VALUE_TRUNCATED);
        }
        redacted
    }
}

impl Serialize for RedactedJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.schema.is_some_and(schema_marks_sensitive) {
            return serializer.serialize_str(REDACTED);
        }
        match self.value {
            Value::Null => serializer.serialize_none(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Number(value) => value.serialize(serializer),
            Value::String(text) => serializer.serialize_str(&self.redactor.redact_text_bounded(
                text,
                self.max_text_bytes,
                self.traversal_truncated,
            )),
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                let item_schema = self.schema.and_then(schema_items);
                for value in values {
                    sequence.serialize_element(&RedactedJson {
                        redactor: self.redactor,
                        value,
                        schema: item_schema,
                        max_text_bytes: self.max_text_bytes,
                        traversal_truncated: self.traversal_truncated,
                    })?;
                }
                sequence.end()
            }
            Value::Object(values) => serialize_redacted_map(
                serializer,
                self.redactor,
                values,
                self.schema,
                self.max_text_bytes,
                self.traversal_truncated,
            ),
        }
    }
}

impl Serialize for RedactedJsonMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_redacted_map(
            serializer,
            self.redactor,
            self.values,
            None,
            self.max_text_bytes,
            self.traversal_truncated,
        )
    }
}

impl Serialize for RedactedText<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.redactor.redact_text_bounded(
            self.text,
            self.max_text_bytes,
            self.traversal_truncated,
        ))
    }
}

fn serialize_redacted_map<S>(
    serializer: S,
    redactor: &SecretRedactor,
    values: &Map<String, Value>,
    schema: Option<&Value>,
    max_text_bytes: usize,
    traversal_truncated: &Cell<bool>,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut map = serializer.serialize_map(Some(values.len()))?;
    for (name, value) in values {
        map.serialize_key(name)?;
        if sensitive_json_key(name)
            || schema.is_some_and(|schema| schema_property_is_sensitive(schema, name))
        {
            map.serialize_value(REDACTED)?;
        } else {
            map.serialize_value(&RedactedJson {
                redactor,
                value,
                schema: schema.and_then(|schema| schema_property(schema, name)),
                max_text_bytes,
                traversal_truncated,
            })?;
        }
    }
    map.end()
}

fn collect_secret_values(
    config: &AppConfig,
    redact_env: &[String],
    redact_all_mcp_values: bool,
) -> Zeroizing<Vec<String>> {
    let mut values = Vec::new();
    if let Some(api_key) = config.api_key.as_deref() {
        push_secret(&mut values, api_key, false);
    }
    if let Some(token) = config.conversation_auth_token.as_deref() {
        push_secret(&mut values, token, false);
    }
    for server in config.mcp_servers.values() {
        for (name, value) in &server.env {
            push_secret(
                &mut values,
                value,
                !redact_all_mcp_values && !secret_env_name(name),
            );
        }
        for (name, value) in &server.http_headers {
            push_secret(
                &mut values,
                value,
                !redact_all_mcp_values && !secret_env_name(name),
            );
        }
        if let Some(name) = server.bearer_token_env_var.as_deref()
            && let Ok(value) = std::env::var(name)
        {
            push_secret(&mut values, &value, false);
        }
        for (header, name) in &server.env_http_headers {
            if let Ok(value) = std::env::var(name) {
                push_secret(
                    &mut values,
                    &value,
                    !redact_all_mcp_values && !secret_env_name(header) && !secret_env_name(name),
                );
            }
        }
    }
    for name in redact_env {
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

fn schema_marks_sensitive(schema: &Value) -> bool {
    schema_marks_sensitive_at(schema, 0)
}

fn schema_marks_sensitive_at(schema: &Value, depth: usize) -> bool {
    if depth >= MAX_SCHEMA_COMPOSITION_DEPTH
        || schema_composition_count(schema) > MAX_SCHEMA_COMPOSITION_BRANCHES
    {
        return true;
    }
    schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || schema
            .get("format")
            .and_then(Value::as_str)
            .is_some_and(|format| format.eq_ignore_ascii_case("password"))
        || schema_compositions(schema).any(|schema| schema_marks_sensitive_at(schema, depth + 1))
}

fn schema_property_is_sensitive(schema: &Value, property: &str) -> bool {
    schema_property_is_sensitive_at(schema, property, 0)
}

fn schema_property_is_sensitive_at(schema: &Value, property: &str, depth: usize) -> bool {
    if depth >= MAX_SCHEMA_COMPOSITION_DEPTH
        || schema_composition_count(schema) > MAX_SCHEMA_COMPOSITION_BRANCHES
    {
        return true;
    }
    schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(property))
        .is_some_and(schema_marks_sensitive)
        || schema_compositions(schema)
            .any(|schema| schema_property_is_sensitive_at(schema, property, depth + 1))
}

fn schema_property<'a>(schema: &'a Value, property: &str) -> Option<&'a Value> {
    schema_property_at(schema, property, 0)
}

fn schema_property_at<'a>(schema: &'a Value, property: &str, depth: usize) -> Option<&'a Value> {
    if depth >= MAX_SCHEMA_COMPOSITION_DEPTH
        || schema_composition_count(schema) > MAX_SCHEMA_COMPOSITION_BRANCHES
    {
        return None;
    }
    schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(property))
        .or_else(|| {
            schema_compositions(schema)
                .find_map(|schema| schema_property_at(schema, property, depth + 1))
        })
}

fn schema_items(schema: &Value) -> Option<&Value> {
    schema_items_at(schema, 0)
}

fn schema_items_at(schema: &Value, depth: usize) -> Option<&Value> {
    if depth >= MAX_SCHEMA_COMPOSITION_DEPTH
        || schema_composition_count(schema) > MAX_SCHEMA_COMPOSITION_BRANCHES
    {
        return None;
    }
    schema.get("items").or_else(|| {
        schema_compositions(schema).find_map(|schema| schema_items_at(schema, depth + 1))
    })
}

fn schema_composition_count(schema: &Value) -> usize {
    ["allOf", "anyOf", "oneOf"]
        .into_iter()
        .filter_map(|keyword| schema.get(keyword).and_then(Value::as_array))
        .map(Vec::len)
        .fold(0usize, usize::saturating_add)
}

fn schema_compositions(schema: &Value) -> impl Iterator<Item = &Value> {
    ["allOf", "anyOf", "oneOf"]
        .into_iter()
        .filter_map(|keyword| schema.get(keyword).and_then(Value::as_array))
        .flatten()
}

fn push_secret(values: &mut Vec<String>, value: &str, automatic: bool) {
    if !value.is_empty() && (!automatic || value.len() >= 8) {
        values.push(utf8_prefix(value, MAX_SECRET_MATCH_BYTES).0.to_string());
    }
}

fn utf8_prefix(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

fn sensitive_json_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();

    matches!(
        normalized.as_str(),
        "token"
            | "secret"
            | "password"
            | "passphrase"
            | "credential"
            | "authorization"
            | "cookie"
            | "apikey"
            | "privatekey"
            | "downloadurl"
            | "fileid"
            | "env"
            | "headers"
            | "httpheaders"
            | "envhttpheaders"
            | "checksum"
            | "digest"
            | "md5"
            | "sha1"
            | "sha256"
            | "sha384"
            | "sha512"
            | "crc32"
            | "hash"
            | "etag"
            | "fingerprint"
            | "base64"
            | "binary"
            | "blob"
    ) || normalized.ends_with("token")
        || normalized.contains("secret")
        || normalized.ends_with("cookie")
        || normalized.contains("password")
        || normalized.contains("passphrase")
        || normalized.contains("credential")
        || normalized.contains("authorization")
        || normalized.contains("privatekey")
        || normalized.contains("apikey")
        || normalized.ends_with("checksum")
        || normalized.ends_with("digest")
        || normalized.ends_with("hash")
        || normalized.ends_with("fingerprint")
        || normalized.ends_with("base64")
        || normalized.ends_with("blobdata")
        || normalized.ends_with("binarydata")
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
        "access_key",
        "account_key",
        "signing_key",
        "database_url",
        "connection_string",
        "jwt",
    ]
    .iter()
    .any(|fragment| normalized.contains(fragment))
        || normalized.ends_with("_key")
        || normalized.ends_with("_pat")
        || normalized.ends_with("_auth")
        || normalized.ends_with("_dsn")
}

fn secret_flag_takes_next(argument: &str) -> bool {
    if !argument.starts_with('-') || argument.contains('=') {
        return false;
    }
    let name = argument.trim_start_matches('-').to_ascii_lowercase();
    matches!(name.as_str(), "u" | "user" | "proxy-user") || sensitive_value_name(&name)
}

fn sensitive_value_name(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    secret_env_name(name)
        || normalized.contains("checksum")
        || normalized.contains("digest")
        || normalized.contains("fingerprint")
        || normalized.ends_with("hash")
        || matches!(
            normalized.as_str(),
            "md5" | "sha1" | "sha256" | "sha384" | "sha512" | "crc32" | "etag"
        )
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
            r#"(?i)((?:^|\s)--?[A-Za-z0-9_-]*(?:api[-_]?key|token|secret|password|passphrase|authorization|credential|checksum|digest|fingerprint|hash|etag|sha(?:1|256|384|512)|md5|crc32)[A-Za-z0-9_-]*(?:=|\s+))(?:\"[^\"]*\"|'[^']*'|[^\s;]+)"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)(\b[A-Za-z0-9_]*(?:api[_-]?key|token|secret|password|passphrase|authorization|credential|checksum|digest|fingerprint|hash|etag|sha(?:1|256|384|512)|md5|crc32)[A-Za-z0-9_]*\s*=\s*)(?:\"[^\"]*\"|'[^']*'|[^\s;]+)"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)([\"']?[A-Za-z0-9_-]*(?:api[-_]?key|token|secret|password|passphrase|credential|checksum|digest|fingerprint|hash|etag|sha(?:1|256|384|512)|md5|crc32)[A-Za-z0-9_-]*[\"']?\s*:\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;}]+)"#,
            "$1[REDACTED]",
        ),
        (
            r#"(?i)([?&](?:x-amz-signature|signature|sig|token|access_token|api[-_]?key)=)[^&\s'\"]+"#,
            "$1[REDACTED]",
        ),
    ]
    .into_iter()
    .map(|(pattern, replacement)| {
        (
            Regex::new(pattern).expect("static secret-redaction regex"),
            replacement,
        )
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::config::default_config;

    #[test]
    fn json_redaction_preserves_useful_values_and_removes_credentials() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.api_key = Some("known-api-secret".to_string());
        let redactor = SecretRedactor::for_tool_logging(&config);
        let value = json!({
            "path": "src/main.rs",
            "cmd": "echo known-api-secret",
            "checksum": "known-api-secret",
            "sha256": "0123456789abcdef",
            "content_hash": "content-hash-value",
            "image_base64": "raw-binary-text",
            "download_url": "https://files.example/object?signature=capability",
            "file_id": "file_sensitive_identifier",
            "github_token": "github-token-value",
            "original_token_count": 42,
            "nested": { "accessToken": "nested-secret", "message": "visible" }
        });

        let traversal_truncated = Cell::new(false);
        let rendered = serde_json::to_string(&redactor.redacted_json(
            &value,
            None,
            4096,
            &traversal_truncated,
        ))
        .unwrap();
        assert!(rendered.contains("src/main.rs"));
        assert!(rendered.contains("original_token_count"));
        assert!(rendered.contains("42"));
        assert!(rendered.contains("visible"));
        assert!(!rendered.contains("known-api-secret"));
        assert!(!rendered.contains("0123456789abcdef"));
        assert!(!rendered.contains("content-hash-value"));
        assert!(!rendered.contains("raw-binary-text"));
        assert!(!rendered.contains("capability"));
        assert!(!rendered.contains("file_sensitive_identifier"));
        assert!(!rendered.contains("github-token-value"));
        assert!(!rendered.contains("nested-secret"));
    }

    #[test]
    fn argv_redaction_covers_separate_secret_values() {
        let root = tempfile::tempdir().unwrap();
        let config = default_config(root.path().to_path_buf());
        let redactor = SecretRedactor::for_tool_logging(&config);
        let redacted = redactor.redact_argv_preview(
            [
                "tool",
                "--github-token",
                "separate-secret",
                "--header",
                "Authorization: Bearer header-secret",
                "--checksum",
                "deadbeef-checksum",
            ],
            4096,
        );
        let rendered = serde_json::to_string(&redacted).unwrap();
        assert!(!rendered.contains("separate-secret"));
        assert!(!rendered.contains("header-secret"));
        assert!(!rendered.contains("deadbeef-checksum"));
    }

    #[test]
    fn short_benign_mcp_values_do_not_corrupt_unrelated_payload_text() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.mcp_servers.insert(
            "fixture".to_string(),
            crate::types::McpServerSpec {
                env: HashMap::from([
                    ("FEATURE_FLAG".to_string(), "1".to_string()),
                    ("API_TOKEN".to_string(), "abc".to_string()),
                ]),
                http_headers: HashMap::from([
                    ("X-Client".to_string(), "dev".to_string()),
                    ("Authorization".to_string(), "xyz".to_string()),
                ]),
                ..Default::default()
            },
        );
        let redactor = SecretRedactor::for_tool_logging(&config);

        let redacted = redactor.redact_text("1 dev abc xyz");
        assert!(redacted.contains("1 dev"));
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("xyz"));
    }

    #[test]
    fn audit_policy_preserves_redaction_of_short_mcp_environment_values() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.mcp_servers.insert(
            "fixture".to_string(),
            crate::types::McpServerSpec {
                env: HashMap::from([("UNCLASSIFIED_VALUE".to_string(), "abc".to_string())]),
                ..Default::default()
            },
        );
        let redactor = SecretRedactor::for_audit(&config);

        assert_eq!(redactor.redact_text("echo abc"), "echo [REDACTED]");
    }

    #[test]
    fn json_schema_write_only_and_password_fields_are_redacted() {
        let root = tempfile::tempdir().unwrap();
        let config = default_config(root.path().to_path_buf());
        let redactor = SecretRedactor::for_tool_logging(&config);
        let value = json!({
            "ref": "stale-auth-reference",
            "nested": {
                "password": "schema-password",
                "visible": "keep-me"
            }
        });
        let schema = json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "writeOnly": true },
                "nested": {
                    "type": "object",
                    "properties": {
                        "password": { "type": "string", "format": "password" },
                        "visible": { "type": "string" }
                    }
                }
            }
        });

        let traversal_truncated = Cell::new(false);
        let rendered = serde_json::to_string(&redactor.redacted_json(
            &value,
            Some(&schema),
            4096,
            &traversal_truncated,
        ))
        .unwrap();
        assert!(!rendered.contains("stale-auth-reference"));
        assert!(!rendered.contains("schema-password"));
        assert!(rendered.contains("keep-me"));
    }
}
