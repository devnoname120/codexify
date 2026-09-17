#[tokio::test]
async fn workspace_picker_wait_and_switch_require_brief_without_touching_old_files() {
    for stable in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        for name in ["a", "b"] {
            std::fs::create_dir_all(projects.join(name)).unwrap();
            std::fs::write(
                projects.join(name).join("AGENTS.md"),
                format!("Instructions for project {name}"),
            )
            .unwrap();
        }
        let mut config = crate::config::default_config(projects);
        config.multi_project = true;
        config.markdown_chat.enabled = true;
        config.markdown_chat.max_wait_ms = 2000;
        config.worktrees.mode = crate::types::WorktreeMode::Never;
        config.memory.dir = Some(root.path().join("memory").display().to_string());
        let mut handler = handler_with_tools(
            root.path(),
            crate::registry::load_tools_for_config(&config),
            crate::types::ToolLogLevel::Info,
        );
        handler.config = Arc::new(config);
        let (server, client_transport) = tokio::io::duplex(128 * 1024);
        let server_task = tokio::spawn(async move {
            handler
                .serve(server)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap()
        });
        let client = ().serve(client_transport).await.unwrap();
        let request = |name: &str, args: Value| {
            let mut r = CallToolRequestParams::new(name.to_owned())
                .with_arguments(args.as_object().unwrap().clone());
            if stable {
                r.meta = Some(
                    serde_json::from_value(json!({"openai/session":"workspace-switch"})).unwrap(),
                );
            }
            r
        };
        let brief = client
            .call_tool(request("get_agent_brief", json!({})))
            .await
            .unwrap();
        assert_ne!(brief.is_error, Some(true), "{brief:?}");
        assert!(
            brief.structured_content.unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("hello")
        );
        {
            let waiting = client.call_tool(request("chat_await", json!({})));
            tokio::pin!(waiting);
            tokio::select! {
                _ = &mut waiting => panic!("Unselected chat should wait for the picker"),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            let first = client
                .call_tool(request(
                    "setup_ui_select_project",
                    json!({"path":"a","createWorktree":false}),
                ))
                .await
                .unwrap();
            assert_ne!(first.is_error, Some(true), "{first:?}");
            let first_path = first.structured_content.unwrap()["active_root"]
                .as_str()
                .unwrap()
                .to_owned();
            let selected = tokio::time::timeout(Duration::from_secs(3), &mut waiting)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                selected.structured_content.unwrap()["status"],
                "workspace_selected"
            );
            let brief = client
                .call_tool(request("get_agent_brief", json!({})))
                .await
                .unwrap();
            assert!(
                brief.structured_content.unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("Instructions for project a")
            );
            let waiting = client.call_tool(request("chat_await", json!({})));
            tokio::pin!(waiting);
            tokio::select! {
                _ = &mut waiting => panic!("Selected chat should wait for input"),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            let switch = client
                .call_tool(request(
                    "setup_ui_switch_project",
                    json!({"expectedPath":first_path}),
                ))
                .await
                .unwrap();
            assert_ne!(switch.is_error, Some(true), "{switch:?}");
            assert!(
                switch
                    .structured_content
                    .unwrap()
                    .get(WORKSPACE_CHANGE_FIELD)
                    .is_none()
            );
            let unselected = tokio::time::timeout(Duration::from_secs(3), &mut waiting)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                unselected.structured_content.as_ref().unwrap()["status"],
                "workspace_selection"
            );
            assert!(
                unselected.structured_content.as_ref().unwrap()[WORKSPACE_CHANGE_FIELD]
                    .as_str()
                    .unwrap()
                    .contains(&first_path)
            );
            let second = client
                .call_tool(request(
                    "setup_ui_select_project",
                    json!({"path":"b","createWorktree":false}),
                ))
                .await
                .unwrap();
            assert_ne!(second.is_error, Some(true), "{second:?}");
            let second_path = second.structured_content.unwrap()["active_root"]
                .as_str()
                .unwrap()
                .to_owned();
            let write = client
                .call_tool(request(
                    "write_file",
                    json!({"path":"must-not-exist.txt","content":"stale instruction"}),
                ))
                .await
                .unwrap();
            assert_eq!(write.is_error, Some(true));
            let notice = write.structured_content.unwrap()[WORKSPACE_CHANGE_FIELD]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                notice.contains(&first_path)
                    && notice.contains(&second_path)
                    && notice.contains("get_agent_brief")
            );
            assert!(
                !std::path::Path::new(&first_path)
                    .join("must-not-exist.txt")
                    .exists()
            );
            assert!(
                !std::path::Path::new(&second_path)
                    .join("must-not-exist.txt")
                    .exists()
            );
            let brief = client
                .call_tool(request("get_agent_brief", json!({})))
                .await
                .unwrap();
            assert!(
                brief.structured_content.unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("Instructions for project b")
            );
            let write = client
                .call_tool(request(
                    "write_file",
                    json!({"path":"new.txt","content":"new instructions"}),
                ))
                .await
                .unwrap();
            assert_ne!(write.is_error, Some(true), "{write:?}");
            assert!(
                write
                    .structured_content
                    .unwrap()
                    .get(WORKSPACE_CHANGE_FIELD)
                    .is_none()
            );
            assert_eq!(
                std::fs::read_to_string(std::path::Path::new(&second_path).join("new.txt"))
                    .unwrap(),
                "new instructions"
            );
            assert!(
                std::path::Path::new(&first_path)
                    .join("AGENTS.md")
                    .is_file()
            );
        }
        client.cancel().await.unwrap();
        server_task.await.unwrap();
    }
}

