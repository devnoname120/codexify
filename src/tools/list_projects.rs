use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::project_catalog::{
    DEFAULT_PROJECT_LIMIT, MAX_PROJECT_LIMIT, ProjectListOutput, discover_project_catalog,
};
use crate::setup_ui;
use crate::tool::{Tool, ToolBehavior, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub struct ListProjects;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListProjectsArgs {
    query: Option<String>,
    limit: Option<u64>,
}

impl ListProjects {
    pub const NAME: &'static str = "list_projects";
}

fn render(output: &ProjectListOutput) -> String {
    let mut lines = Vec::new();
    if output.projects.is_empty() {
        lines.push(format!(
            "No selectable projects matched under the access root `{}`.",
            output.access_root
        ));
    } else {
        lines.push(format!(
            "Selectable projects (showing {} of {} matches):",
            output.projects.len(),
            output.total
        ));
        for project in &output.projects {
            let aliases = if project.aliases.is_empty() {
                String::new()
            } else {
                format!("; aliases: {}", project.aliases.join(", "))
            };
            let description = project
                .description
                .as_deref()
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {} (`{}`){aliases}{description}",
                project.name, project.selector
            ));
        }
    }
    if !output.warnings.is_empty() {
        lines.push(String::new());
        lines.push("Catalogue warnings:".to_string());
        lines.extend(output.warnings.iter().map(|warning| format!("- {warning}")));
    }
    lines.join("\n")
}

#[async_trait]
impl Tool for ListProjects {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> String {
        "List projects".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Discovers configured local project candidates without binding or modifying them.",
        )
    }

    fn meta(&self) -> Option<rmcp::model::MetaObject> {
        Some(setup_ui::app_callable_tool_meta())
    }

    fn description(&self) -> String {
        "List selectable projects before binding the current conversation. Use a query derived from the user's project name or purpose when the exact path is unknown. The result is read-only and never selects a project. Pass one unambiguous result's selector to set_project_root; when multiple candidates remain plausible, ask the user instead of guessing because project binding cannot be changed in the same conversation.".into()
    }

    fn describe(&self, config: &AppConfig) -> String {
        if config.multi_project {
            format!(
                "{} The configured access root is `{}`. Only existing directories authorized beneath that root are returned.",
                self.description(),
                config.work_dir.display()
            )
        } else {
            "Project catalogue discovery is disabled on this server. Start codexify with --multi-project or set multiProject to true to enable it.".into()
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive filter over project names, aliases, descriptions, and relative selectors"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_PROJECT_LIMIT,
                    "default": DEFAULT_PROJECT_LIMIT
                }
            },
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "access_root": { "type": "string" },
                "projects": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "selector": { "type": "string" },
                            "name": { "type": "string" },
                            "aliases": { "type": "array", "items": { "type": "string" } },
                            "description": { "type": ["string", "null"] },
                            "trust_level": {
                                "type": ["string", "null"],
                                "enum": ["trusted", "untrusted", null]
                            },
                            "sources": {
                                "type": "array",
                                "items": {
                                    "type": "string",
                                    "enum": ["codex_config", "explicit_metadata"]
                                }
                            }
                        },
                        "required": ["selector", "name", "aliases", "description", "trust_level", "sources"],
                        "additionalProperties": false
                    }
                },
                "total": { "type": "integer", "minimum": 0 },
                "warnings": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["access_root", "projects", "total", "warnings"],
            "additionalProperties": false
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let ListProjectsArgs { query, limit } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if !config.multi_project {
            return ToolResult::error(
                "Project catalogue discovery is disabled. Start codexify with `--multi-project` or set `multiProject` to true.",
            );
        }
        let limit = match limit {
            None => DEFAULT_PROJECT_LIMIT,
            Some(limit) if (1..=MAX_PROJECT_LIMIT as u64).contains(&limit) => limit as usize,
            Some(_) => {
                return ToolResult::error(format!(
                    "limit must be an integer between 1 and {MAX_PROJECT_LIMIT}"
                ));
            }
        };

        let catalog = match discover_project_catalog(config) {
            Ok(catalog) => catalog,
            Err(error) => return ToolResult::error(error),
        };
        let output = catalog.list(query.as_deref(), limit);
        let structured = serde_json::to_value(&output)
            .expect("ProjectListOutput contains only serializable fields");
        ToolResult::text(render(&output)).with_structured(structured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use crate::types::ProjectCatalogEntryConfig;

    #[tokio::test]
    async fn validates_limit_before_discovery() {
        let mut config = default_config(std::path::PathBuf::from("/missing"));
        config.multi_project = true;
        let result = ListProjects
            .call(
                json!({ "limit": MAX_PROJECT_LIMIT + 1 }),
                &config,
                &SessionState::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("between 1"));
    }

    #[tokio::test]
    async fn rejects_single_project_mode() {
        let root = tempfile::tempdir().unwrap();
        let config = default_config(root.path().to_path_buf());
        let result = ListProjects
            .call(json!({}), &config, &SessionState::new())
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("--multi-project"));
    }

    #[tokio::test]
    async fn returns_structured_selectors_before_project_selection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.multi_project = true;
        config.project_catalog.codex_config.enabled = false;
        config
            .project_catalog
            .entries
            .push(ProjectCatalogEntryConfig {
                path: Some("alpha".to_string()),
                name: Some("Alpha".to_string()),
                ..Default::default()
            });

        let result = ListProjects
            .call(json!({ "query": "alpha" }), &config, &SessionState::new())
            .await;
        assert!(!result.is_error);
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["projects"][0]["selector"], "alpha");
        assert_eq!(structured["total"], 1);
    }
}
