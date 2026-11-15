use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

fn default_config(root: std::path::PathBuf) -> codexify::types::AppConfig {
    let mut config = codexify::config::default_config(root);
    config.set_filesystem_worker_executable(env!("CARGO_BIN_EXE_codexify").into());
    config
}
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::{MarkdownChatStore, NotificationState, WaitOutcome};
use codexify::project_bindings::ConversationIdentity;
use codexify::registry::load_tools_for_config;
use codexify::tool::ToolRequestContext;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fixture() -> (
    tempfile::TempDir,
    codexify::types::AppConfig,
    SessionState,
    ToolRequestContext,
) {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().canonicalize().unwrap());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.max_wait_ms = 1000;
    let conversation = ConversationIdentity::from_openai_session("tool-test");
    let context = ToolRequestContext {
        conversation: conversation.clone(),
        task_conversation: conversation,
        task_owner_generation: Some(0),
        conversation_retired: false,
        connector_schema_version: None,
        conversation_schema_version: None,
        conversation_authorizations: Arc::new(
            codexify::conversation_auth::ConversationAuthorizationStore::new(),
        ),
        project_bindings: Arc::new(codexify::project_bindings::ProjectBindingStore::new(
            root.path().join("bindings"),
        )),
        diff_checkpoints: Arc::new(codexify::diff::DiffCheckpointManager::new()),
        artifact_egress: Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            root.path().join("artifacts"),
        )),
        markdown_chat: Arc::new(MarkdownChatStore::default()),
        agent_automation: None,
        agent_runtime: None,
        cancellation: CancellationToken::new(),
    };
    (root, config, SessionState::new(), context)
}

fn widget_args(
    mut value: serde_json::Value,
    config: &codexify::types::AppConfig,
    session: &SessionState,
    context: &ToolRequestContext,
) -> serde_json::Value {
    let chat = context
        .markdown_chat
        .chat(config, context.task_conversation.as_ref(), session)
        .unwrap();
    value["expected_chat_file"] = json!(chat.path());
    value
}
async fn select_fixture_workspace(
    config: &codexify::types::AppConfig,
    context: &ToolRequestContext,
) {
    let mut config = config.clone();
    config.worktrees.mode = codexify::types::WorktreeMode::Never;
    context
        .project_bindings
        .select_project_root(
            &config,
            context.conversation.as_ref().unwrap(),
            &config.work_dir.to_string_lossy(),
        )
        .await
        .unwrap();
}

