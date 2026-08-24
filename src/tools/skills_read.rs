use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::output_budget::{file_budget, window_file_lines};
use crate::skills::{
    MAX_SKILL_PACKAGE_FILES, SKILL_FILENAME, discover_skills, find_skill, resolve_skill_resource,
    skill_package_files, skills_enabled,
};
use crate::tool::{Tool, arg_str, arg_u64};
use crate::types::{AppConfig, ToolResult};

pub struct SkillsRead;

#[async_trait]
impl Tool for SkillsRead {
    fn name(&self) -> &'static str {
        "skills_read"
    }

    fn description(&self) -> String {
        format!(
            "Read a skill's instructions. Pass the name from skills_list and this returns its {SKILL_FILENAME}, which is the skill's body: follow it for the rest of the task. Read it completely before acting on it, and do not delegate reading or summarising it. When the body points at another file in the package — references, scripts, assets — call this again with the same name and that file's path as 'resource'; paths in a skill are relative to the skill's own directory, not to work-dir. Long files come back a window at a time; when the result says so, call again with the offset it names."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name, as listed by skills_list." },
                "resource": {
                    "type": "string",
                    "description": format!("File to read inside the skill's directory, relative to it (e.g. 'references/api.md'). Defaults to {SKILL_FILENAME}.")
                },
                "offset": { "type": "number", "description": "Start reading from this line (0-based). Default: 0" },
                "limit": { "type": "number", "description": "Maximum number of lines to return. Capped by the server's own line and byte budget." }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The file's text, with a trailing note when it was cut short." }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        if !skills_enabled(config) {
            return ToolResult::error("Skills are disabled by the server configuration.");
        }

        let name = arg_str(&args, "name").map(|s| s.trim()).unwrap_or("");
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

        let resource = match arg_str(&args, "resource") {
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
        let offset = arg_u64(&args, "offset").unwrap_or(0) as usize;
        let limit = arg_u64(&args, "limit").map(|n| n as usize);
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