#[tokio::test]
async fn workspace_switch_notices_are_present_even_without_markdown_chat() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(root.path().join("projects"));
    std::fs::create_dir_all(config.work_dir.join("a")).unwrap();
    config.multi_project = true;
    config.worktrees.mode = crate::types::WorktreeMode::Never;
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    let identity = ConversationIdentity::from_openai_session("plain").unwrap();
    let selected = handler
        .project_bindings
        .select_project_root(&config, &identity, "a")
        .await
        .unwrap();
    handler
        .project_bindings
        .switch_to_picker(&config, &identity, &selected.project_root)
        .await
        .unwrap();
    handler.config = Arc::new(config.clone());
    let tool = handler
        .tools
        .iter()
        .find(|t| t.name() == "clock_curr_time")
        .unwrap();
    let advertised = advertised_tool(tool.as_ref(), &config);
    let (server, client_transport) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move {
        handler
            .serve(server)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let mut request = CallToolRequestParams::new("clock_curr_time");
    request.meta = Some(serde_json::from_value(json!({"openai/session":"plain"})).unwrap());
    let result = client.call_tool(request).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    assert!(
        output[WORKSPACE_CHANGE_FIELD]
            .as_str()
            .unwrap()
            .contains("no longer selected")
    );
    assert!(jsonschema::is_valid(
        &serde_json::to_value(advertised.output_schema.unwrap()).unwrap(),
        &output
    ));
    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn tunnel_routes_attribute_anonymous_discovery_without_cross_tunnel_or_chat_updates() {
    use rmcp::transport::StreamableHttpClientTransport;
    let root = tempfile::tempdir().unwrap();
    let first_store = Arc::new(crate::connector_schema::ConnectorSchemaStore::new(
        Some(root.path().join("schema-first")),
        true,
    ));
    let second_store = Arc::new(crate::connector_schema::ConnectorSchemaStore::new(
        Some(root.path().join("schema-second")),
        true,
    ));
    let first_settings = crate::types::OpenAiTunnelConfig {
        tunnel_id: "tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        api_key_ref: "env:NOT_USED".into(),
        organization_id: None,
        client_path: None,
    };
    let second_settings = crate::types::OpenAiTunnelConfig {
        tunnel_id: "tunnel_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ..first_settings.clone()
    };
    let path_a = crate::openai_tunnel::local_mcp_path(&first_settings);
    let path_b = crate::openai_tunnel::local_mcp_path(&second_settings);
    assert_ne!(path_a, path_b);
    let make = |store: Arc<crate::connector_schema::ConnectorSchemaStore>| {
        let root = root.path().to_path_buf();
        let mut config =
            rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
        config.json_response = true;
        StreamableHttpService::new(
            move || {
                let mut handler = handler_with_tools(
                    &root,
                    vec![Box::new(ConnectorSchemaProbe)],
                    crate::types::ToolLogLevel::Info,
                );
                handler.connector_schemas = store.clone();
                Ok(handler)
            },
            Arc::new(LocalSessionManager::default()),
            config,
        )
    };
    let app = Router::new()
        .nest_service(
            "/mcp",
            make(Arc::new(
                crate::connector_schema::ConnectorSchemaStore::default(),
            )),
        )
        .nest_service(&path_a, make(first_store.clone()))
        .nest_service(&path_b, make(second_store.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let a = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{address}{path_a}"
        )))
        .await
        .unwrap();
    let b = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{address}{path_b}"
        )))
        .await
        .unwrap();
    assert!(
        first_store.connector_version(None).is_none(),
        "initialize is not a schema refresh"
    );
    assert!(second_store.connector_version(None).is_none());
    let conversation = ConversationIdentity::from_openai_session("old-schema").unwrap();
    first_store
        .remember_conversation_version(&conversation, "old")
        .unwrap();
    a.list_tools(None).await.unwrap();
    assert_eq!(
        first_store.connector_version(None).as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(second_store.connector_version(None).is_none());
    let probe = b
        .call_tool(CallToolRequestParams::new("connector_schema_probe"))
        .await
        .unwrap();
    assert_eq!(probe.structured_content.unwrap()["content"], "unknown");
    assert!(
        second_store.connector_version(None).is_none(),
        "ordinary tools are not refreshes"
    );
    b.list_tools(None).await.unwrap();
    let probe = b
        .call_tool(CallToolRequestParams::new("connector_schema_probe"))
        .await
        .unwrap();
    assert_eq!(
        probe.structured_content.unwrap()["content"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        first_store.conversation_version(&conversation).as_deref(),
        Some("old")
    );
    a.cancel().await.unwrap();
    b.cancel().await.unwrap();
    server.abort();
}