fn append(path: &std::path::Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

#[tokio::test]
async fn attachment_tools_list_search_read_and_isolate_the_current_conversation() {
    use codexify::markdown_chat::attachments::AttachmentStore;
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.task_conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let store = AttachmentStore::for_chat(chat.path()).unwrap();
    let file = store
        .upload(
            "tool-upload",
            "Notes.TXT",
            "text/plain",
            Some(1000),
            false,
            b"Attachment text\nSecond line".to_vec(),
        )
        .unwrap();
    let tools = load_tools_for_config(&config);
    let list = tools
        .iter()
        .find(|t| t.name() == "chat_list_attachments")
        .unwrap();
    let search = tools
        .iter()
        .find(|t| t.name() == "chat_search_attachments")
        .unwrap();
    let get = tools
        .iter()
        .find(|t| t.name() == "chat_get_attachment")
        .unwrap();
    let draft = list
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!draft.is_error);
    assert_eq!(draft.structured_content.unwrap()["total"], 0);
    chat.append_user_with_attachments(
        "tool-prompt".into(),
        "Read the notes".into(),
        vec![file.id.clone()],
    )
    .await
    .unwrap();
    let listed = list
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!listed.is_error);
    let listed = listed.structured_content.unwrap();
    assert_eq!(listed["attachments"][0]["originalName"], "Notes.TXT");
    assert_eq!(listed["attachments"][0]["path"], file.path);
    let found = search
        .call_with_context(json!({"query":"notes"}), &config, &session, &context)
        .await;
    assert_eq!(found.structured_content.unwrap()["total"], 1);
    let found = search
        .call_with_context(json!({"query":"missing"}), &config, &session, &context)
        .await;
    assert_eq!(found.structured_content.unwrap()["total"], 0);
    let read = get
        .call_with_context(
            json!({"attachment_id":file.id}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!read.is_error);
    let data = read.structured_content.unwrap();
    assert_eq!(data["content"], "Attachment text\nSecond line");
    assert!(data["resource_uri"].is_null());
    for (tool, value) in [(list, &listed), (get, &data)] {
        assert!(
            jsonschema::is_valid(&tool.output_schema().unwrap(), value),
            "attachment output must satisfy its public schema"
        );
    }
    let mut other = context.clone();
    other.task_conversation = ConversationIdentity::from_openai_session("other-attachments");
    let denied = get
        .call_with_context(json!({"attachment_id":file.id}), &config, &session, &other)
        .await;
    assert!(denied.is_error);
    let denied = get
        .call_with_context(
            json!({"attachment_id":"../../secret"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(denied.is_error);
    assert_eq!(
        chat.unread_snapshot().await.unwrap().messages.len(),
        1,
        "listing and reading files must not acknowledge the prompt"
    );
}

#[tokio::test]
async fn attachment_inline_content_survives_with_file_materialization_disabled() {
    use codexify::markdown_chat::attachments::AttachmentStore;
    use codexify::types::ToolContent;
    let (root, mut config, session, mut context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.task_conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let store = AttachmentStore::for_chat(chat.path()).unwrap();
    let mut image = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
    let prefix = "a".repeat(65_535);
    let cases = [
        (
            "small.txt",
            "text/plain",
            b"Small attachment".to_vec(),
            Some("Small attachment"),
            false,
            false,
        ),
        (
            "large.txt",
            "text/plain",
            format!("{prefix}λ{}", "b".repeat(1024)).into_bytes(),
            Some(prefix.as_str()),
            true,
            false,
        ),
        (
            "binary.bin",
            "application/octet-stream",
            vec![0, 1, 255],
            None,
            false,
            false,
        ),
        (
            "large.bin",
            "application/octet-stream",
            vec![255; 70_000],
            None,
            true,
            false,
        ),
        (
            "image.png",
            "image/png",
            image.into_inner(),
            None,
            false,
            true,
        ),
    ];
    for enabled in [false, true] {
        config.artifact_egress.enabled = enabled;
        context.artifact_egress = Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            root.path().join(format!("artifacts-{enabled}")),
        ));
        let tools = load_tools_for_config(&config);
        let get = tools
            .iter()
            .find(|tool| tool.name() == "chat_get_attachment")
            .unwrap();
        for (name, mime, bytes, text, truncated, is_image) in &cases {
            let request = format!("{enabled}-{}", name.replace('.', "-"));
            let file = store
                .upload(&request, name, mime, None, false, bytes.clone())
                .unwrap();
            chat.append_user_with_attachments(
                request,
                "Inspect the attachment".into(),
                vec![file.id.clone()],
            )
            .await
            .unwrap();
            let result = get
                .call_with_context(
                    json!({"attachment_id":file.id}),
                    &config,
                    &session,
                    &context,
                )
                .await;
            assert!(!result.is_error, "{name}: {}", result.joined_text());
            let data = result.structured_content.as_ref().unwrap();
            assert_eq!(data["content"], json!(text), "{name}");
            assert_eq!(data["content_truncated"], *truncated, "{name}");
            assert!(data["preview_error"].is_null(), "{name}");
            assert_eq!(data["resource_uri"].is_string(), enabled, "{name}");
            assert_eq!(
                result
                    .content
                    .iter()
                    .any(|block| matches!(block, ToolContent::ResourceLink(_))),
                enabled,
                "{name}"
            );
            assert_eq!(
                result
                    .content
                    .iter()
                    .any(|block| matches!(block, ToolContent::Image { .. })),
                *is_image,
                "{name}"
            );
            assert_eq!(std::fs::read(&file.path).unwrap(), *bytes, "{name}");
        }
    }
}

#[test]
fn tools_are_opt_in_and_read_wait_have_no_parameters() {
    let (_root, mut config, _session, _context) = fixture();
    let enabled = load_tools_for_config(&config);
    for name in [
        "chat_read",
        "chat_write",
        "chat_await",
        "turn_completed",
        "rollout_read",
    ] {
        let tool = enabled.iter().find(|tool| tool.name() == name).unwrap();
        if name != "chat_write" {
            assert_eq!(tool.input_schema()["properties"], json!({}));
            assert!(!jsonschema::is_valid(
                &tool.input_schema(),
                &json!({"maxWaitMs":1})
            ));
        }
    }
    config.markdown_chat.enabled = false;
    assert!(
        load_tools_for_config(&config)
            .iter()
            .all(|tool| !tool.name().starts_with("chat_")
                && !["turn_completed", "rollout_read", "rollout_update"].contains(&tool.name()))
    );
}

#[test]
fn chat_tool_descriptions_define_a_non_terminal_state_machine() {
    let (_root, config, _session, _context) = fixture();
    let tools = load_tools_for_config(&config);
    let reader = tools
        .iter()
        .find(|tool| tool.name() == "chat_read")
        .unwrap();
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let waiter = tools
        .iter()
        .find(|tool| tool.name() == "chat_await")
        .unwrap();

    let read_description = reader.description();
    assert!(read_description.contains("NON-TERMINAL TOOL"));
    assert!(read_description.contains("call turn_completed"));

    let write_description = writer.description();
    assert!(write_description.contains("NON-TERMINAL TOOL"));
    assert!(write_description.contains("MUST NOT end the assistant turn"));
    assert!(write_description.contains("new_chat_message_from_user"));
    assert!(write_description.contains("chat_await"));

    let await_description = waiter.description();
    assert!(await_description.contains("does not complete the request or release queued requests"));
    assert!(await_description.contains("Never substitute a normal assistant final response"));
}

#[tokio::test]
async fn chat_await_honors_configured_timeout_with_and_without_a_workspace() {
    for unselected in [false, true] {
        for max_wait_ms in [1_000, 1_500] {
            let (_root, mut config, session, context) = fixture();

            if !unselected {
                select_fixture_workspace(&config, &context).await;
            }
            config.markdown_chat.max_wait_ms = max_wait_ms;
            let tools = load_tools_for_config(&config);
            let waiter = tools
                .iter()
                .find(|tool| tool.name() == "chat_await")
                .unwrap();
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                waiter.call_with_context(json!({}), &config, &session, &context),
            )
            .await
            .expect("chat_await ignored the configured timeout");
            assert!(!result.is_error, "{}", result.joined_text());
            assert_eq!(
                result.structured_content.as_ref().unwrap()["status"],
                "timeout"
            );
            assert!(started.elapsed() >= Duration::from_millis(max_wait_ms));
        }
    }
}

#[tokio::test]
async fn chat_tool_results_expose_the_required_next_action() {
    let (_root, mut config, session, context) = fixture();
    config.markdown_chat.max_wait_ms = 1;
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let tools = load_tools_for_config(&config);
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let reader = tools
        .iter()
        .find(|tool| tool.name() == "chat_read")
        .unwrap();
    let waiter = tools
        .iter()
        .find(|tool| tool.name() == "chat_await")
        .unwrap();

    let written = writer
        .call_with_context(
            json!({"message":"Progress update"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert_eq!(
        written.structured_content.as_ref().unwrap()["required_next_action"],
        "continue_or_turn_completed"
    );
    assert_eq!(
        written.structured_content.as_ref().unwrap()["assistant_turn_may_end"],
        false
    );

    append(chat.path(), "Interrupting user message\n");
    let interrupted = writer
        .call_with_context(
            json!({"message":"Another update"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(interrupted.new_chat_message_from_user.is_some());
    assert_eq!(
        interrupted.structured_content.as_ref().unwrap()["required_next_action"],
        "chat_write"
    );
    assert_eq!(
        interrupted.structured_content.as_ref().unwrap()["assistant_turn_may_end"],
        false
    );

    let empty = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert_eq!(
        empty.structured_content.as_ref().unwrap()["required_next_action"],
        "continue_or_turn_completed"
    );

    append(chat.path(), "Read me\n");
    let message = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert_eq!(
        message.structured_content.as_ref().unwrap()["required_next_action"],
        "chat_write"
    );

    let timeout = waiter
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert_eq!(
        timeout.structured_content.as_ref().unwrap()["status"],
        "timeout"
    );
    assert_eq!(
        timeout.structured_content.as_ref().unwrap()["required_next_action"],
        "chat_await"
    );
    assert_eq!(
        timeout.structured_content.as_ref().unwrap()["assistant_turn_may_end"],
        false
    );
}

#[tokio::test]
async fn chat_file_links_resolve_real_exports_and_reject_ambiguous_or_foreign_files() {
    let (root, mut config, session, mut context) = fixture();
    config.artifact_egress.enabled = true;
    context.artifact_egress = Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
        config.artifact_egress.clone(),
        root.path().join("artifacts"),
    ));
    std::fs::create_dir(root.path().join("reports")).unwrap();
    std::fs::write(
        root.path().join("reports/report one.txt"),
        "original contents",
    )
    .unwrap();
    let exported = context
        .artifact_egress
        .export_project_file(
            &config.work_dir,
            "reports/report one.txt",
            &context.cancellation,
        )
        .await
        .unwrap();
    let tools = load_tools_for_config(&config);
    let file_tool = tools
        .iter()
        .find(|tool| tool.name() == "chat_ui_file")
        .expect("app-only file resolver");
    assert_eq!(
        file_tool.meta().unwrap().get("openai/visibility"),
        Some(&json!("private"))
    );
    for href in [
        "sandbox:/mnt/data/report%20one.txt",
        exported.resource.uri.as_str(),
        "reports/report%20one.txt",
    ] {
        let result = file_tool
            .call_with_context(
                widget_args(json!({"href":href}), &config, &session, &context),
                &config,
                &session,
                &context,
            )
            .await;
        assert!(!result.is_error, "{href}: {}", result.joined_text());
        let file = &result
            .meta
            .as_ref()
            .unwrap()
            .get(codexify::markdown_chat_ui::CHAT_WIDGET_META)
            .unwrap()["file"];
        assert_eq!(file["type"], "resource_link");
        assert_eq!(file["name"], "report one.txt");
        let contents = context
            .artifact_egress
            .read_resource(file["uri"].as_str().unwrap(), &context.cancellation)
            .await
            .unwrap()
            .unwrap();
        let contents = serde_json::to_value(contents).unwrap();
        use base64::Engine;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(contents["blob"].as_str().unwrap())
                .unwrap(),
            b"original contents"
        );
    }
    std::fs::write(root.path().join("reports/part#1.txt"), "encoded filename").unwrap();
    let encoded = file_tool
        .call_with_context(
            widget_args(
                json!({"href":"reports/part%231.txt#section"}),
                &config,
                &session,
                &context,
            ),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!encoded.is_error, "{}", encoded.joined_text());
    for href in [
        "sandbox:/mnt/data/not-exported.txt",
        "sandbox:/etc/passwd",
        "sandbox:/mnt/data/../secret",
        "../secret",
        "%2e%2e/secret",
        "/etc/passwd",
        "file:///etc/passwd",
        "https://example.com/data",
    ] {
        assert!(
            file_tool
                .call_with_context(
                    widget_args(json!({"href":href}), &config, &session, &context),
                    &config,
                    &session,
                    &context
                )
                .await
                .is_error,
            "{href}"
        );
    }
    std::fs::write(root.path().join("report one.txt"), "different file").unwrap();
    context
        .artifact_egress
        .export_project_file(&config.work_dir, "report one.txt", &context.cancellation)
        .await
        .unwrap();
    let ambiguous = file_tool
        .call_with_context(
            widget_args(
                json!({"href":"sandbox:/mnt/data/report%20one.txt"}),
                &config,
                &session,
                &context,
            ),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(ambiguous.is_error);
    assert!(ambiguous.joined_text().contains("ambiguous"));
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("foreign.txt"), "private").unwrap();
    let foreign = context
        .artifact_egress
        .export_project_file(other.path(), "foreign.txt", &context.cancellation)
        .await
        .unwrap();
    assert!(
        file_tool
            .call_with_context(
                widget_args(
                    json!({"href":foreign.resource.uri}),
                    &config,
                    &session,
                    &context
                ),
                &config,
                &session,
                &context
            )
            .await
            .is_error
    );
}

#[tokio::test]
async fn apprise_runtime_failure_does_not_undo_or_duplicate_a_chat_write() {
    let (root, mut config, session, context) = fixture();
    config.markdown_chat.notifications = Some(
        serde_json::from_value(json!({
            "urls":["ntfys://test-topic"],"pythonPath":root.path().join("missing-python")
        }))
        .unwrap(),
    );
    let result = load_tools_for_config(&config)
        .into_iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap()
        .call_with_context(
            json!({"message":"Persist even when delivery fails"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap()["notification"], "failed");
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let history = chat.widget_page(None, None).await.unwrap();
    assert_eq!(history.messages.len(), 1);
    assert_eq!(
        history.messages[0].markdown,
        "Persist even when delivery fails"
    );
}

#[test]
fn chat_widget_tools_are_app_only_and_chat_calls_never_link_a_widget() {
    let (_root, mut config, _session, _context) = fixture();
    let tools = load_tools_for_config(&config);
    for name in ["chat_ui_send", "chat_ui_state", "chat_ui_queue"] {
        let tool = tools
            .iter()
            .find(|tool| tool.name() == name)
            .unwrap_or_else(|| panic!("missing app-only tool {name}"));
        let meta = tool.meta().unwrap();
        assert_eq!(meta.get("ui").unwrap()["visibility"], json!(["app"]));
        assert_eq!(meta.get("openai/visibility"), Some(&json!("private")));
        assert!(tool.input_schema()["properties"].get("path").is_none());
    }
    for name in [
        "chat_read",
        "chat_write",
        "chat_await",
        "turn_completed",
        "rollout_read",
    ] {
        let tool = tools.iter().find(|tool| tool.name() == name).unwrap();
        let meta = serde_json::to_value(tool.meta()).unwrap();
        assert!(meta.get("openai/outputTemplate").is_none());
        assert!(meta["ui"].get("resourceUri").is_none());
    }
    config.ui_widgets = false;
    assert!(
        load_tools_for_config(&config)
            .iter()
            .all(|tool| !tool.name().starts_with("chat_ui_"))
    );
}

#[tokio::test]
async fn widget_send_is_idempotent_and_history_never_consumes_or_marks_delivery() {
    let (_root, mut config, session, context) = fixture();
    config.markdown_chat.default_message_mode = codexify::markdown_chat::MessageMode::Steer;
    let tools = load_tools_for_config(&config);
    let sender = tools
        .iter()
        .find(|tool| tool.name() == "chat_ui_send")
        .unwrap();
    let reader = tools
        .iter()
        .find(|tool| tool.name() == "chat_ui_state")
        .unwrap();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let markdown = "## User request\n\n**Full text**\n```rust\nlet x = 1;\n```\n";
    let args = widget_args(
        json!({"request_id":"test-request-1", "message":markdown}),
        &config,
        &session,
        &context,
    );
    let first = sender
        .call_with_context(args.clone(), &config, &session, &context)
        .await;
    assert!(!first.is_error, "{}", first.joined_text());
    assert!(first.audit.sensitive_output);
    let receipt = first
        .meta
        .unwrap()
        .get(codexify::markdown_chat_ui::CHAT_WIDGET_META)
        .unwrap()
        .clone();
    let length = std::fs::metadata(chat.path()).unwrap().len();
    let repeated = sender
        .call_with_context(args, &config, &session, &context)
        .await;
    assert!(!repeated.is_error);
    assert_eq!(std::fs::metadata(chat.path()).unwrap().len(), length);
    assert_eq!(receipt["sent"]["end"], length);
    let expected = format!(
        "[{}]\n{markdown}\n\n",
        codexify::markdown_chat::attachments::date(
            receipt["sent"]["created_at_ms"].as_u64().unwrap()
        )
    );
    assert_eq!(chat.read(false).await.unwrap().text, expected);
    let page = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!page.is_error);
    let metadata = page.meta.unwrap();
    let page = metadata
        .get(codexify::markdown_chat_ui::CHAT_WIDGET_META)
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["messages"][0]["markdown"], markdown);
    assert_eq!(page["delivered_through"], 0);
    assert_eq!(chat.read(false).await.unwrap().text, expected);
    let other_conversation = ConversationIdentity::from_openai_session("other-widget");
    let other = ToolRequestContext {
        conversation: other_conversation.clone(),
        task_conversation: other_conversation,
        conversation_retired: false,
        ..context.clone()
    };
    let page = reader
        .call_with_context(json!({}), &config, &session, &other)
        .await;
    assert_eq!(
        page.meta
            .unwrap()
            .get(codexify::markdown_chat_ui::CHAT_WIDGET_META)
            .unwrap()["messages"],
        json!([])
    );
    let conflict = sender
        .call_with_context(
            widget_args(
                json!({"request_id":"test-request-1", "message":"changed"}),
                &config,
                &session,
                &context,
            ),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(conflict.is_error);
}

#[tokio::test]
async fn delivery_receipt_is_persistent_but_independent_of_acknowledgement() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let receipt = chat
        .append_user("persistent-message".into(), "Keep this pending".into())
        .await
        .unwrap();
    chat.mark_delivered(receipt.end).await.unwrap();
    let store = MarkdownChatStore::default();
    let reopened = store
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let page = reopened.widget_page(None, None).await.unwrap();
    assert_eq!(page.delivered_through, receipt.end);
    assert!(
        reopened
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Keep this pending")
    );
    let next = reopened
        .append_user("later-message".into(), "Not delivered yet".into())
        .await
        .unwrap();
    let page = reopened.widget_page(None, None).await.unwrap();
    assert!(page.delivered_through < next.end);
    let unchanged = reopened
        .widget_page(None, Some(page.revision))
        .await
        .unwrap();
    assert!(unchanged.unchanged);
    assert!(unchanged.messages.is_empty());
}

#[tokio::test]
async fn widget_read_receipt_advances_only_when_a_chat_tool_consumes_the_message() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let sent = chat
        .append_user("read-receipt".into(), "Please read this".into())
        .await
        .unwrap();
    let initial = serde_json::to_value(chat.widget_page(None, None).await.unwrap()).unwrap();
    assert!(
        initial["read_through"]
            .as_u64()
            .expect("read cursor is exposed")
            < sent.end
    );
    chat.mark_delivered(sent.end).await.unwrap();
    let delivered = serde_json::to_value(chat.widget_page(None, None).await.unwrap()).unwrap();
    assert_eq!(delivered["delivered_through"], sent.end);
    assert_eq!(delivered["read_through"], initial["read_through"]);
    chat.read(false).await.unwrap();
    let reader = crate::load_tools_for_config(&config)
        .into_iter()
        .find(|tool| tool.name() == "chat_read")
        .unwrap();
    let result = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!result.is_error);
    let read = serde_json::to_value(
        chat.widget_page(None, Some(delivered["revision"].as_str().unwrap().into()))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read["read_through"], sent.end);
    assert_eq!(
        read["unchanged"], false,
        "reading must invalidate the receipt revision without an append"
    );
    let store = MarkdownChatStore::default();
    let reopened = store
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let persisted = serde_json::to_value(reopened.widget_page(None, None).await.unwrap()).unwrap();
    assert_eq!(persisted["read_through"], sent.end);
}

#[tokio::test]
async fn unanswered_await_exposes_a_fixed_grace_without_changing_presence_or_history() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(1000).await.unwrap();
    let initial = chat.widget_page(None, None).await.unwrap();
    assert!(matches!(
        chat.wait(Duration::from_millis(1), CancellationToken::new())
            .await
            .unwrap(),
        WaitOutcome::TimedOut(_)
    ));
    let page = serde_json::to_value(
        chat.widget_page(None, Some(initial.revision))
            .await
            .unwrap(),
    )
    .unwrap();
    let deadline = page["agent_waiting_until_ms"]
        .as_u64()
        .expect("timeout must expose its grace deadline");
    assert!(deadline > page["server_time_ms"].as_u64().unwrap());
    assert!(deadline <= page["server_time_ms"].as_u64().unwrap() + 20_000);
    assert_eq!(page["last_agent_call_at_ms"], 1000);
    assert_eq!(page["unchanged"], true);
    let tools = load_tools_for_config(&config);
    for (name, args) in [
        ("chat_read", json!({})),
        ("chat_write", json!({"message":"Still waiting."})),
        ("chat_read", json!({})),
    ] {
        let tool = tools.iter().find(|tool| tool.name() == name).unwrap();
        let result = tool
            .call_with_context(args, &config, &session, &context)
            .await;
        assert!(!result.is_error, "{}", result.joined_text());
        let page = serde_json::to_value(chat.widget_page(None, None).await.unwrap()).unwrap();
        assert_eq!(
            page["agent_waiting_until_ms"], deadline,
            "{name} must not renew grace"
        );
    }
    let summary = serde_json::to_value(chat.owner_summary().await.unwrap()).unwrap();
    assert_eq!(summary["agent_waiting_until_ms"], deadline);
    let reopened = MarkdownChatStore::default()
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let page = serde_json::to_value(reopened.widget_page(None, None).await.unwrap()).unwrap();
    assert!(
        page["agent_waiting_until_ms"].is_null(),
        "a restarted server cannot still be awaiting"
    );
    chat.append_user("late-reply".into(), "Continue".into())
        .await
        .unwrap();
    let page = serde_json::to_value(chat.widget_page(None, None).await.unwrap()).unwrap();
    assert!(page["agent_waiting_until_ms"].is_null());
}

#[tokio::test]
async fn cancelled_await_does_not_leave_a_previous_timeout_grace_active() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    assert!(matches!(
        chat.wait(Duration::from_millis(1), CancellationToken::new())
            .await
            .unwrap(),
        WaitOutcome::TimedOut(_)
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        chat.wait(Duration::from_secs(30), cancellation)
            .await
            .unwrap(),
        WaitOutcome::Cancelled
    ));
    assert!(
        chat.widget_page(None, None)
            .await
            .unwrap()
            .agent_waiting_until_ms
            .is_none()
    );
}

