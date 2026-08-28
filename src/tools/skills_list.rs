use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::skills::{SKILL_FILENAME, SkillCatalog, discover_skills, skills_enabled};
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

/// Reproduces the TS `renderSkillList`.
pub fn render_skill_list(catalog: &SkillCatalog, enabled: bool) -> String {
    if !enabled {
        return "Skills are disabled by the server configuration. Nothing was searched."
            .to_string();
    }

    let mut lines: Vec<String> = Vec::new();

    if catalog.skills.is_empty() {
        lines.push(format!(
            "No skills found. A skill is a directory holding a {SKILL_FILENAME} whose frontmatter carries a name and a description."
        ));
        lines.push(String::new());
        lines.push("Searched:".to_string());
        for root in &catalog.roots {
            lines.push(format!(
                "- {} ({})",
                root.path.display(),
                root.scope.as_str()
            ));
        }
    } else {
        let count = catalog.skills.len();
        lines.push(format!(
            "{count} skill{} available. Read one with skills_read before acting on it — the description says when a skill applies, the body says what to do.",
            if count == 1 { "" } else { "s" }
        ));
        lines.push(String::new());
        for skill in &catalog.skills {
            lines.push(format!(
                "- {} ({}) — {}",
                skill.name,
                skill.scope.as_str(),
                skill.description
            ));
            lines.push(format!("  {}", skill.path.display()));
        }
    }

    if !catalog.warnings.is_empty() {
        lines.push(String::new());
        lines.push("Not offered:".to_string());
        for warning in &catalog.warnings {
            lines.push(format!("- {}: {}", warning.path.display(), warning.message));
        }
    }

    lines.join("\n")
}

pub struct SkillsList;

#[async_trait]
impl Tool for SkillsList {
    fn name(&self) -> &'static str {
        "skills_list"
    }

    fn title(&self) -> String {
        "List skills".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Discovers local skill metadata without modifying files or external systems.",
        )
    }

    fn description(&self) -> String {
        format!(
            "List the skills available for this project. A skill is a set of instructions stored in a {SKILL_FILENAME}, covering a task the user or the repository has already worked out how to do well. Skills are found under .agents/skills, .codex/skills and .claude/skills, in the project and in the user's home directory. Each entry gives a name and a description of when it applies; call skills_read with the name to get the instructions themselves. If the user names a skill, or the task clearly matches one of these descriptions, use it."
        )
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "skills": {
                    "type": "array",
                    "description": "The available skills, sorted by name.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Name to pass to skills_read." },
                            "description": { "type": "string", "description": "When this skill applies." },
                            "scope": { "type": "string", "description": "`repo` for a skill shipped with the project, `user` for a personal one." },
                            "path": { "type": "string", "description": format!("Absolute path of the {SKILL_FILENAME}.") }
                        },
                        "required": ["name", "description", "scope", "path"],
                        "additionalProperties": false
                    }
                },
                "content": { "type": "string", "description": "The same catalogue as readable text." }
            },
            "required": ["skills", "content"],
            "additionalProperties": false
        }))
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let enabled = skills_enabled(config);
        let catalog = discover_skills(config);
        let skills: Vec<Value> = catalog
            .skills
            .iter()
            .map(|skill| {
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "scope": skill.scope.as_str(),
                    "path": skill.path.display().to_string(),
                })
            })
            .collect();
        let text = render_skill_list(&catalog, enabled);
        ToolResult::text(text.clone()).with_structured(json!({ "skills": skills, "content": text }))
    }
}
