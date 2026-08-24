use std::fs;
use std::sync::{Arc, Barrier};

use clap::Parser;
use codexify::config::{Cli, default_config, load_config};
use codexify::exec_sessions::SessionState;
use codexify::instructions::{build_initial_instructions, build_instructions};
use codexify::memory::memory_dir;
use codexify::project_bindings::{ConversationIdentity, ProjectBindingScope, ProjectBindingStore};
use codexify::tool::Tool;
use codexify::tools::git_status::GitStatus;
use codexify::tools::list_projects::ListProjects;
use codexify::tools::read_file::ReadFile;
use codexify::tools::recall::Recall;
use codexify::tools::remember::Remember;
use codexify::tools::run_command::RunCommand;
use codexify::tools::set_project_root::SetProjectRoot;
use rmcp::model::RequestMetaObject;
use serde_json::json;
use tempfile::TempDir;

fn multi_project_config(access_root: &std::path::Path) -> codexify::types::AppConfig {
    let mut config = default_config(access_root.to_path_buf());
    config.multi_project = true;
    config.skills.enabled = Some(false);
    config
}

fn conversation_identity(session: &str) -> ConversationIdentity {
    ConversationIdentity::from_openai_session(session).unwrap()
}

#[test]
fn project_tools_require_a_selection_in_multi_project_mode() {
    let root = TempDir::new().unwrap();
    let config = multi_project_config(root.path());
    let session = SessionState::new();

    let error = session.effective_config(&config).unwrap_err();
    assert!(error.contains("set_project_root"));
    assert!(error.contains(root.path().to_string_lossy().as_ref()));
}

#[tokio::test]
async fn catalogue_selector_can_bind_an_unbound_session() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let project = access.join("codexify");
    fs::create_dir_all(&project).unwrap();
    let mut config = multi_project_config(&access);
    config.project_catalog.codex_config.enabled = false;
    config
        .project_catalog
        .entries
        .push(codexify::types::ProjectCatalogEntryConfig {
            path: Some("codexify".to_string()),
            name: Some("Codexify".to_string()),
            aliases: vec!["ChatGPT bridge".to_string()],
            description: Some("Rust MCP bridge".to_string()),
        });
    let session = SessionState::new();

    let listed = ListProjects
        .call(json!({ "query": "bridge" }), &config, &session)
        .await;
    assert!(!listed.is_error);
    let selector = listed
        .structured_content
        .as_ref()
        .and_then(|output| output["projects"][0]["selector"].as_str())
        .unwrap();
    assert_eq!(selector, "codexify");
    assert!(session.effective_config(&config).is_err());

    let selected = SetProjectRoot
        .call(json!({ "path": selector }), &config, &session)
        .await;
    assert!(!selected.is_error);
    assert_eq!(
        session.effective_config(&config).unwrap().work_dir,
        fs::canonicalize(project).unwrap()
    );
}

#[tokio::test]
async fn sessions_are_isolated_to_their_selected_roots() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let project_a = access.join("alpha");
    let project_b = access.join("beta");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();
    fs::write(project_a.join("identity.txt"), "alpha").unwrap();
    fs::write(project_b.join("identity.txt"), "beta").unwrap();

    let config = multi_project_config(&access);
    let alpha_session = SessionState::new();
    let beta_session = SessionState::new();

    let alpha_selector = SetProjectRoot;
    let beta_selector = SetProjectRoot;
    let beta_path = project_b.to_string_lossy().into_owned();
    let (alpha, beta) = tokio::join!(
        alpha_selector.call(json!({ "path": "alpha" }), &config, &alpha_session),
        beta_selector.call(json!({ "path": beta_path }), &config, &beta_session)
    );
    assert!(!alpha.is_error);
    assert!(!beta.is_error);

    let alpha_config = alpha_session.effective_config(&config).unwrap();
    let beta_config = beta_session.effective_config(&config).unwrap();
    assert_eq!(alpha_config.work_dir, fs::canonicalize(&project_a).unwrap());
    assert_eq!(beta_config.work_dir, fs::canonicalize(&project_b).unwrap());

    let alpha_reader = ReadFile;
    let beta_reader = ReadFile;
    let (alpha_file, beta_file) = tokio::join!(
        alpha_reader.call(
            json!({ "path": "identity.txt" }),
            &alpha_config,
            &alpha_session,
        ),
        beta_reader.call(
            json!({ "path": "identity.txt" }),
            &beta_config,
            &beta_session,
        )
    );
    assert_eq!(alpha_file.joined_text(), "1\talpha");
    assert_eq!(beta_file.joined_text(), "1\tbeta");

    let sibling = ReadFile
        .call(
            json!({ "path": "../beta/identity.txt" }),
            &alpha_config,
            &alpha_session,
        )
        .await;
    assert!(sibling.is_error);
    assert!(sibling.joined_text().contains("within work directory"));
}

