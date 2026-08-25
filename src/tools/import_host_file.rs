use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::{MetaObject, ToolAnnotations};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::time::{Duration, Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::artifact_ingress::{OpenAiFileParam, import_openai_file_before};
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolRequestContext, arg_str};
use crate::types::{AppConfig, DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS, ToolResult};

pub struct ImportHostFile {
    permits: Arc<Semaphore>,
}

impl ImportHostFile {
    pub const NAME: &'static str = "import_host_file";

    pub fn new(max_concurrent_downloads: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent_downloads.max(1))),
        }
    }

    async fn run(
        &self,
        args: Value,
        config: &AppConfig,
        cancellation: &CancellationToken,
    ) -> ToolResult {
        if !config.artifact_ingress.enabled {
            return ToolResult::error(
                "artifact_ingress_disabled: Native file ingress is disabled by configuration.",
            );
        }
        let Some(file_value) = args.get("file") else {
            return ToolResult::error("file must be a ChatGPT native-file object");
        };
        let Some(path) = arg_str(&args, "path") else {
            return ToolResult::error("path must be a string");
        };
        let file = match OpenAiFileParam::parse(file_value) {
            Ok(file) => file,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let Some(deadline) = Instant::now().checked_add(Duration::from_millis(
            config.artifact_ingress.request_timeout_ms,
        )) else {
            return ToolResult::error(
                "artifact_ingress_invalid: artifactIngress.requestTimeoutMs is too large for this platform.",
            );
        };
        let permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return ToolResult::error(
                    "file_import_cancelled: Native file ingress was cancelled by the MCP client.",
                );
            }
            result = timeout_at(deadline, self.permits.acquire()) => match result {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => {
                    return ToolResult::error(
                        "file_import_unavailable: Native file ingress is shutting down.",
                    );
                }
                Err(_) => {
                    return ToolResult::error(
                        "file_import_timed_out: Native file ingress remained at its concurrency limit until the request deadline.",
                    );
                }
            }
        };
        let result = import_openai_file_before(config, &file, path, deadline, cancellation).await;
        drop(permit);

        match result {
            Ok(receipt) => {
                let text = format!(
                    "Imported {} bytes to {} ({})",
                    receipt.bytes, receipt.path, receipt.sha256
                );
                let structured = serde_json::to_value(&receipt)
                    .expect("native-file import receipt must serialize");
                ToolResult::text(text).with_structured(structured)
            }
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}

impl Default for ImportHostFile {
    fn default() -> Self {
        Self::new(DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS)
    }
}

#[async_trait]
impl Tool for ImportHostFile {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> Option<String> {
        Some("Import attached file".to_string())
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(false)
                .open_world(true),
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "openai/fileParams": ["file"],
                "openai/toolInvocation/invoking": "Importing file",
                "openai/toolInvocation/invoked": "File imported"
            }))
            .expect("static native-file tool metadata must be an object"),
        )
    }

    fn description(&self) -> String {
        "Import one user-attached or ChatGPT-generated native file into the active project. The host supplies a temporary authorized OpenAI file reference; arbitrary URLs and local source paths are rejected. The destination is project-relative, created only after the complete file is verified, and never overwrites an existing path."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "$defs": {
                "OpenAIFile": {
                    "type": "object",
                    "properties": {
                        "download_url": { "type": "string" },
                        "file_id": { "type": "string" },
                        "mime_type": { "type": "string" },
                        "file_name": { "type": "string" }
                    },
                    "required": ["download_url", "file_id"],
                    "additionalProperties": false
                }
            },
            "properties": {
                "file": {
                    "$ref": "#/$defs/OpenAIFile",
                    "description": "Native file value authorized and supplied by ChatGPT."
                },
                "path": {
                    "type": "string",
                    "description": "New destination file path relative to the active project root."
                }
            },
            "required": ["file", "path"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "bytes": { "type": "integer" },
                "sha256": { "type": "string" },
                "source": { "type": "string", "enum": ["openai_file"] },
                "mimeType": { "type": "string" }
            },
            "required": ["path", "bytes", "sha256", "source"],
            "additionalProperties": false
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        self.run(args, config, &CancellationToken::new()).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, config, &context.cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_the_openai_native_file_contract() {
        let tool = ImportHostFile::default();
        let schema = tool.input_schema();
        let file = &schema["$defs"]["OpenAIFile"];
        assert_eq!(file["required"], json!(["download_url", "file_id"]));
        assert_eq!(file["properties"].as_object().unwrap().len(), 4);
        for property in ["download_url", "file_id", "mime_type", "file_name"] {
            assert_eq!(file["properties"][property]["type"], "string");
        }
        assert_eq!(schema["required"], json!(["file", "path"]));

        let meta = tool.meta().unwrap();
        assert_eq!(meta["openai/fileParams"], json!(["file"]));
        let annotations = tool.annotations().unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    #[tokio::test]
    async fn rejects_non_native_file_values_without_network_access() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::default_config(root.path().to_path_buf());
        let result = ImportHostFile::default()
            .call(
                json!({ "file": "https://example.com/file", "path": "file.bin" }),
                &config,
                &SessionState::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("invalid_file_reference"));
    }

    #[tokio::test]
    async fn concurrency_queue_is_bounded_by_the_request_deadline() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.artifact_ingress.request_timeout_ms = 10;
        config.artifact_ingress.idle_timeout_ms = 10;
        let tool = ImportHostFile::new(1);
        let _held = tool.permits.acquire().await.unwrap();

        let result = tool
            .call(
                json!({
                    "file": {
                        "download_url": "https://files.oaiusercontent.com/object",
                        "file_id": "file_test"
                    },
                    "path": "file.bin"
                }),
                &config,
                &SessionState::new(),
            )
            .await;

        assert!(result.is_error);
        assert!(result.joined_text().contains("file_import_timed_out"));
        assert!(!root.path().join("file.bin").exists());
    }

    #[tokio::test]
    async fn mcp_cancellation_interrupts_the_concurrency_queue() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::default_config(root.path().to_path_buf());
        let tool = ImportHostFile::new(1);
        let _held = tool.permits.acquire().await.unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let context = ToolRequestContext {
            conversation: None,
            conversation_authorizations: Arc::new(
                crate::conversation_auth::ConversationAuthorizationStore::new(),
            ),
            review_checkpoints: Arc::new(crate::review::ReviewCheckpointManager::new()),
            cancellation,
        };

        let result = tool
            .call_with_context(
                json!({
                    "file": {
                        "download_url": "https://files.oaiusercontent.com/object",
                        "file_id": "file_test"
                    },
                    "path": "file.bin"
                }),
                &config,
                &SessionState::new(),
                &context,
            )
            .await;

        assert!(result.is_error);
        assert!(result.joined_text().contains("file_import_cancelled"));
        assert!(!root.path().join("file.bin").exists());
    }
}