#[tokio::test]
async fn activity_survives_restart_without_consuming_messages_or_reloading_history() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let sent = chat
        .append_user("activity".into(), "Still unread".into())
        .await
        .unwrap();
    let first = chat.widget_page(None, None).await.unwrap();
    assert!(first.last_agent_call_at_ms.is_none());
    assert_eq!(first.total_tool_calls, 0);
    chat.record_agent_call(1000).await.unwrap();
    chat.record_agent_call(500).await.unwrap();
    let active = chat.widget_page(None, Some(first.revision)).await.unwrap();
    assert!(
        active.unchanged,
        "presence changes do not need a full history retransmission"
    );
    assert_eq!(active.last_agent_call_at_ms, Some(1000));
    assert_eq!(active.total_tool_calls, 2);
    assert_eq!(active.delivered_through, 0);
    assert!(active.read_through < sent.end);
    assert!(
        chat.read(false)
            .await
            .unwrap()
            .text
            .contains("Still unread")
    );
    let reopened = MarkdownChatStore::default()
        .chat(&config, context.conversation.as_ref(), &SessionState::new())
        .unwrap();
    let reopened_page = reopened.widget_page(None, None).await.unwrap();
    assert_eq!(reopened_page.last_agent_call_at_ms, Some(1000));
    assert_eq!(reopened_page.total_tool_calls, 2);

    let path = chat.path().with_file_name("cursor.json");
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for field in [
        "lastAgentCallAtMs",
        "totalToolCalls",
        "toolCallEpoch",
        "toolCallSequence",
    ] {
        old.as_object_mut().unwrap().remove(field);
    }
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let legacy = MarkdownChatStore::default()
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let legacy_page = legacy.widget_page(None, None).await.unwrap();
    assert!(legacy_page.last_agent_call_at_ms.is_none());
    assert_eq!(legacy_page.total_tool_calls, 0);
    assert_eq!(legacy_page.read_through, active.read_through);
    assert_eq!(legacy_page.messages[0].markdown, "Still unread");
}