#[tokio::test]
async fn command_and_git_tools_use_the_selected_root() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let project_a = access.join("alpha");
    let project_b = access.join("beta");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();

    for project in [&project_a, &project_b] {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(project)
            .status()
            .expect("git must be installed to run these tests");
        assert!(status.success());
    }
    fs::write(project_a.join("alpha-only.txt"), "alpha").unwrap();
    fs::write(project_b.join("beta-only.txt"), "beta").unwrap();

    let config = multi_project_config(&access);
    let session = SessionState::new();
    SetProjectRoot
        .call(json!({ "path": "alpha" }), &config, &session)
        .await;
    let selected = session.effective_config(&config).unwrap();

    let command = RunCommand
        .call(
            json!({
                "command": "git",
                "args": ["status", "--porcelain"]
            }),
            &selected,
            &session,
        )
        .await;
    assert!(!command.is_error);
    assert!(command.joined_text().contains("alpha-only.txt"));
    assert!(!command.joined_text().contains("beta-only.txt"));

    let git = GitStatus.call(json!({}), &selected, &session).await;
    assert!(!git.is_error);
    assert!(git.joined_text().contains("alpha-only.txt"));
    assert!(!git.joined_text().contains("beta-only.txt"));
}

#[tokio::test]
async fn project_selection_is_idempotent_but_cannot_switch() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    fs::create_dir_all(access.join("alpha")).unwrap();
    fs::create_dir_all(access.join("beta")).unwrap();
    let config = multi_project_config(&access);
    let session = SessionState::new();

    let first = SetProjectRoot
        .call(json!({ "path": "alpha" }), &config, &session)
        .await;
    assert!(!first.is_error);
    assert_eq!(
        first
            .structured_content
            .as_ref()
            .and_then(|value| value.get("newly_selected"))
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        first
            .structured_content
            .as_ref()
            .and_then(|value| value.get("binding_scope"))
            .and_then(|value| value.as_str()),
        Some("mcp_transport_session")
    );

    let repeated = SetProjectRoot
        .call(json!({ "path": "alpha/." }), &config, &session)
        .await;
    assert!(!repeated.is_error);
    assert_eq!(
        repeated
            .structured_content
            .as_ref()
            .and_then(|value| value.get("newly_selected"))
            .and_then(|value| value.as_bool()),
        Some(false)
    );

    let switched = SetProjectRoot
        .call(json!({ "path": "beta" }), &config, &session)
        .await;
    assert!(switched.is_error);
    assert!(switched.joined_text().contains("cannot switch"));
}

#[test]
fn openai_conversation_identity_is_read_from_request_metadata() {
    let mut meta = RequestMetaObject::new();
    assert!(ConversationIdentity::from_request_meta(&meta).is_none());

    meta.insert("openai/session".to_string(), json!("conversation-123"));
    assert!(ConversationIdentity::from_request_meta(&meta).is_some());

    meta.insert("openai/session".to_string(), json!("   "));
    assert!(ConversationIdentity::from_request_meta(&meta).is_none());
}

#[test]
fn chatgpt_project_binding_survives_transport_reconnect_and_server_restart() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let project = access.join("alpha");
    fs::create_dir_all(&project).unwrap();

    let mut config = multi_project_config(&access);
    config.memory.enabled = Some(false);
    let state_dir = root.path().join("bindings");
    let identity = conversation_identity("conversation-follow-up");

    let first_process = ProjectBindingStore::new(state_dir.clone());
    let selected = first_process
        .select_project_root(&config, &identity, "alpha")
        .unwrap();
    assert!(selected.newly_selected);
    assert_eq!(selected.scope, ProjectBindingScope::ChatGptConversation);

    let replacement_transport = SessionState::new();
    assert!(replacement_transport.effective_config(&config).is_err());

    drop(first_process);
    let restarted_process = ProjectBindingStore::new(state_dir);
    let restored = restarted_process
        .effective_config(&config, &identity)
        .unwrap();
    assert_eq!(restored.work_dir, fs::canonicalize(&project).unwrap());

    let repeated = restarted_process
        .select_project_root(&config, &identity, "alpha/.")
        .unwrap();
    assert!(!repeated.newly_selected);
}

#[test]
fn chatgpt_conversations_keep_independent_project_bindings() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let alpha = access.join("alpha");
    let beta = access.join("beta");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&beta).unwrap();

    let config = multi_project_config(&access);
    let store = ProjectBindingStore::new(root.path().join("bindings"));
    let first = conversation_identity("conversation-alpha");
    let second = conversation_identity("conversation-beta");

    store.select_project_root(&config, &first, "alpha").unwrap();
    store.select_project_root(&config, &second, "beta").unwrap();

    assert_eq!(
        store.effective_config(&config, &first).unwrap().work_dir,
        fs::canonicalize(&alpha).unwrap()
    );
    assert_eq!(
        store.effective_config(&config, &second).unwrap().work_dir,
        fs::canonicalize(&beta).unwrap()
    );
}

