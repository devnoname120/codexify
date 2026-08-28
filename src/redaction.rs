use std::path::Path;

use regex::Regex;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::types::AppConfig;

const MAX_REFERENCED_SECRET_BYTES: u64 = 64 * 1024;
const REDACTED: &str = "[REDACTED]";

pub(crate) struct SecretRedactor {
    secrets: Zeroizing<Vec<String>>,
    secret_patterns: Vec<(Regex, &'static str)>,
}

impl SecretRedactor {
    pub(crate) fn from_config(config: &AppConfig, redact_env: &[String]) -> Self {
        Self {
            secrets: collect_secret_values(config, redact_env),
            secret_patterns: secret_patterns(),
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

    pub(crate) fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        let mut redact_next = false;
        argv.iter()
            .map(|argument| {
                if redact_next {
                    redact_next = false;
                    return REDACTED.to_string();
                }
                redact_next = secret_flag_takes_next(argument);
                self.redact_text(argument)
            })
            .collect()
    }

    pub(crate) fn redact_json(&self, value: &Value) -> Value {
        self.redact_json_value(value, None)
    }

    fn redact_json_value(&self, value: &Value, key: Option<&str>) -> Value {
        if key.is_some_and(sensitive_json_key) {
            return Value::String(REDACTED.to_string());
        }
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
            Value::String(text) => Value::String(self.redact_text(text)),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|value| self.redact_json_value(value, None))
                    .collect(),
            ),
            Value::Object(values) => {
                let mut redacted = Map::with_capacity(values.len());
                for (name, value) in values {
                    redacted.insert(
                        name.clone(),
                        self.redact_json_value(value, Some(name.as_str())),
                    );
                }
                Value::Object(redacted)
            }
        }
    }
}

fn collect_secret_values(config: &AppConfig, redact_env: &[String]) -> Zeroizing<Vec<String>> {
    let mut values = Vec::new();
    if let Some(api_key) = config.api_key.as_deref() {
        push_secret(&mut values, api_key, false);
    }
    if let Some(token) = config.conversation_auth_token.as_deref() {
        push_secret(&mut values, token, false);
    }
    for server in config.mcp_servers.values() {
        for (name, value) in &server.env {
            push_secret(&mut values, value, !secret_env_name(name));
        }
        for (name, value) in &server.http_headers {
            push_secret(&mut values, value, !secret_env_name(name));
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
                    !secret_env_name(header) && !secret_env_name(name),
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

fn push_secret(values: &mut Vec<String>, value: &str, automatic: bool) {
    if !value.is_empty() && (!automatic || value.len() >= 8) {
        values.push(value.to_string());
    }
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
    ) || normalized.ends_with("token")
        || normalized.contains("secret")
        || normalized.ends_with("cookie")
        || normalized.contains("password")
        || normalized.contains("passphrase")
        || normalized.contains("credential")
        || normalized.contains("authorization")
        || normalized.contains("privatekey")
        || normalized.contains("apikey")
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
        let redactor = SecretRedactor::from_config(&config, &[]);
        let value = json!({
            "path": "src/main.rs",
            "cmd": "echo known-api-secret",
            "checksum": "known-api-secret",
            "sha256": "0123456789abcdef",
            "download_url": "https://files.example/object?signature=capability",
            "file_id": "file_sensitive_identifier",
            "github_token": "github-token-value",
            "original_token_count": 42,
            "nested": { "accessToken": "nested-secret", "message": "visible" }
        });

        let redacted = redactor.redact_json(&value);
        let rendered = redacted.to_string();
        assert!(rendered.contains("src/main.rs"));
        assert!(rendered.contains("original_token_count"));
        assert!(rendered.contains("42"));
        assert!(rendered.contains("visible"));
        assert!(!rendered.contains("known-api-secret"));
        assert!(rendered.contains("0123456789abcdef"));
        assert!(!rendered.contains("capability"));
        assert!(!rendered.contains("file_sensitive_identifier"));
        assert!(!rendered.contains("github-token-value"));
        assert!(!rendered.contains("nested-secret"));
    }

    #[test]
    fn argv_redaction_covers_separate_secret_values() {
        let root = tempfile::tempdir().unwrap();
        let config = default_config(root.path().to_path_buf());
        let redactor = SecretRedactor::from_config(&config, &[]);
        let redacted = redactor.redact_argv(&[
            "tool".into(),
            "--github-token".into(),
            "separate-secret".into(),
            "--header".into(),
            "Authorization: Bearer header-secret".into(),
        ]);
        let rendered = serde_json::to_string(&redacted).unwrap();
        assert!(!rendered.contains("separate-secret"));
        assert!(!rendered.contains("header-secret"));
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
        let redactor = SecretRedactor::from_config(&config, &[]);

        let redacted = redactor.redact_text("1 dev abc xyz");
        assert!(redacted.contains("1 dev"));
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("xyz"));
    }
}
