fn automation_request(name: &str, arguments: Value, conversation: &str) -> CallToolRequestParams {
    let mut request = CallToolRequestParams::new(name.to_owned())
        .with_arguments(arguments.as_object().unwrap().clone());
    request.meta = Some(serde_json::from_value(json!({"openai/session":conversation})).unwrap());
    request
}

struct ServerAutomationNoopExecutor;

#[async_trait::async_trait]
impl crate::agent_automation::AutomationExecutor for ServerAutomationNoopExecutor {
    async fn execute(
        &self,
        _execution: crate::agent_automation::AutomationExecution,
        _events: tokio::sync::mpsc::UnboundedSender<crate::agent_automation::CompanionEvent>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<
        crate::agent_automation::CompanionActionSnapshot,
        crate::agent_automation::AutomationExecutionError,
    > {
        panic!("an online server call must not execute browser automation")
    }
}

#[tokio::test]
async fn unbound_pairing_continues_the_existing_task_before_verifying_without_delivering_work() {
    let root = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(root.path()).unwrap();
    let mut config = crate::config::default_config(workspace.clone());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    config.conversation_auth_token = Some("a".repeat(64).into());
    let mut handler = handler_with_tools(root.path(), crate::registry::load_tools_for_config(&config), crate::types::ToolLogLevel::Info);
    handler.config = Arc::new(config.clone());
    handler.continuations = Arc::new(crate::conversation_continuations::ConversationContinuationStore::new(root.path().join("continuations.json")).unwrap());
    let source_name = "pairing-original-owner";
    let target_name = "pairing-new-unbound";
    let source = ConversationIdentity::from_openai_session(source_name).unwrap();
    let target = ConversationIdentity::from_openai_session(target_name).unwrap();
    bind_fixture_workspace(&handler, Some(&source)).await;
    let effective = handler.effective_task_config(Some(&source), workspace.clone()).unwrap();
    let store = Arc::new(crate::agent_automation::AgentAutomationStore::for_config(&effective).unwrap());
    let chat = handler.markdown_chat.chat(&effective, Some(&source), &handler.session).unwrap();
    chat.append_user("pending-work".into(), "Continue the existing task after pairing.".into()).await.unwrap();
    let supervisor = crate::agent_automation::AgentAutomationSupervisor::new(config.markdown_chat.automation.clone(), Arc::new(ServerAutomationNoopExecutor));
    supervisor.enable_conversation_rollover(handler.config.clone(), handler.project_bindings.clone(), handler.continuations.clone(), handler.tools.clone(), handler.agent_tickets.clone()).unwrap();
    let handle = crate::agent_automation::AutomationTaskHandle::new(source.clone(), 0, store.clone(), chat.clone());
    supervisor.register_task(handle.clone()).await.unwrap();
    handler.agent_automation = Some(supervisor.clone());
    let now = crate::markdown_chat::now_ms();
    let issue = store.issue_pairing(&source, "https://chatgpt.com/c/unbound-pairing", 0, now).unwrap();
    store.record_pairing_canonical_url(&source, issue.generation, "https://chatgpt.com/c/unbound-pairing", now).unwrap();
    let authorizations = handler.conversation_authorizations.clone();
    let bindings = handler.project_bindings.clone();
    let continuations = handler.continuations.clone();
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server = tokio::spawn(async move { handler.serve(server_transport).await.unwrap().waiting().await.unwrap(); });
    let client = ().serve(client_transport).await.unwrap();

    let unauthorized = client.call_tool(automation_request("automation_verify_pairing", json!({"reference":issue.reference}), target_name)).await.unwrap();
    assert_eq!(unauthorized.is_error, Some(true));
    assert_eq!(store.load(&source).unwrap().pairing.pending.unwrap().owner_generation, 0);
    authorizations.authorize(Some(&target), &SessionState::new()).unwrap();
    let invalid = client.call_tool(automation_request("automation_verify_pairing", json!({"reference":"B".repeat(43)}), target_name)).await.unwrap();
    assert_eq!(invalid.is_error, Some(true));
    assert_eq!(store.load(&source).unwrap().pairing.pending.unwrap().owner_generation, 0);

    let mut tokens = Vec::new();
    for _ in 0..2 {
        let response = client.call_tool(automation_request("automation_verify_pairing", json!({"reference":issue.reference}), target_name)).await.unwrap();
        assert_ne!(response.is_error, Some(true), "{response:?}");
        let result = response.structured_content.unwrap();
        assert_eq!(result["verified"], false);
        assert_eq!(result["terminal"], false);
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("continue_task"));
        tokens.push(content.split('`').nth(3).unwrap().to_owned());
    }
    assert_ne!(tokens[0], tokens[1]);
    assert!(bindings.selected_project_root(&config, &target).unwrap().is_none());
    assert!(continuations.owner_snapshot(&source).unwrap().active);
    // A delayed source call must not revoke the pairing reserved for the next owner.
    supervisor.register_task(handle).await.unwrap();
    assert_eq!(store.load(&source).unwrap().pairing.pending.unwrap().owner_generation, 1);
    let superseded = client.call_tool(automation_request("continue_task", json!({"continuationToken":tokens[0]}), target_name)).await.unwrap();
    assert_eq!(superseded.is_error, Some(true));
    let continued = client.call_tool(automation_request("continue_task", json!({"continuationToken":tokens[1]}), target_name)).await.unwrap();
    assert_ne!(continued.is_error, Some(true), "{continued:?}");
    let continued = continued.structured_content.unwrap();
    assert_eq!(continued["continued"], true);
    assert_eq!(continued["activeRoot"], workspace.to_string_lossy().as_ref());
    assert!(continued["content"].as_str().unwrap().contains("automation_verify_pairing"));
    assert!(continued["new_chat_message_from_user"].is_null());
    assert_eq!(chat.unread_snapshot().await.unwrap().messages.len(), 1);
    assert_eq!(continuations.owner_snapshot(&target).unwrap().task, source);
    assert!(!continuations.owner_snapshot(&source).unwrap().active);

