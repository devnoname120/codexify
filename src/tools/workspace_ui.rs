use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub enum WorkspaceUi {
    Switch,
    Worktrees,
    Reuse,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SwitchArgs {
    expected_path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktreesArgs {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ReuseArgs {
    path: String,
    worktree_path: String,
}

#[async_trait]
impl Tool for WorkspaceUi {
    fn name(&self) -> &'static str {
        match self {
            Self::Switch => "setup_ui_switch_project",
            Self::Worktrees => "setup_ui_list_worktrees",
            Self::Reuse => "setup_ui_reuse_worktree",
        }
    }
    fn title(&self) -> String {
        match self {
            Self::Switch => "Switch to another project",
            Self::Worktrees => "List project worktrees",
            Self::Reuse => "Reuse existing worktree",
        }
        .into()
    }
    fn description(&self) -> String {
        match self {
            Self::Switch => "App-only user action: return this conversation to workspace selection, preserving all previous files and worktrees. expectedPath must match the selected path shown in the card. The agent must reload its brief after the new choice.",
            Self::Worktrees => "App-only: list existing registered Git worktrees of a local project, including name, path, branch and last use recorded by Codexify. Does not select, clone or create anything.",
            Self::Reuse => "App-only user action: select a registered worktree of this local project unchanged. Does not create a worktree, checkout a branch, or discard any edits."
        }.into()
    }
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            matches!(self, Self::Worktrees),
            false,
            true,
            false,
            "Lists local workspace information or changes this conversation's private selection bookkeeping; existing project files are never deleted or moved.",
        )
    }
    fn meta(&self) -> Option<MetaObject> {
        Some(serde_json::from_value(json!({"ui":{"visibility":["app"]},"openai/visibility":"private","openai/widgetAccessible":true})).unwrap())
    }
    fn input_schema(&self) -> Value {
        let properties = match self {
            Self::Switch => json!({"expectedPath":{"type":"string","minLength":1}}),
            Self::Worktrees => json!({"path":{"type":"string","minLength":1}}),
            Self::Reuse => {
                json!({"path":{"type":"string","minLength":1},"worktreePath":{"type":"string","minLength":1}})
            }
        };
        let required = properties
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
    fn output_schema(&self) -> Option<Value> {
        Some(match self {
            Self::Switch => text_output_schema(),
            Self::Reuse => super::set_project_root::SetProjectRoot
                .output_schema()
                .unwrap(),
            Self::Worktrees => json!({"type":"object","properties":{
                "sourcePath":{"type":"string"},
                "worktrees":{"type":"array","items":{"type":"object","properties":{
                    "name":{"type":"string"},"path":{"type":"string"},"gitRoot":{"type":"string"},"branch":{"type":["string","null"]},
                    "lastUsedAtMs":{"type":["integer","null"]},"managedWorktree":{"type":"boolean"},"sourceCheckout":{"type":"boolean"}
                },"required":["name","path","gitRoot","branch","lastUsedAtMs","managedWorktree","sourceCheckout"],"additionalProperties":false}}
            },"required":["sourcePath","worktrees"],"additionalProperties":false}),
        })
    }
    fn fills_structured_content(&self) -> bool {
        matches!(self, Self::Switch)
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        ToolResult::error("Workspace UI actions require request context")
    }
    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        if !config.ui_widgets || !config.multi_project {
            return ToolResult::error("Workspace selection UI is disabled");
        }
        if context.cancellation.is_cancelled() {
            return ToolResult::error("Workspace action was cancelled");
        }
        match self {
            Self::Switch => {
                let SwitchArgs { expected_path } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                let switched = if let Some(identity) = &context.conversation {
                    context
                        .project_bindings
                        .switch_to_picker(config, identity, Path::new(&expected_path))
                        .await
                } else {
                    session
                        .switch_to_picker(config, Path::new(&expected_path))
                        .await
                };
                match switched {
                    Ok(()) => ToolResult::text(
                        "Workspace selection is open. Existing files and worktrees were preserved. Choose the new workspace in the setup card.",
                    ),
                    Err(error) => ToolResult::error(error),
                }
            }
            Self::Worktrees => {
                let WorktreesArgs { path } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                let (_, source) = match crate::project_bindings::resolve_project_root(config, &path)
                {
                    Ok(value) => value,
                    Err(error) => return ToolResult::error(error),
                };
                match crate::worktrees::list_existing_worktrees(config, &source).await {
                    Ok(rows) => ToolResult::text(
                        "Available worktrees; listing does not select a workspace.",
                    )
                    .with_structured(json!({"sourcePath":source,"worktrees":rows})),
                    Err(error) => ToolResult::error(error),
                }
            }
            Self::Reuse => {
                let ReuseArgs {
                    path,
                    worktree_path,
                } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                let selected = if let Some(identity) = &context.conversation {
                    context
                        .project_bindings
                        .reuse_worktree(config, identity, &path, Path::new(&worktree_path))
                        .await
                } else {
                    session
                        .reuse_worktree(config, &path, Path::new(&worktree_path))
                        .await
                };
                match selected {
                    Ok(selection) => super::set_project_root::render_project_selection(selection),
                    Err(error) => ToolResult::error(error),
                }
            }
        }
    }
}
