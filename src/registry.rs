//! The tool registry. Ports `src/registry.ts`.
//!
//! Workspace selectors are always included. The legacy mode helper ignores its
//! compatibility argument. All registries enforce
//! unique names, panicking on a duplicate (a programming error, as in the TS
//! which throws at startup). The common order mirrors the TypeScript registry.

use crate::tool::Tool;
use crate::tools;
use crate::types::{AppConfig, ArtifactEgressConfig, DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS};

pub fn load_tools() -> Vec<Box<dyn Tool>> {
    load_tools_with_options(
        false,
        true,
        ArtifactEgressConfig::default().enabled,
        true,
        true,
        DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS,
    )
}

pub fn load_tools_for_mode(_legacy_mode: bool) -> Vec<Box<dyn Tool>> {
    load_tools()
}

pub fn load_tools_for_config(config: &AppConfig) -> Vec<Box<dyn Tool>> {
    let mut tools = load_tools_with_options(
        config.conversation_auth_token.is_some(),
        config.artifact_ingress.enabled,
        config.artifact_egress.enabled,
        config.markdown_chat.enabled,
        config.ui_widgets,
        config.artifact_ingress.max_concurrent_downloads,
    );
    if config.markdown_chat.enabled
        && config.markdown_chat.automation.enabled
        && config.markdown_chat.features.agent_teams
    {
        tools.push(Box::new(tools::agents::AgentTool::Spawn));
        tools.push(Box::new(tools::agents::AgentTool::Status));
        tools.push(Box::new(tools::agents::AgentTool::Report));
        tools.push(Box::new(tools::agents::AgentTool::Control));
        tools.push(Box::new(tools::agents::AgentTool::Message));
        if config.markdown_chat.features.external_agent_control {
            tools.push(Box::new(tools::agent_client::AgentClient));
        }
    }
    if config.experimental.agent_tickets {
        tools.push(Box::new(tools::ticket_wait::TicketWait));
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
    if config.ui_widgets {
        tools.push(Box::new(tools::setup_ui_action::SetupUiAction::Update));
        {
            tools.push(Box::new(tools::setup_ui_action::SetupUiAction::Projects));
            tools.push(Box::new(tools::setup_ui_action::SetupUiAction::Select));
        }
    }
    if config.ui_widgets {
        tools.push(Box::new(tools::workspace_ui::WorkspaceUi::Switch));
        tools.push(Box::new(tools::workspace_ui::WorkspaceUi::Worktrees));
        tools.push(Box::new(tools::workspace_ui::WorkspaceUi::Reuse));
    }
    tools
}

fn load_tools_with_options(
    conversation_auth: bool,
    artifact_ingress_enabled: bool,
    artifact_egress_enabled: bool,
    markdown_chat_enabled: bool,
    ui_widgets: bool,
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
    } else if markdown_chat_enabled || ui_widgets {
        all.push(Box::new(tools::setup::UnrestrictedSetup));
    }
    all.push(Box::new(tools::list_projects::ListProjects));
    all.push(Box::new(tools::set_project_root::SetProjectRoot));
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
    fn agent_tools_require_explicit_settings_opt_in() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        for enabled in [false, true, false] {
            config.markdown_chat.features.agent_teams = enabled;
            let tools = load_tools_for_config(&config);
            for name in [
                "agent_spawn",
                "agent_status",
                "agent_report",
                "agent_control",
                "agent_message",
            ] {
                assert_eq!(
                    tools.iter().any(|tool| tool.name() == name),
                    enabled,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn coding_app_tool_requires_all_feature_dependencies() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        assert!(!config.markdown_chat.features.external_agent_control);
        for chats in [false, true] {
            for browser in [false, true] {
                for teams in [false, true] {
                    for clients in [false, true] {
                        config.markdown_chat.enabled = chats;
                        config.markdown_chat.automation.enabled = browser;
                        config.markdown_chat.features.agent_teams = teams;
                        config.markdown_chat.features.external_agent_control = clients;
                        assert_eq!(
                            load_tools_for_config(&config)
                                .iter()
                                .any(|tool| tool.name() == "agent_client"),
                            chats && browser && teams && clients
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn workspace_picker_is_independent_of_agent_chat_and_preserves_authorization() {
        let root = tempfile::tempdir().unwrap();
        for chat in [false, true] {
            for widgets in [false, true] {
                for auth in [false, true] {
                    let mut config = crate::config::default_config(root.path().into());
                    config.markdown_chat.enabled = chat;
                    config.ui_widgets = widgets;
                    config.conversation_auth_token = auth.then(|| "a".repeat(64).into());
                    let tools = load_tools_for_config(&config);
                    let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
                    assert_eq!(
                        names.len(),
                        names.iter().collect::<std::collections::HashSet<_>>().len()
                    );
                    let setup = tools.iter().find(|tool| tool.name() == "setup");
                    assert_eq!(setup.is_some(), auth || chat || widgets);
                    if let Some(setup) = setup {
                        assert_eq!(
                            setup.input_schema()["properties"].get("ref").is_some(),
                            auth
                        );
                        assert_eq!(
                            setup.meta().unwrap().get("ui").unwrap()["resourceUri"],
                            json!(crate::setup_ui::SETUP_UI_URI)
                        );
                        if !auth && !chat {
                            assert!(!setup.describe(&config).contains("chat_await"));
                        }
                    }
                    for name in [
                        "setup_ui_list_projects",
                        "setup_ui_select_project",
                        "setup_ui_update",
                    ] {
                        let tool = tools.iter().find(|tool| tool.name() == name);
                        assert_eq!(tool.is_some(), widgets);
                        if let Some(tool) = tool {
                            assert_eq!(
                                tool.meta().unwrap().get("ui").unwrap()["visibility"],
                                json!(["app"])
                            );
                        }
                    }
                    if !chat {
                        assert!(!names.iter().any(|name| name.starts_with("chat_")
                            || name.starts_with("goal_")
                            || name.starts_with("rollout_")
                            || *name == "automation_verify_pairing"));
                    }
                }
            }
        }
    }
}