#[tokio::test]
async fn agent_messages_snapshot_the_tool_count_for_interval_rendering() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();

    chat.record_agent_call(1000).await.unwrap();
    chat.append("First agent message".into()).await.unwrap();
    chat.record_agent_call(1100).await.unwrap();
    chat.record_agent_call(1200).await.unwrap();
    chat.append("Second agent message".into()).await.unwrap();

    let page = chat.widget_page(None, None).await.unwrap();
    assert_eq!(page.total_tool_calls, 3);
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].tool_call_count, Some(1));
    assert_eq!(page.messages[1].tool_call_count, Some(3));
}

#[tokio::test]
async fn widget_history_pages_preserve_markdown_and_manual_file_appends() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    append(chat.path(), "Plain file message\n");
    for number in 0..54 {
        chat.append(format!("Agent reply {number}\n\n`code`\n"))
            .await
            .unwrap();
    }
    let latest = chat.widget_page(None, None).await.unwrap();
    assert!(latest.has_more);
    assert_eq!(latest.messages.len(), 50);
    assert_eq!(
        latest.messages.last().unwrap().markdown,
        "Agent reply 53\n\n`code`\n"
    );
    let older = chat.widget_page(latest.before, None).await.unwrap();
    assert!(!older.has_more);
    assert_eq!(older.messages.len(), 5);
    assert_eq!(older.messages[0].markdown, "Plain file message\n");
    assert_eq!(older.messages[0].role, "user");
    let literal = "Literal delimiters:\n\n<!-- codexify-agent-message:v1:start id=\"123-4\" -->\n\n## Agent\n\ninside a user message\n\n<!-- codexify-agent-message:v1:end id=\"123-4\" -->\n";
    let receipt = chat
        .append_user("literal-markers".into(), literal.into())
        .await
        .unwrap();
    assert_eq!(
        chat.read(false).await.unwrap().text,
        format!(
            "[{}]\n{literal}\n\n",
            codexify::markdown_chat::attachments::date(receipt.created_at_ms.unwrap())
        )
    );
    assert_eq!(
        chat.widget_page(None, None)
            .await
            .unwrap()
            .messages
            .last()
            .unwrap()
            .markdown,
        literal
    );
}

