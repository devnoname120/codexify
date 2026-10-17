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
        true,
        false,
        DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS,
    )
}

pub fn load_tools_for_mode(multi_project: bool) -> Vec<Box<dyn Tool>> {
    load_tools_with_options(
        multi_project,
        false,
        true,
        true,
        false,
        DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS,
    )
}

pub fn load_tools_for_config(config: &AppConfig) -> Vec<Box<dyn Tool>> {
    let mut tools = load_tools_with_options(
        config.multi_project,
        config.conversation_auth_token.is_some(),
        config.artifact_ingress.enabled,
        config.artifact_egress.enabled,
        config.markdown_chat.enabled,
        config.artifact_ingress.max_concurrent_downloads,
    );
    if config.ui_widgets
        && !config.conversation_auth_token.is_some()
        && !config.markdown_chat.enabled
    {
        tools.push(Box::new(tools::setup::UnrestrictedSetup));
    }
    if config.markdown_chat.enabled && config.ui_widgets {
        tools.push(Box::new(tools::markdown_chat_ui::ChatUiTool::Send));
        tools.push(Box::new(tools::markdown_chat_ui::ChatUiTool::State));
        tools.push(Box::new(tools::markdown_chat_ui::ChatUiTool::File));
        tools.push(Box::new(tools::markdown_chat_ui::ChatUiTool::Queue));
    }
    if config.ui_widgets {
        tools.push(Box::new(tools::continuation::PrepareContinuation));
    }
    if config.ui_widgets && (config.markdown_chat.enabled || config.experimental.agent_tickets) {
        tools.push(Box::new(tools::setup_ui_action::SetupUiAction::Update));
        if config.multi_project {
            tools.push(Box::new(tools::setup_ui_action::SetupUiAction::Projects));
            tools.push(Box::new(tools::setup_ui_action::SetupUiAction::Select));
        }
    }
    if config.multi_project && config.ui_widgets {
        tools.push(Box::new(tools::workspace_ui::WorkspaceUi::Switch));
        tools.push(Box::new(tools::workspace_ui::WorkspaceUi::Worktrees));
        tools.push(Box::new(tools::workspace_ui::WorkspaceUi::Reuse));
    }
    tools
}

fn load_tools_with_options(
    multi_project: bool,
    conversation_auth: bool,
    artifact_ingress_enabled: bool,
    artifact_egress_enabled: bool,
    markdown_chat_enabled: bool,
    max_concurrent_downloads: usize,
) -> Vec<Box<dyn Tool>> {
    let mut all: Vec<Box<dyn Tool>> = Vec::new();
    if markdown_chat_enabled {
        all.push(Box::new(tools::markdown_chat::ChatTool::Read));
        all.push(Box::new(tools::markdown_chat::ChatTool::Write));
        all.push(Box::new(tools::markdown_chat::ChatTool::Await));
        all.push(Box::new(tools::markdown_chat::ChatTool::Completed));
        all.push(Box::new(tools::rollout::RolloutTool::Read));
        all.push(Box::new(tools::rollout::RolloutTool::Update));
        all.push(Box::new(tools::chat_attachments::ChatAttachments::List));
        all.push(Box::new(tools::chat_attachments::ChatAttachments::Search));
        all.push(Box::new(tools::chat_attachments::ChatAttachments::Get));
        all.push(Box::new(tools::automation_pair::AutomationVerifyPairing));
        all.push(Box::new(tools::goal::GoalTool::Read));
        all.push(Box::new(tools::goal::GoalTool::Set));
        all.push(Box::new(tools::goal::GoalTool::Resume));
        all.push(Box::new(tools::goal::GoalTool::Finish));
    }
    if conversation_auth {
        all.push(Box::new(tools::setup::ConversationAuthorization));
    } else if markdown_chat_enabled {
        all.push(Box::new(tools::setup::UnrestrictedSetup));
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
    if artifact_egress_enabled {
        all.push(Box::new(tools::export_host_file::ExportHostFile));
    }
    all.extend([
        Box::new(tools::git_status::GitStatus),
        Box::new(tools::show_diff::ShowDiff),
        Box::new(tools::git_push::GitPush),
        Box::new(tools::git_commit::GitCommit),
        Box::new(tools::git_log::GitLog),
        Box::new(tools::glob::Glob),
        Box::new(tools::grep::Grep::default()),
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
        Box::new(tools::continuation::ContinueTask),
        // None of these three is a Codex tool: get_environment, get_project_doc
        // and get_agent_brief surface facts Codex sends through channels an MCP
        // server does not have.
        Box::new(tools::get_environment::GetEnvironment),
        Box::new(tools::get_project_doc::GetProjectDoc),
        Box::new(tools::get_agent_brief::GetAgentBrief),
        Box::new(tools::check_for_updates::CheckForUpdates),
        Box::new(tools::setup::SetupStatus),
        Box::new(tools::doctor::Doctor),
        Box::new(tools::self_update::SelfUpdate),
        Box::new(tools::self_update_status::SelfUpdateStatus),
        // Persistent working memory: what a chat window loses between conversations.
        Box::new(tools::remember::Remember),
        Box::new(tools::update_memory_note::UpdateMemoryNote),
        Box::new(tools::forget_memory_note::ForgetMemoryNote),
        Box::new(tools::recall::Recall),
        // Codex's skills.list / skills.read.
        Box::new(tools::skills_list::SkillsList),
        Box::new(tools::skills_read::SkillsRead),
    ] as [Box<dyn Tool>; 31]);

    let mut seen = std::collections::HashSet::new();
    for tool in &all {
        if !seen.insert(tool.name()) {
            panic!("Duplicate tool name: {}", tool.name());
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn setup_widget_is_available_without_markdown_agent() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        config.multi_project = true;
        config.ui_widgets = true;
        config.markdown_chat.enabled = false;

        let tools = load_tools_for_config(&config);
        let setup = tools
            .iter()
            .find(|tool| tool.name() == "setup")
            .expect("plain setup widget is available without Markdown Agent");
        let meta = setup.meta().expect("setup metadata");
        assert_eq!(
            meta.get("ui").and_then(|ui| ui.get("resourceUri")),
            Some(&json!(crate::setup_ui::SETUP_UI_URI))
        );
        for name in [
            "chat_read",
            "chat_write",
            "chat_await",
            "chat_attachments_list",
            "chat_attachments_search",
            "chat_attachments_get",
            "chat_ui_send",
            "chat_ui_state",
            "chat_ui_file",
        ] {
            assert!(
                !tools.iter().any(|tool| tool.name() == name),
                "{name} must remain disabled without Markdown Agent"
            );
        }
    }
}
