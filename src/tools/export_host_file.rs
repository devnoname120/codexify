use async_trait::async_trait;
use rmcp::model::{MetaObject, ToolAnnotations};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::artifact_egress::{ArtifactEgressStore, snapshot_project_file};
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolRequestContext, arg_str};
use crate::types::{AppConfig, ToolContent, ToolResult};

pub struct ExportHostFile;

impl ExportHostFile {
    pub const NAME: &'static str = "export_host_file";

    async fn run(
        &self,
        args: Value,
        config: &AppConfig,
        store: &ArtifactEgressStore,
        cancellation: &CancellationToken,
    ) -> ToolResult {
        if !config.artifact_egress.enabled {
            return ToolResult::error(
                "artifact_egress_disabled: File export is disabled by configuration.",
            );
        }
        let Some(path) = arg_str(&args, "path") else {
            return ToolResult::error("path must be a string");
        };
        let snapshot = match snapshot_project_file(
            &config.work_dir,
            path,
            &config.artifact_egress,
            cancellation,
        )
        .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let registered = match store.register(snapshot) {
            Ok(registered) => registered,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let text = format!(
            "Exported `{path}` as `{}` ({} bytes, SHA-256 {}). The attached resource is available for {} ms.",
            registered.name, registered.byte_count, registered.sha256, registered.expires_in_ms
        );
        ToolResult {
            content: vec![
                ToolContent::Text(text),
                ToolContent::ResourceLink(registered.resource),
            ],
            is_error: false,
            structured_content: Some(json!({
                "path": path,
                "name": registered.name,
                "bytes": registered.byte_count,
                "sha256": registered.sha256,
                "mimeType": registered.mime_type,
                "expiresInMs": registered.expires_in_ms
            })),
            meta: None,
            audit: Default::default(),
        }
    }
}

#[async_trait]
impl Tool for ExportHostFile {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> Option<String> {
        Some("Download project file".to_string())
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(false)
                .open_world(false),
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "openai/toolInvocation/invoking": "Preparing file download",
                "openai/toolInvocation/invoked": "File ready to download"
            }))
            .expect("static file-export tool metadata must be an object"),
        )
    }

    fn description(&self) -> String {
        "Export one existing file from the active project to ChatGPT as a downloadable MCP resource. The path is project-relative. Codexify snapshots the exact bytes before returning, enforces the configured size and cache limits, and exposes only a short-lived opaque resource reference rather than a local filesystem path."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Existing file path relative to the active project root."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "name": { "type": "string" },
                "bytes": { "type": "integer" },
                "sha256": { "type": "string" },
                "mimeType": { "type": "string" },
                "expiresInMs": { "type": "integer" }
            },
            "required": [
                "path",
                "name",
                "bytes",
                "sha256",
                "mimeType",
                "expiresInMs"
            ],
            "additionalProperties": false
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::error(
            "artifact_egress_context_missing: File export requires a server request context.",
        )
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(
            args,
            config,
            &context.artifact_egress,
            &context.cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_a_resource_link_and_matching_structured_receipt() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("report.txt"), b"final report\n").unwrap();
        let config = crate::config::default_config(root.path().to_path_buf());
        let store = ArtifactEgressStore::new(config.artifact_egress.clone());
        let result = ExportHostFile
            .run(
                json!({ "path": "report.txt" }),
                &config,
                &store,
                &CancellationToken::new(),
            )
            .await;

        assert!(!result.is_error, "{}", result.joined_text());
        assert!(matches!(
            &result.content[1],
            ToolContent::ResourceLink(resource)
                if resource.name == "report.txt"
                    && resource.mime_type.as_deref() == Some("text/plain")
        ));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["path"], "report.txt");
        assert_eq!(structured["name"], "report.txt");
        assert_eq!(structured["bytes"], 13);
        assert_eq!(structured["mimeType"], "text/plain");
        assert!(structured.get("resourceUri").is_none());
    }

    #[test]
    fn schema_and_annotations_describe_read_only_file_egress() {
        let tool = ExportHostFile;
        assert_eq!(tool.input_schema()["required"], json!(["path"]));
        let annotations = tool.annotations().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(false));
    }
}