#[tokio::test]
async fn chat_tools_return_pending_input_without_the_normal_output_budget() {
    let (_root, mut config, session, context) = fixture();
    config.output.max_tool_output_tokens = Some(1);
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let text = format!("{}END", "user text\n".repeat(12000));
    append(chat.path(), &text);
    let tools = load_tools_for_config(&config);
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let result = writer
        .call_with_context(
            json!({"message":"I am waiting for clarification."}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    let pending = result.new_chat_message_from_user.unwrap();
    assert!(pending.contains("Before"));
    assert!(pending.ends_with(&text));
    assert_eq!(chat.read(false).await.unwrap().text, "");
    append(chat.path(), "Read this\n");
    let reader = tools
        .iter()
        .find(|tool| tool.name() == "chat_read")
        .unwrap();
    let result = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(
        result
            .new_chat_message_from_user
            .unwrap()
            .ends_with("Read this\n")
    );
    let result = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(result.new_chat_message_from_user.is_none());
    assert!(result.joined_text().contains("chat_await"));
}

#[tokio::test]
async fn brief_has_the_conversation_path_and_read_only_history_guidance() {
    let (_root, config, session, context) = fixture();
    let tools = load_tools_for_config(&config);
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "get_agent_brief")
        .unwrap();
    let result = tool
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!result.is_error);
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let brief = result.joined_text();
    assert!(brief.starts_with("## Markdown-driven communication"));
    assert!(brief.contains("### Mandatory state machine"));
    assert!(brief.contains("Do not send a normal ChatGPT final response"));
    assert!(brief.contains(&chat.path().display().to_string()));
    assert!(brief.contains("read-only"));
    assert!(brief.contains("chat_read"));
    assert!(brief.contains("chat_write"));
    assert!(brief.contains("chat_await"));
    assert!(chat.path().is_file());
}

#[tokio::test]
async fn history_tools_read_only_the_current_conversation_without_acknowledging() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    append(chat.path(), "Historical search target\n");
    let other_identity = ConversationIdentity::from_openai_session("other-conversation").unwrap();
    let other = context
        .markdown_chat
        .chat(&config, Some(&other_identity), &session)
        .unwrap();
    other.ensure().await.unwrap();
    append(other.path(), "Unrelated private text\n");
    let mut tools = load_tools_for_config(&config);
    // This test runs in a Cargo harness; workers must enter the real CLI binary.
    *tools.iter_mut().find(|tool| tool.name() == "grep").unwrap() = Box::new(
        codexify::tools::grep::Grep::new(env!("CARGO_BIN_EXE_codexify").into()),
    );
    for name in ["read_file", "grep"] {
        let tool = tools.iter().find(|tool| tool.name() == name).unwrap();
        let args = if name == "grep" {
            json!({"path":chat.path(), "pattern":"Historical"})
        } else {
            json!({"path":chat.path()})
        };
        let result = tool
            .call_with_context(args, &config, &session, &context)
            .await;
        assert!(!result.is_error, "{name}: {}", result.joined_text());
        assert!(result.joined_text().contains("Historical search target"));
        assert_eq!(
            chat.read(false).await.unwrap().text,
            "Historical search target\n"
        );
        let args = if name == "grep" {
            json!({"path":other.path(), "pattern":"private"})
        } else {
            json!({"path":other.path()})
        };
        let denied = tool
            .call_with_context(args, &config, &session, &context)
            .await;
        assert!(denied.is_error);
        assert!(!denied.joined_text().contains("Unrelated private text"));
    }
}