#[test]
fn chatgpt_conversation_cannot_switch_projects_after_restart() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    fs::create_dir_all(access.join("alpha")).unwrap();
    fs::create_dir_all(access.join("beta")).unwrap();

    let config = multi_project_config(&access);
    let state_dir = root.path().join("bindings");
    let identity = conversation_identity("immutable-conversation");
    ProjectBindingStore::new(state_dir.clone())
        .select_project_root(&config, &identity, "alpha")
        .unwrap();

    let error = ProjectBindingStore::new(state_dir)
        .select_project_root(&config, &identity, "beta")
        .unwrap_err();
    assert!(error.contains("already bound"));
    assert!(error.contains("Start a new chat"));
}

#[test]
fn concurrent_chatgpt_bindings_choose_one_project_without_overwriting() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let alpha = access.join("alpha");
    let beta = access.join("beta");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&beta).unwrap();

    let config = multi_project_config(&access);
    let state_dir = root.path().join("bindings");
    let identity = conversation_identity("concurrent-conversation");
    let barrier = Arc::new(Barrier::new(3));

    let attempts = ["alpha", "beta"].map(|project| {
        let barrier = barrier.clone();
        let config = config.clone();
        let identity = identity.clone();
        let store = ProjectBindingStore::new(state_dir.clone());
        std::thread::spawn(move || {
            barrier.wait();
            store.select_project_root(&config, &identity, project)
        })
    });
    barrier.wait();

    let results = attempts.map(|attempt| attempt.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

    let selected = ProjectBindingStore::new(state_dir)
        .effective_config(&config, &identity)
        .unwrap()
        .work_dir;
    assert!(
        selected == fs::canonicalize(alpha).unwrap() || selected == fs::canonicalize(beta).unwrap()
    );
}

#[test]
fn missing_bound_project_fails_closed_instead_of_rebinding() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let alpha = access.join("alpha");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(access.join("beta")).unwrap();

    let config = multi_project_config(&access);
    let store = ProjectBindingStore::new(root.path().join("bindings"));
    let identity = conversation_identity("missing-project-conversation");
    store
        .select_project_root(&config, &identity, "alpha")
        .unwrap();
    fs::remove_dir_all(alpha).unwrap();

    let restore_error = store.effective_config(&config, &identity).unwrap_err();
    assert!(restore_error.contains("no longer exists"));

    let rebind_error = store
        .select_project_root(&config, &identity, "beta")
        .unwrap_err();
    assert!(rebind_error.contains("no longer exists"));
}

#[test]
fn conversation_binding_is_namespaced_by_access_root_and_hides_the_session_id() {
    let root = TempDir::new().unwrap();
    let first_access = root.path().join("first");
    let second_access = root.path().join("second");
    fs::create_dir_all(first_access.join("project")).unwrap();
    fs::create_dir_all(second_access.join("project")).unwrap();

    let state_dir = root.path().join("bindings");
    let store = ProjectBindingStore::new(state_dir.clone());
    let session_id = "sensitive-conversation-identifier";
    let identity = conversation_identity(session_id);
    let first_config = multi_project_config(&first_access);
    let second_config = multi_project_config(&second_access);

    store
        .select_project_root(&first_config, &identity, "project")
        .unwrap();
    assert!(
        store
            .selected_project_root(&second_config, &identity)
            .unwrap()
            .is_none()
    );
    store
        .select_project_root(&second_config, &identity, "project")
        .unwrap();

    let mut pending = vec![state_dir];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            assert!(!path.to_string_lossy().contains(session_id));
            assert!(!fs::read_to_string(path).unwrap().contains(session_id));
        }
    }
}

#[tokio::test]
async fn selection_rejects_paths_outside_the_access_root_and_non_directories() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let outside = root.path().join("outside");
    fs::create_dir_all(&access).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(access.join("file.txt"), "not a directory").unwrap();
    let config = multi_project_config(&access);
    let session = SessionState::new();

    let relative_escape = SetProjectRoot
        .call(json!({ "path": "../outside" }), &config, &session)
        .await;
    assert!(relative_escape.is_error);

    let absolute_escape = SetProjectRoot
        .call(
            json!({ "path": outside.to_string_lossy() }),
            &config,
            &session,
        )
        .await;
    assert!(absolute_escape.is_error);

    let file = SetProjectRoot
        .call(json!({ "path": "file.txt" }), &config, &session)
        .await;
    assert!(file.is_error);
    assert!(file.joined_text().contains("not a directory"));
}

