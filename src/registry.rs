//! The tool registry. Ports `src/registry.ts`.
//!
//! `load_tools` returns the single-project registry; `load_tools_for_mode` adds
//! the session selector only when multi-project mode is enabled. Both enforce
//! unique names, panicking on a duplicate (a programming error, as in the TS
//! which throws at startup). The common order mirrors the TypeScript registry.

use crate::tool::Tool;
use crate::tools;
use crate::types::{AppConfig, DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS};

pub fn load_tools() -> Vec<Box<dyn Tool>> {
    load_tools_with_options(
        false,
        false,
        true,
        DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS,
    )
}

pub fn load_tools_for_mode(multi_project: bool) -> Vec<Box<dyn Tool>> {
    load_tools_with_options(
        multi_project,
        false,
        true,
        DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS,
    )
}

pub fn load_tools_for_config(config: &AppConfig) -> Vec<Box<dyn Tool>> {
    load_tools_with_options(
        config.multi_project,
        config.conversation_auth_token.is_some(),
        config.artifact_ingress.enabled,
        config.artifact_ingress.max_concurrent_downloads,
    )
}

fn load_tools_with_options(
    multi_project: bool,
    conversation_auth: bool,
    artifact_ingress_enabled: bool,
    max_concurrent_downloads: usize,
) -> Vec<Box<dyn Tool>> {
    let mut all: Vec<Box<dyn Tool>> = Vec::new();
    if conversation_auth {
        all.push(Box::new(tools::authenticate::Authenticate));
    }
    if multi_project {
        all.push(Box::new(tools::list_projects::ListProjects));
        all.push(Box::new(tools::set_project_root::SetProjectRoot));
    }
    all.push(Box::new(tools::read_file::ReadFile));
    all.push(Box::new(tools::write_file::WriteFile));
    if artifact_ingress_enabled {
        all.push(Box::new(tools::import_host_file::ImportHostFile::new(
            max_concurrent_downloads,
        )));
    }
    all.extend([
        Box::new(tools::run_command::RunCommand),
        Box::new(tools::git_status::GitStatus),
        Box::new(tools::show_changes::ShowChanges),
        Box::new(tools::git_push::GitPush),
        Box::new(tools::git_commit::GitCommit),
        Box::new(tools::git_log::GitLog),
        Box::new(tools::glob::Glob),
        Box::new(tools::grep::Grep),
        Box::new(tools::list_directory::ListDirectory),
        Box::new(tools::tree::Tree),
        // Ported from Codex (codex-rs/core/src/tools). Names use underscores
        // because MCP tool names must match ^[a-zA-Z0-9_-]{1,64}$.
        Box::new(tools::apply_patch::ApplyPatch),
        Box::new(tools::exec_command::ExecCommand),
        Box::new(tools::write_stdin::WriteStdin),
        Box::new(tools::view_image::ViewImage),
        Box::new(tools::update_plan::UpdatePlan),
        Box::new(tools::clock_curr_time::ClockCurrTime),
        Box::new(tools::clock_sleep::ClockSleep),
        // None of these three is a Codex tool: get_environment, get_project_doc
        // and get_agent_brief surface facts Codex sends through channels an MCP
        // server does not have.
        Box::new(tools::get_environment::GetEnvironment),
        Box::new(tools::get_project_doc::GetProjectDoc),
        Box::new(tools::get_agent_brief::GetAgentBrief),
        // Persistent working memory: what a chat window loses between conversations.
        Box::new(tools::remember::Remember),
        Box::new(tools::recall::Recall),
        // Codex's skills.list / skills.read.
        Box::new(tools::skills_list::SkillsList),
        Box::new(tools::skills_read::SkillsRead),
    ] as [Box<dyn Tool>; 24]);

    let mut seen = std::collections::HashSet::new();
    for tool in &all {
        if !seen.insert(tool.name()) {
            panic!("Duplicate tool name: {}", tool.name());
        }
    }
    all
}