#[tokio::test]
async fn await_returns_immediate_input_and_can_be_cancelled_without_consuming() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    append(chat.path(), "already waiting\n");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        chat.wait(Duration::from_secs(2), cancelled).await.unwrap(),
        WaitOutcome::Cancelled
    ));
    assert_eq!(chat.read(false).await.unwrap().text, "already waiting\n");
    let result = chat
        .wait(Duration::from_secs(2), CancellationToken::new())
        .await
        .unwrap();
    match result {
        WaitOutcome::Message(snapshot) => assert_eq!(snapshot.text, "already waiting\n"),
        _ => panic!("pending input was not returned"),
    }
    let token = CancellationToken::new();
    let waiting = tokio::spawn({
        let chat = chat.clone();
        let token = token.clone();
        async move { chat.wait(Duration::from_secs(20), token).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        WaitOutcome::Cancelled
    ));
}

#[tokio::test]
async fn directory_watcher_handles_atomic_editor_replacement() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let waiting = tokio::spawn({
        let chat = chat.clone();
        async move {
            chat.wait(Duration::from_secs(5), CancellationToken::new())
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut new_file = tempfile::NamedTempFile::new_in(chat.path().parent().unwrap()).unwrap();
    new_file
        .write_all(&std::fs::read(chat.path()).unwrap())
        .unwrap();
    new_file.write_all(b"saved by replacement\n").unwrap();
    new_file.persist(chat.path()).unwrap();
    match waiting.await.unwrap().unwrap() {
        WaitOutcome::Message(snapshot) => assert_eq!(snapshot.text, "saved by replacement\n"),
        _ => panic!("atomic editor append was missed"),
    }
}

#[tokio::test]
async fn timeout_instructs_another_await_without_claiming_a_read_receipt() {
    let (_root, config, session, context) = fixture();
    select_fixture_workspace(&config, &context).await;
    let tools = load_tools_for_config(&config);
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "chat_await")
        .unwrap();
    let result = tool
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap()["status"], "timeout");
    assert!(result.new_chat_message_from_user.is_none());
    let text = result
        .content
        .iter()
        .filter_map(|part| match part {
            codexify::types::ToolContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("chat_await again"));
    assert!(!text.contains("confirmed that the user has seen"));
}

#[tokio::test]
#[ignore = "requires CODEXIFY_TEST_APPRISE_PYTHON; exercised explicitly by CI on every platform"]
async fn apprise_ntfy_token_and_delivery_failure_preserve_transcript() {
    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    let (_root, mut config, session, context) = fixture();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/",
        post(move |headers: HeaderMap, body: Bytes| {
            let tx = tx.clone();
            async move {
                let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
                if payload["topic"] == "failure" {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(json!({"error":"unavailable"})),
                    );
                }
                tx.send((headers, payload)).unwrap();
                (
                    StatusCode::OK,
                    axum::Json(json!({"id":"test","event":"message"})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    config.markdown_chat.notifications = Some(serde_json::from_value(json!({
        "urls":[format!("ntfy://test-ntfy-token@{address}/topic?auth=token&image=no")],
        "pythonPath":std::env::var("CODEXIFY_TEST_APPRISE_PYTHON").expect("Apprise test interpreter")
    })).unwrap());
    let tools = load_tools_for_config(&config);
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let markdown = format!("## Question\n{}\n", "**full message**\n".repeat(100));
    let result = writer
        .call_with_context(json!({"message":markdown}), &config, &session, &context)
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    assert_eq!(
        result.structured_content.unwrap()["notification"],
        "accepted",
        "received {} requests at the local ntfy endpoint",
        rx.len()
    );
    let mut chunks = Vec::new();
    while let Ok((headers, payload)) = rx.try_recv() {
        assert_eq!(headers["authorization"], "Bearer test-ntfy-token");
        assert_eq!(headers["x-markdown"], "yes");
        chunks.push(payload["message"].as_str().unwrap().to_owned());
    }
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks.join("\n"), markdown.trim());
    config.markdown_chat.notifications.as_mut().unwrap().urls = vec![format!(
        "ntfy://test-ntfy-token@{address}/failure?auth=token&image=no"
    )];
    let result = writer
        .call_with_context(
            json!({"message":"Written despite notification failure"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap()["notification"], "failed");
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    assert!(
        std::fs::read_to_string(chat.path())
            .unwrap()
            .contains(&markdown)
    );
    assert!(
        std::fs::read_to_string(chat.path())
            .unwrap()
            .contains("Written despite notification failure")
    );
    assert_eq!(
        chat.read(false).await.unwrap().notification,
        NotificationState::Failed
    );
    server.abort();
}

#[tokio::test]
async fn stop_requests_persist_without_acknowledging_messages_and_resume_only_on_new_intent() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.append_user("unread".into(), "Pending message".into())
        .await
        .unwrap();
    chat.record_agent_call(1000).await.unwrap();
    let before = chat.widget_page(None, None).await.unwrap();
    chat.request_agent_stop("stop-one".into()).await.unwrap();
    chat.record_agent_call(2000).await.unwrap();
    let stopped = chat.widget_page(None, None).await.unwrap();
    assert!(stopped.agent_stop_requested);
    assert_eq!(stopped.last_agent_call_at_ms, None);
    assert_eq!(stopped.agent_waiting_until_ms, None);
    assert_eq!(stopped.total_tool_calls, 2);
    assert_eq!(stopped.read_through, before.read_through);
    assert_eq!(stopped.delivered_through, before.delivered_through);
    assert_eq!(
        chat.read(false).await.unwrap().text,
        format!(
            "[{}]\nPending message\n\n",
            codexify::markdown_chat::attachments::date(before.messages[0].created_at_ms.unwrap())
        )
    );
    let reopened = MarkdownChatStore::default()
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    assert!(
        reopened
            .widget_page(None, None)
            .await
            .unwrap()
            .agent_stop_requested
    );
    reopened
        .append_user("next-intent".into(), "Continue with this instead".into())
        .await
        .unwrap();
    let resumed = reopened.widget_page(None, None).await.unwrap();
    assert!(!resumed.agent_stop_requested);
    assert_eq!(resumed.last_agent_call_at_ms, None);
    reopened
        .request_agent_stop("stop-one".into())
        .await
        .unwrap();
    assert!(
        !reopened.agent_stop_requested().await.unwrap(),
        "a retry of the old stop must not stop new work"
    );
    reopened.record_agent_call(3000).await.unwrap();
    assert_eq!(
        reopened
            .widget_page(None, None)
            .await
            .unwrap()
            .last_agent_call_at_ms,
        Some(3000)
    );
}

#[tokio::test]
#[ignore = "requires CODEXIFY_TEST_APPRISE_PYTHON; local notification receiver only"]
async fn chat_event_filters_and_repeated_awaits_deliver_only_requested_alerts() {
    use axum::{Router, routing::post};
    let (_root, mut config, session, mut context) = fixture();
    select_fixture_workspace(&config, &context).await;
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/",
        post(move |body: String| {
            let sender = sender.clone();
            async move {
                sender
                    .send(serde_json::from_str::<serde_json::Value>(&body).unwrap())
                    .unwrap();
                axum::Json(json!({"id":"test","event":"message"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    config.markdown_chat.notifications = Some(
        serde_json::from_value(json!({
            "urls":[format!("ntfy://{address}/test?image=no")],
            "pythonPath":std::env::var("CODEXIFY_TEST_APPRISE_PYTHON").unwrap(),
            "events":{"messages":false,"offline":false}
        }))
        .unwrap(),
    );
    let tools = load_tools_for_config(&config);
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let waiter = tools
        .iter()
        .find(|tool| tool.name() == "chat_await")
        .unwrap();
    let result = writer
        .call_with_context(
            json!({"message":"A progress update"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert_eq!(
        result.structured_content.unwrap()["notification"],
        "suppressed"
    );
    assert!(received.try_recv().is_err());
    for _ in 0..2 {
        let result = waiter
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        assert_eq!(result.structured_content.unwrap()["status"], "timeout");
    }
    let input = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        input["message"]
            .as_str()
            .unwrap()
            .contains("waiting for your input")
    );
    assert!(received.try_recv().is_err());
    context.markdown_chat = Arc::new(MarkdownChatStore::default());
    waiter
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(
        received.try_recv().is_err(),
        "reopening a persistent chat must not repeat the input alert"
    );
    let chat = context
        .markdown_chat
        .chat(&config, context.task_conversation.as_ref(), &session)
        .unwrap();
    chat.append_user("reply".into(), "Continue".into())
        .await
        .unwrap();
    waiter
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(received.try_recv().is_err());
    for (message, event, expected) in [
        ("Which option?", "input_required", "accepted"),
        ("Still the same question", "input_required", "suppressed"),
        ("The request is complete", "completed", "accepted"),
    ] {
        let result = writer
            .call_with_context(
                json!({"message":message,"event":event}),
                &config,
                &session,
                &context,
            )
            .await;
        let output = result.structured_content.unwrap();
        assert_eq!(output["notification"], expected);
        assert_eq!(output["assistant_turn_may_end"], false);
        if expected == "accepted" {
            assert_eq!(received.recv().await.unwrap()["message"], message);
        } else {
            assert!(received.try_recv().is_err());
        }
    }
    waiter
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(
        received.try_recv().is_err(),
        "waiting after completion must not request input again"
    );
    config
        .markdown_chat
        .notifications
        .as_mut()
        .unwrap()
        .events
        .completed = false;
    let result = writer
        .call_with_context(
            json!({"message":"Complete","event":"completed"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert_eq!(
        result.structured_content.unwrap()["notification"],
        "suppressed"
    );
    assert!(received.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn queued_widget_input_is_private_until_turn_completed_and_not_chat_await() {
    let (_root, config, session, context) = fixture();
    select_fixture_workspace(&config, &context).await;
    let chat = context
        .markdown_chat
        .chat(&config, context.task_conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(now_ms()).await.unwrap();
    let tools = load_tools_for_config(&config);
    let find = |name| tools.iter().find(|t| t.name() == name).unwrap();
    let sent = find("chat_ui_send")
        .call_with_context(
            widget_args(json!({"request_id":"queued-one","message":"keep this private","message_mode":"queue"}), &config, &session, &context),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!sent.is_error, "{}", sent.joined_text());
    assert!(!sent.joined_text().contains("keep this private"));
    assert!(sent.structured_content.is_none());
    let result = find("chat_read")
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(
        result
            .structured_content
            .as_ref()
            .unwrap()
            .get("new_chat_message_from_user")
            .is_none()
    );
    assert!(
        !std::fs::read_to_string(chat.path())
            .unwrap()
            .contains("keep this private")
    );
    let result = find("chat_await")
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert_eq!(
        result.structured_content.as_ref().unwrap()["status"],
        "timeout"
    );
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    let result = find("turn_completed")
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    let value = result.structured_content.as_ref().unwrap();
    assert!(
        result
            .new_chat_message_from_user
            .as_ref()
            .unwrap()
            .contains("keep this private")
    );
    assert_eq!(value["required_next_action"], "chat_write");
    assert!(jsonschema::is_valid(
        &find("turn_completed").output_schema().unwrap(),
        value
    ));
}

#[tokio::test]
async fn input_required_holds_private_tasks_and_default_queue_sends_the_answer_directly() {
    for blocked_goal in [false, true] {
        let (_root, mut config, session, context) = fixture();
        select_fixture_workspace(&config, &context).await;
        config.markdown_chat.default_message_mode = codexify::markdown_chat::MessageMode::Queue;
        let chat = context
            .markdown_chat
            .chat(&config, context.task_conversation.as_ref(), &session)
            .unwrap();
        chat.ensure().await.unwrap();
        chat.record_agent_call(now_ms()).await.unwrap();
        if blocked_goal {
            std::fs::write(
                chat.path().with_file_name("automation.json"),
                json!({"goal":{"status":"blocked"}}).to_string(),
            )
            .unwrap();
        }
        for id in ["first", "second"] {
            chat.queue_user_message(id.into(), format!("private {id}"), vec![])
                .await
                .unwrap();
        }
        let tools = load_tools_for_config(&config);
        let find = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        let continuing_question = find("chat_write")
            .call_with_context(
                json!({"message":"Which platform should I target? I can inspect shared code meanwhile.","event":"message"}),
                &config,
                &session,
                &context,
            )
            .await;
        assert!(!continuing_question.is_error);
        let continuing = continuing_question.structured_content.as_ref().unwrap();
        assert_eq!(continuing["assistant_turn_may_end"], false);
        assert_eq!(
            continuing["required_next_action"],
            "continue_or_turn_completed"
        );
        assert!(
            continuing_question
                .joined_text()
                .contains("continue useful work that does not need the answer")
        );
        assert!(!chat.queue_input_required().await.unwrap());
        assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
        let question = find("chat_write")
            .call_with_context(
                json!({"message":"Does the app freeze?","event":"input_required"}),
                &config,
                &session,
                &context,
            )
            .await;
        assert!(!question.is_error);
        assert_eq!(
            question.structured_content.as_ref().unwrap()["assistant_turn_may_end"],
            false
        );
        assert!(
            question
                .joined_text()
                .contains("Call chat_await now for the needed answer")
        );
        assert_eq!(
            question.structured_content.as_ref().unwrap()["required_next_action"],
            "chat_await"
        );
        assert!(chat.queue_input_required().await.unwrap());
        assert!(
            chat.release_queued_message(u64::MAX)
                .await
                .unwrap()
                .is_none(),
            "a question blocks queued tasks even before chat_await and after presence expires"
        );
        let reopened = MarkdownChatStore::default()
            .chat(&config, context.task_conversation.as_ref(), &session)
            .unwrap();
        assert!(
            reopened.queue_input_required().await.unwrap(),
            "the input request survives a restart"
        );
        let state = find("chat_ui_state")
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        assert_eq!(
            state
                .meta
                .as_ref()
                .unwrap()
                .get(codexify::markdown_chat_ui::CHAT_WIDGET_META)
                .unwrap()["input_required"],
            true
        );
        let waiting = find("chat_await")
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        assert_eq!(
            waiting.structured_content.as_ref().unwrap()["required_next_action"],
            "chat_await"
        );
        assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
        let answer = find("chat_ui_send")
            .call_with_context(
                widget_args(
                    json!({"request_id":"answer","message":"Freezes too"}),
                    &config,
                    &session,
                    &context,
                ),
                &config,
                &session,
                &context,
            )
            .await;
        assert!(!answer.is_error);
        assert!(!chat.queue_input_required().await.unwrap());
        assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
        let read = find("chat_read")
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        let text = read.new_chat_message_from_user.unwrap();
        assert!(text.contains("Freezes too"));
        assert!(!text.contains("private first"));
        if blocked_goal {
            let complete = find("turn_completed")
                .call_with_context(json!({}), &config, &session, &context)
                .await;
            assert!(complete.is_error);
            assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
            std::fs::write(
                chat.path().with_file_name("automation.json"),
                json!({"goal":{"status":"completed"}}).to_string(),
            )
            .unwrap();
        }
        let complete = find("turn_completed")
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        assert!(
            complete
                .new_chat_message_from_user
                .unwrap()
                .contains("private first")
        );
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
        let repeated = find("turn_completed")
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        assert_eq!(
            repeated.structured_content.as_ref().unwrap()["required_next_action"],
            "chat_write"
        );
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn completed_waits_repeatedly_when_empty_and_unfinished_goals_hold_input() {
    let (_root, config, session, context) = fixture();
    select_fixture_workspace(&config, &context).await;
    let tools = load_tools_for_config(&config);
    let complete = tools.iter().find(|t| t.name() == "turn_completed").unwrap();
    let chat = context
        .markdown_chat
        .chat(&config, context.task_conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(now_ms()).await.unwrap();
    for _ in 0..2 {
        let result = complete
            .call_with_context(json!({}), &config, &session, &context)
            .await;
        assert!(!result.is_error);
        let value = result.structured_content.as_ref().unwrap();
        assert_eq!(value["status"], "timeout");
        assert_eq!(value["required_next_action"], "turn_completed");
        assert_eq!(value["assistant_turn_may_end"], false);
        assert!(
            result
                .joined_text()
                .contains("Required next call: turn_completed")
        );
        assert!(result.joined_text().contains("anticipated timeout"));
        assert!(
            chat.owner_summary()
                .await
                .unwrap()
                .agent_waiting_until_ms
                .is_none()
        );
    }
    let finished = tools
        .iter()
        .find(|t| t.name() == "chat_write")
        .unwrap()
        .call_with_context(
            json!({"message":"The task is complete","event":"completed"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert_eq!(
        finished.structured_content.as_ref().unwrap()["required_next_action"],
        "turn_completed"
    );
    // An explicit input wait must not be reinterpreted from an earlier completion.
    let needed = tools
        .iter()
        .find(|t| t.name() == "chat_await")
        .unwrap()
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert_eq!(
        needed.structured_content.as_ref().unwrap()["required_next_action"],
        "chat_await"
    );
    assert!(chat.queue_input_required().await.unwrap());
    std::fs::write(
        chat.path().with_file_name("automation.json"),
        json!({"goal":{"status":"blocked"}}).to_string(),
    )
    .unwrap();
    chat.queue_user_message("private-blocked".into(), "next task".into(), vec![])
        .await
        .unwrap();
    let result = complete
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(result.is_error);
    assert!(result.joined_text().contains("goal is unfinished"));
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
}

#[tokio::test]
async fn legacy_completion_releases_only_one_and_read_receipt_is_correct() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.task_conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(now_ms()).await.unwrap();
    for id in ["first", "second"] {
        chat.queue_user_message(id.into(), format!("queued {id}"), vec![])
            .await
            .unwrap();
    }
    let tools = load_tools_for_config(&config);
    let write = tools.iter().find(|t| t.name() == "chat_write").unwrap();
    let result = write
        .call_with_context(
            json!({"message":"Finished current request","event":"completed"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(
        result
            .new_chat_message_from_user
            .as_ref()
            .unwrap()
            .contains("queued first")
    );
    assert!(!result.joined_text().contains("queued second"));
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    assert!(chat.read(true).await.unwrap().text.is_empty());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn rollout_tools_use_only_the_current_chat_and_validate_their_public_schemas() {
    let (_root, config, session, context) = fixture();
    let tools = load_tools_for_config(&config);
    let update = tools.iter().find(|t| t.name() == "rollout_update").unwrap();
    let read = tools.iter().find(|t| t.name() == "rollout_read").unwrap();
    let args = json!({"request_id":"checkpoint","status":"Investigating","progress":"Verified the current behavior","next_steps":"Implement and test the fix"});
    assert!(jsonschema::is_valid(&update.input_schema(), &args));
    let saved = update
        .call_with_context(args.clone(), &config, &session, &context)
        .await;
    assert!(!saved.is_error);
    assert!(jsonschema::is_valid(
        &update.output_schema().unwrap(),
        saved.structured_content.as_ref().unwrap()
    ));
    assert_eq!(
        saved.structured_content.as_ref().unwrap()["assistant_turn_may_end"],
        false
    );
    assert_eq!(
        saved.structured_content.as_ref().unwrap()["required_next_action"],
        "continue_work"
    );
    assert!(saved.joined_text().contains("Run the next task tool now"));
    assert!(saved.joined_text().contains("normal ChatGPT final answer"));
    assert!(update.description().contains("NON-TERMINAL"));
    let retry = update
        .call_with_context(args, &config, &session, &context)
        .await;
    assert_eq!(
        retry.structured_content.as_ref().unwrap()["already_recorded"],
        true
    );
    let recent = read
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!recent.is_error);
    assert_eq!(
        recent.structured_content.as_ref().unwrap()["report_count"],
        1
    );
    assert_eq!(
        recent.structured_content.as_ref().unwrap()["assistant_turn_may_end"],
        false
    );
    assert!(recent.joined_text().contains("Run the next task tool now"));
    assert!(jsonschema::is_valid(
        &read.output_schema().unwrap(),
        recent.structured_content.as_ref().unwrap()
    ));
    assert!(
        recent
            .joined_text()
            .contains("Verified the current behavior")
    );
    let mut other = context.clone();
    other.task_conversation = ConversationIdentity::from_openai_session("different-rollout");
    let isolated = read
        .call_with_context(json!({}), &config, &session, &other)
        .await;
    assert_eq!(
        isolated.structured_content.as_ref().unwrap()["report_count"],
        0
    );
    assert!(
        !isolated
            .joined_text()
            .contains("Verified the current behavior")
    );
    assert!(
        read.call_with_context(
            json!({"path":"../foreign/rollout.md"}),
            &config,
            &session,
            &context
        )
        .await
        .is_error
    );
}
