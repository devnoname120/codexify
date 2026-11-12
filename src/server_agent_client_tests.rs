struct CodingClientExecutor;

#[async_trait::async_trait]
impl crate::agent_automation::AutomationExecutor for CodingClientExecutor {
    async fn prepare_new_chat(
        &self,
        _: Option<&str>,
        _: CancellationToken,
    ) -> Result<crate::agent_automation::CapabilityCatalog, String> {
        Ok(serde_json::from_value(json!({"protocolVersion":1,"build":format!("fixture{}",crate::agent_automation::PROTOCOL_BUILD_SUFFIX),
            "accountFingerprint":"coding-client-account","catalogRevision":"coding-client-catalog","models":[{
            "id":"client-model","label":"Client model","efforts":[{"id":"high","label":"High","available":true,"reason":null}]}]})).unwrap())
    }
    async fn execute(
        &self,
        _: crate::agent_automation::AutomationExecution,
        _: tokio::sync::mpsc::UnboundedSender<crate::agent_automation::CompanionEvent>,
        _: CancellationToken,
    ) -> Result<
        crate::agent_automation::CompanionActionSnapshot,
        crate::agent_automation::AutomationExecutionError,
    > {
        panic!("MCP integration test must never send to the real browser")
    }
}

#[tokio::test]
async fn coding_client_mcp_setup_reconnect_and_control_need_no_chatgpt_metadata_or_transport_project()
 {
    let root = tempfile::tempdir().unwrap();
    let access = root.path().join("projects");
    std::fs::create_dir_all(access.join("repo")).unwrap();
    let mut config = crate::config::default_config(access);
    config.memory.dir = Some(root.path().join("state").display().to_string());
    config.conversation_auth_token = Some("e".repeat(64).into());
    config.markdown_chat.features.agent_teams = true;
    config.markdown_chat.features.external_agent_control = true;
    config.markdown_chat.automation.connector_name = "Test connector".into();
    config.markdown_chat.automation.descriptor_path = root
        .path()
        .join("browser/descriptor.json")
        .display()
        .to_string();
    let supervisor = crate::agent_automation::AgentAutomationSupervisor::new(
        config.markdown_chat.automation.clone(),
        Arc::new(CodingClientExecutor),
    );
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    handler.agent_automation = Some(supervisor.clone());
    let bindings = handler.project_bindings.clone();
    let chats = handler.markdown_chat.clone();
    let continuations = handler.continuations.clone();
    let auth = handler.conversation_authorizations.clone();
    let session = crate::exec_sessions::ConversationExecSessionStore::new().session_for(
        &ConversationIdentity::from_openai_session("test-transport-authorization").unwrap(),
        &handler.session,
    );
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |args: Value| {
        CallToolRequestParams::new("agent_client").with_arguments(args.as_object().unwrap().clone())
    };
    let unauthorized = client
        .call_tool(request(json!({"action":"list"})))
        .await
        .unwrap();
    assert_eq!(
        unauthorized.is_error,
        Some(true),
        "normal setup(ref) authorization still applies"
    );
    auth.authorize(None, &session).unwrap();
    let created = client
        .call_tool(request(
            json!({"action":"setup","requestId":"code-team","clientName":"Coding app",
        "projectPath":"repo","model":"client-model","effort":"high"}),
        ))
        .await
        .unwrap();
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let team = created.structured_content.as_ref().unwrap()["teamId"]
        .as_str()
        .unwrap()
        .to_owned();
    client.cancel().await.unwrap();
    server.await.unwrap();

    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config);
    handler.agent_automation = Some(supervisor.clone());
    handler.project_bindings = bindings;
    handler.markdown_chat = chats;
    handler.continuations = continuations;
    handler
        .conversation_authorizations
        .authorize(None, &handler.session)
        .unwrap();
    let physical =
        ConversationIdentity::from_openai_session("cannot-control-external-teams").unwrap();
    handler
        .conversation_authorizations
        .authorize(Some(&physical), &SessionState::new())
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(client_transport).await.unwrap();
    let listed = client
        .call_tool(request(json!({"action":"list"})))
        .await
        .unwrap();
    assert_eq!(
        listed.structured_content.as_ref().unwrap()["teams"][0]["teamId"],
        team
    );
    let spawned = client
        .call_tool(request(
            json!({"action":"spawn","teamId":team,"requestId":"create-verifier","workers":[{
        "role":"Verifier","goal":"Verify and report the result","scope":"tests","readOnly":true}]}),
        ))
        .await
        .unwrap();
    assert_ne!(spawned.is_error, Some(true), "{spawned:?}");
    let worker =
        spawned.structured_content.as_ref().unwrap()["team"]["workers"][0]["taskKey"].clone();
    assert_eq!(supervisor.queued_count().await, 1);
    let stopped = client
        .call_tool(request(json!({"action":"control","teamId":team,"control":{
        "requestId":"stop-worker","workerTaskKey":worker,"action":"stop","expectedRevision":1}})))
        .await
        .unwrap();
    assert_ne!(stopped.is_error, Some(true), "{stopped:?}");
    let forbidden = client
        .call_tool(automation_request(
            "agent_client",
            json!({"action":"status","teamId":team}),
            "cannot-control-external-teams",
        ))
        .await
        .unwrap();
    assert_eq!(forbidden.is_error, Some(true));
    assert!(format!("{forbidden:?}").contains("cannot switch"));
    client.cancel().await.unwrap();
    server.await.unwrap();
}