    let verified = client.call_tool(automation_request("automation_verify_pairing", json!({"reference":issue.reference}), target_name)).await.unwrap();
    assert_ne!(verified.is_error, Some(true), "{verified:?}");
    let verified = verified.structured_content.unwrap();
    assert_eq!(verified["verified"], true);
    assert!(verified["new_chat_message_from_user"].is_null());
    assert_eq!(store.load(&source).unwrap().pairing.verified_owner_generation, Some(1));
    assert!(store.load(&source).unwrap().pairing.pending.is_none());
    assert_eq!(chat.unread_snapshot().await.unwrap().messages.len(), 1);

    let read = client.call_tool(automation_request("chat_read", json!({}), target_name)).await.unwrap();
    assert_ne!(read.is_error, Some(true));
    assert!(format!("{read:?}").contains("Continue the existing task after pairing."));
    assert!(chat.unread_snapshot().await.unwrap().messages.is_empty());
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn pairing_verification_respects_setup_and_receives_the_owner_generation() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    config.conversation_auth_token = Some("a".repeat(64).into());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let authorizations = handler.conversation_authorizations.clone();
    let conversation = "automation-pair-owner";
    let identity = ConversationIdentity::from_openai_session(conversation).unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let store = crate::agent_automation::AgentAutomationStore::for_config(&config).unwrap();
    let issued = store
        .issue_pairing(
            &identity,
            "https://chatgpt.com/c/server-pairing",
            0,
            crate::markdown_chat::now_ms(),
        )
        .unwrap();
    store
        .record_pairing_canonical_url(
            &identity,
            issued.generation,
            "https://chatgpt.com/c/server-pairing",
            crate::markdown_chat::now_ms(),
        )
        .unwrap();

    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.append_user(
        "during-pairing".into(),
        "Start the actual work with the selected model.".into(),
    )
    .await
    .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();

    let unauthorized = client
        .call_tool(automation_request(
            "automation_verify_pairing",
            json!({"reference":issued.reference}),
            conversation,
        ))
        .await
        .unwrap();
    assert_eq!(unauthorized.is_error, Some(true));
    assert!(
        unauthorized
            .content
            .iter()
            .any(|content| format!("{content:?}").contains("setup"))
    );
    assert!(store.load(&identity).unwrap().pairing.pending.is_some());

    authorizations
        .authorize(Some(&identity), &SessionState::new())
        .unwrap();
    let verified = client
        .call_tool(automation_request(
            "automation_verify_pairing",
            json!({"reference":issued.reference}),
            conversation,
        ))
        .await
        .unwrap();
    assert_ne!(verified.is_error, Some(true));
    let structured = verified.structured_content.as_ref().unwrap();
    assert_eq!(structured["verified"], true);
    assert_eq!(structured["pairingGeneration"], issued.generation);
    assert_eq!(
        store
            .load(&identity)
            .unwrap()
            .pairing
            .verified_owner_generation,
        Some(0)
    );

    assert!(structured["new_chat_message_from_user"].is_null());
    let pending = chat.unread_snapshot().await.unwrap();
    assert_eq!(pending.messages.len(), 1);
    assert_eq!(pending.messages[0].id, "during-pairing");
    let summary = chat.owner_summary().await.unwrap();
    let until = summary
        .agent_presence_until_ms
        .expect("verification has a short presence lease");
    assert!(
        until
            >= summary.last_agent_call_at_ms.unwrap()
                + crate::markdown_chat::PAIRING_PRESENCE_GRACE_MS
    );
    assert_eq!(
        chat.widget_page(None, None)
            .await
            .unwrap()
            .delivered_through,
        0
    );
    let normal = client
        .call_tool(automation_request(
            "get_agent_brief",
            json!({}),
            conversation,
        ))
        .await
        .unwrap();
    assert_ne!(normal.is_error, Some(true));
    assert!(
        chat.owner_summary()
            .await
            .unwrap()
            .agent_presence_until_ms
            .is_none()
    );
    assert!(
        normal.structured_content.as_ref().unwrap()["new_chat_message_from_user"]
            .as_str()
            .unwrap()
            .contains("Start the actual work")
    );

    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn model_visible_calls_register_the_stable_task_with_the_shared_supervisor() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let supervisor = crate::agent_automation::AgentAutomationSupervisor::new(
        config.markdown_chat.automation.clone(),
        Arc::new(ServerAutomationNoopExecutor),
    );
    handler.agent_automation = Some(supervisor.clone());
    let conversation = "automation-registration-owner";
    let identity = ConversationIdentity::from_openai_session(conversation).unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();

    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();

    let result = client
        .call_tool(automation_request(
            "clock_curr_time",
            json!({}),
            conversation,
        ))
        .await
        .unwrap();

    assert_ne!(result.is_error, Some(true));
    assert_eq!(supervisor.registered_count().await, 1);
    assert!(
        chat.owner_summary()
            .await
            .unwrap()
            .last_agent_call_at_ms
            .is_some()
    );
    client.cancel().await.unwrap();
    task.await.unwrap();
}
