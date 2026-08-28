use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::output_budget::{file_budget, window_file_lines};
use crate::skills::{
    MAX_SKILL_PACKAGE_FILES, SKILL_FILENAME, discover_skills, find_skill, resolve_skill_resource,
    skill_package_files, skills_enabled,
};
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct SkillsRead;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillsReadArgs {
    /// Skill name returned by skills_list.
    #[schemars(length(min = 1))]
    name: String,
    /// Package-relative resource path. Defaults to SKILL.md.
    resource: Option<String>,
    /// Zero-based first line. Defaults to 0.
    offset: Option<u64>,
    /// Maximum lines before the server's own line and byte ceilings.
    limit: Option<u64>,
}

#[async_trait]
impl Tool for SkillsRead {
    fn name(&self) -> &'static str {
        "skills_read"
    }

    fn title(&self) -> String {
        "Read skill".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads a local skill package without modifying files or external systems.",
        )
    }

    fn description(&self) -> String {
        format!(
            "Read a skill's instructions. Pass the name from skills_list and this returns its {SKILL_FILENAME}, which is the skill's body: follow it for the rest of the task. Read it completely before acting on it, and do not delegate reading or summarising it. When the body points at another file in the package — references, scripts, assets — call this again with the same name and that file's path as 'resource'; paths in a skill are relative to the skill's own directory, not to work-dir. Long files come back a window at a time; when the result says so, call again with the offset it names."
        )
    }

    fn input_schema(&self) -> Value {
        schema_for::<SkillsReadArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let SkillsReadArgs {
            name,
            resource,
            offset,
            limit,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if !skills_enabled(config) {
            return ToolResult::error("Skills are disabled by the server configuration.");
        }

        let name = name.trim();
        if name.is_empty() {
            return ToolResult::error("A skill name is required.");
        }

        let catalog = discover_skills(config);
        let skill = match find_skill(&catalog, name) {
            Some(s) => s,
            None => {
                let known = catalog
                    .skills
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let suffix = if known.is_empty() {
                    "No skills are installed.".to_string()
                } else {
                    format!("Available: {known}.")
                };
                return ToolResult::error(format!("No skill named {name}. {suffix}"));
            }
        };

        let resource = match resource.as_deref() {
            Some(r) if !r.trim().is_empty() => r.trim().to_string(),
            _ => SKILL_FILENAME.to_string(),
        };

        let path = match resolve_skill_resource(skill, &resource) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                return ToolResult::error(format!("{} has no file at {resource}.", skill.name));
            }
        };

        let lines: Vec<&str> = contents.split('\n').collect();
        let offset = offset
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
            .unwrap_or(0);
        let limit = limit.map(|value| usize::try_from(value).unwrap_or(usize::MAX));
        let window = window_file_lines(&lines, offset, limit, file_budget(config));
        let truncated = window.notice.is_some();

        let mut parts: Vec<String> = vec![
            format!("{} — {}", skill.name, path.display()),
            String::new(),
            window.lines.join("\n"),
        ];
        if let Some(notice) = &window.notice {
            parts.push(String::new());
            parts.push(notice.clone());
        }

        // Only alongside the body: once the model is reading a resource it already
        // knows the package layout, and repeating the list every call is noise.
        if resource == SKILL_FILENAME && window.notice.is_none() {
            let files = skill_package_files(skill, MAX_SKILL_PACKAGE_FILES);
            if !files.is_empty() {
                parts.push(String::new());
                parts.push(format!(
                    "Other files in this skill, readable with resource=<path>: {}",
                    files.join(", ")
                ));
            }
        }

        ToolResult::text(parts.join("\n")).with_truncation(truncated)
    }
}