#[cfg(unix)]
#[tokio::test]
async fn selection_rejects_a_symlink_that_resolves_outside_the_access_root() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let outside = root.path().join("outside");
    fs::create_dir_all(&access).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, access.join("linked-outside")).unwrap();
    let config = multi_project_config(&access);
    let session = SessionState::new();

    let result = SetProjectRoot
        .call(json!({ "path": "linked-outside" }), &config, &session)
        .await;
    assert!(result.is_error);
    assert!(
        result
            .joined_text()
            .contains("escapes the configured access root")
    );
}

#[tokio::test]
async fn persistent_state_is_keyed_by_the_selected_root_even_with_a_custom_base_dir() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    fs::create_dir_all(access.join("alpha")).unwrap();
    fs::create_dir_all(access.join("beta")).unwrap();
    let mut config = multi_project_config(&access);
    let state_base = root.path().join("state");
    config.memory.dir = Some(state_base.to_string_lossy().into_owned());

    let alpha_session = SessionState::new();
    let beta_session = SessionState::new();
    SetProjectRoot
        .call(json!({ "path": "alpha" }), &config, &alpha_session)
        .await;
    SetProjectRoot
        .call(json!({ "path": "beta" }), &config, &beta_session)
        .await;
    let alpha_config = alpha_session.effective_config(&config).unwrap();
    let beta_config = beta_session.effective_config(&config).unwrap();

    assert_ne!(memory_dir(&alpha_config), memory_dir(&beta_config));
    assert!(memory_dir(&alpha_config).starts_with(&state_base));
    assert!(memory_dir(&beta_config).starts_with(&state_base));

    Remember
        .call(
            json!({ "key": "identity", "value": "alpha-state" }),
            &alpha_config,
            &alpha_session,
        )
        .await;
    Remember
        .call(
            json!({ "key": "identity", "value": "beta-state" }),
            &beta_config,
            &beta_session,
        )
        .await;

    let alpha = Recall
        .call(json!({}), &alpha_config, &alpha_session)
        .await
        .joined_text();
    let beta = Recall
        .call(json!({}), &beta_config, &beta_session)
        .await
        .joined_text();
    assert!(alpha.contains("alpha-state"));
    assert!(!alpha.contains("beta-state"));
    assert!(beta.contains("beta-state"));
    assert!(!beta.contains("alpha-state"));
}

#[tokio::test]
async fn initialize_instructions_defer_project_state_until_selection() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let project = access.join("alpha");
    fs::create_dir_all(&project).unwrap();
    fs::write(access.join("AGENTS.md"), "ACCESS-ROOT-INSTRUCTION").unwrap();
    fs::write(project.join("AGENTS.md"), "SELECTED-PROJECT-INSTRUCTION").unwrap();
    let mut config = multi_project_config(&access);
    config.memory.enabled = Some(false);

    let initial = build_initial_instructions(&config);
    assert!(initial.contains("list_projects"));
    assert!(initial.contains("set_project_root"));
    assert!(initial.contains("<not selected>"));
    assert!(!initial.contains("ACCESS-ROOT-INSTRUCTION"));
    assert!(!initial.contains("SELECTED-PROJECT-INSTRUCTION"));

    let session = SessionState::new();
    SetProjectRoot
        .call(json!({ "path": "alpha" }), &config, &session)
        .await;
    let selected = session.effective_config(&config).unwrap();
    let brief = build_instructions(&selected);
    assert!(brief.contains("SELECTED-PROJECT-INSTRUCTION"));
    assert!(!brief.contains("ACCESS-ROOT-INSTRUCTION"));
}

#[test]
fn multi_project_mode_can_be_enabled_by_config_or_cli() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    fs::create_dir_all(&access).unwrap();

    let enabled_config = root.path().join("enabled.json");
    fs::write(
        &enabled_config,
        r#"{ "multiProject": true, "codexMcp": { "useCli": false } }"#,
    )
    .unwrap();
    let from_file = Cli::try_parse_from([
        "codexify",
        "--work-dir",
        access.to_str().unwrap(),
        "--config",
        enabled_config.to_str().unwrap(),
    ])
    .unwrap();
    assert!(load_config(from_file).unwrap().multi_project);

    let disabled_config = root.path().join("disabled.json");
    fs::write(
        &disabled_config,
        r#"{ "multiProject": false, "codexMcp": { "useCli": false } }"#,
    )
    .unwrap();
    let from_cli = Cli::try_parse_from([
        "codexify",
        "--work-dir",
        access.to_str().unwrap(),
        "--config",
        disabled_config.to_str().unwrap(),
        "--multi-project",
    ])
    .unwrap();
    assert!(load_config(from_cli).unwrap().multi_project);
}
