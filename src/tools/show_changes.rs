use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::review::{
    ReviewBaseline, ReviewCheckpointManager, ReviewOwner, ReviewRequest, ReviewResult,
};
use crate::review_ui;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext};
use crate::types::{AppConfig, ToolResult};

pub struct ShowChanges;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowChangesArgs {
    since: Option<String>,
    advance: Option<bool>,
    include_patch: Option<bool>,
}

impl ShowChanges {
    pub const NAME: &'static str = "show_changes";

    fn tool_result(result: ReviewResult) -> ToolResult {
        let mut output = ToolResult::text(result.render_text());
        output.meta = Some(review_ui::result_meta(&result));
        output
    }

    fn request(args: &Value) -> Result<ReviewRequest, String> {
        let ShowChangesArgs {
            since,
            advance,
            include_patch,
        } = match serde_json::from_value(args.clone()) {
            Ok(args) => args,
            Err(error) => return Err(format!("Invalid tool arguments: {error}")),
        };
        Ok(ReviewRequest {
            since: ReviewBaseline::parse(since.as_deref())?,
            advance: advance.unwrap_or(true),
            include_patch: include_patch.unwrap_or(true),
        })
    }

    async fn run(
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        manager: &ReviewCheckpointManager,
        conversation: Option<&crate::project_bindings::ConversationIdentity>,
    ) -> ToolResult {
        let request = match Self::request(&args) {
            Ok(request) => request,
            Err(error) => return ToolResult::error(error),
        };
        let owner = match conversation {
            Some(identity) => ReviewOwner::conversation(identity),
            None => ReviewOwner::transport(session.review_state()),
        };
        match manager.show_changes(config, owner, request).await {
            Ok(result) => Self::tool_result(result),
            Err(error) => ToolResult::error(error),
        }
    }
}

#[async_trait]
impl Tool for ShowChanges {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> String {
        "Show changes".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Presents project state and updates only Codexify's private incremental-delivery cursor; it does not modify project files, Git history, user-owned data, or external systems.",
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(review_ui::tool_meta())
    }

    fn description(&self) -> String {
        "Present project-scoped working-tree changes against the immutable project-open checkpoint or the incremental last-review checkpoint. The interactive widget receives tracked, untracked, deleted, renamed, executable, symlink, and binary changes through component-only result metadata without adding the patch to model-visible structured content. By default the emitted snapshot becomes the next incremental baseline; this updates only Codexify's private review cursor and does not modify project files, Git history, or external state. Set advance=false to inspect without moving that cursor."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "since": {
                    "type": "string",
                    "enum": ["last_review", "project_open"],
                    "description": "Checkpoint to compare against. Default: last_review."
                },
                "advance": {
                    "type": "boolean",
                    "description": "Record the emitted snapshot as the next incremental baseline. This changes only Codexify's private review cursor. Default: true."
                },
                "include_patch": {
                    "type": "boolean",
                    "description": "Attach the unified binary-capable patch to component-only widget metadata when it fits review.maxPatchBytes. Default: true."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        let manager = ReviewCheckpointManager::new();
        Self::run(args, config, session, &manager, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        Self::run(
            args,
            config,
            session,
            &context.review_checkpoints,
            context.conversation.as_ref(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::ReviewSummary;

    #[test]
    fn request_defaults_to_incremental_advancing_patch_review() {
        let request = ShowChanges::request(&json!({})).unwrap();
        assert_eq!(request.since, ReviewBaseline::LastReview);
        assert!(request.advance);
        assert!(request.include_patch);
    }

    #[test]
    fn request_rejects_wrong_argument_types() {
        assert!(ShowChanges::request(&json!({ "since": 1 })).is_err());
        assert!(ShowChanges::request(&json!({ "advance": "yes" })).is_err());
        assert!(ShowChanges::request(&json!({ "include_patch": 1 })).is_err());
    }

    #[test]
    fn result_keeps_the_patch_out_of_model_visible_structured_content() {
        let result = ReviewResult {
            since: ReviewBaseline::LastReview,
            advance_requested: true,
            checkpoint_advanced: true,
            scope: ".".to_string(),
            summary: ReviewSummary {
                files: 1,
                additions: 1,
                deletions: 0,
                binary_files: 0,
            },
            files: Vec::new(),
            files_omitted: 0,
            patch: "diff --git a/file b/file\n+secret patch line\n".to_string(),
            patch_included: true,
            patch_bytes: Some(48),
            patch_omitted_reason: None,
            warnings: Vec::new(),
        };

        let output = ShowChanges::tool_result(result);
        assert!(output.structured_content.is_none());
        assert!(!output.joined_text().contains("secret patch line"));
        assert_eq!(
            output
                .meta
                .as_ref()
                .and_then(|meta| meta.get(review_ui::REVIEW_RESULT_META_KEY))
                .and_then(|payload| payload.get("patch"))
                .and_then(Value::as_str),
            Some("diff --git a/file b/file\n+secret patch line\n")
        );
    }
}
