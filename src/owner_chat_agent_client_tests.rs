struct CodingAppBrowser;
#[async_trait::async_trait]
impl crate::agent_automation::AutomationExecutor for CodingAppBrowser {
    async fn prepare_new_chat(
        &self,
        _: Option<&str>,
        _: CancellationToken,
    ) -> Result<crate::agent_automation::CapabilityCatalog, String> {
        Ok(serde_json::from_value(json!({"protocolVersion":1,"build":format!("fixture{}",crate::agent_automation::PROTOCOL_BUILD_SUFFIX),
            "accountFingerprint":"coding-app-account","catalogRevision":"coding-app-catalog","models":[{
            "id":"fixture-model","label":"Fixture model","efforts":[{"id":"high","label":"High","available":true,"reason":null}]}]})).unwrap())
    }
    async fn execute(
        &self,
        _: AutomationExecution,
        _: tokio::sync::mpsc::UnboundedSender<crate::agent_automation::CompanionEvent>,
        _: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        panic!("This fixture never sends to the real browser")
    }
}

fn coding_app_state(root: &FilePath, enabled: bool) -> OwnerState {
    let access = root.join("projects");
    std::fs::create_dir_all(access.join("repo")).unwrap();
    let mut config = crate::config::default_config(access);
    config.memory.dir = Some(root.join("state").display().to_string());
    config.conversation_auth_token = Some("e".repeat(64).into());
    config.markdown_chat.features.agent_teams = true;
    config.markdown_chat.features.external_agent_control = enabled;
    config.markdown_chat.automation.connector_name = "Test connector".into();
    config.markdown_chat.automation.descriptor_path =
        root.join("browser/descriptor.json").display().to_string();
    let automation = Some(AgentAutomationSupervisor::new(
        config.markdown_chat.automation.clone(),
        Arc::new(CodingAppBrowser),
    ));
    OwnerState {
        config: Arc::new(config.clone()),
        chats: Arc::default(),
        project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
            root.join("bindings"),
        )),
        continuations: Arc::new(
            ConversationContinuationStore::new(root.join("continuations.json")).unwrap(),
        ),
        artifacts: Arc::new(ArtifactEgressStore::new_at(
            config.artifact_egress,
            root.join("artifacts"),
        )),
        automation,
        creation_lock: Arc::default(),
        open_browser: Arc::new(|_| Ok(())),
        catalog_cache: Arc::default(),
        global: Arc::default(),
        token: crate::auth::generate_internal_bearer_token().unwrap(),
    }
}

#[tokio::test]
async fn coding_app_http_mcp_uses_scoped_auth_reconnects_and_exposes_only_agent_control() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    };
    crate::tls::ensure_crypto_provider();
    let root = tempfile::tempdir().unwrap();
    let state = coding_app_state(root.path(), true);
    let owner = state.token.clone();
    let token = agent_client::bearer(&owner);
    let supervisor = state.automation.as_ref().unwrap().clone();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let profile_path = root.path().join("connection-runtime.json");
    let port = reqwest::Url::parse(&base).unwrap().port().unwrap();
    prepare_runtime(
        root.path(),
        &RuntimeInfo {
            port,
            token: owner.clone(),
        },
    )
    .unwrap()
    .persist(&profile_path)
    .unwrap();
    let profile = agent_client::connection_profile_at(&profile_path)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&profile).unwrap();
    assert_eq!(
        parsed["headers"]["Authorization"].as_str(),
        Some(format!("Bearer {token}").as_str())
    );
    let http = reqwest::Client::new();
    let schema_url = parsed["schemaUrl"].as_str().unwrap();
    let request_url = parsed["requestUrl"].as_str().unwrap();
    for endpoint in [schema_url, request_url] {
        assert_eq!(
            http.get(endpoint).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            http.get(endpoint)
                .bearer_auth(&owner)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let schema: Value = http
        .get(schema_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(schema["name"], "agent_client");
    assert_eq!(schema["instructions"], agent_client::INSTRUCTIONS);
    assert_eq!(
        http.post(request_url)
            .bearer_auth(&token)
            .json(&json!({"action":"status"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let url = format!("{base}/mcp/agents");
    for supplied in [None, Some(owner.as_str()), Some("wrong-token")] {
        let mut request = http.get(&url);
        if let Some(supplied) = supplied {
            request = request.bearer_auth(supplied);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        http.get(format!("{base}/api/chats"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(&url)
            .header(header::HOST, "attacker.test")
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let transport = || {
        StreamableHttpClientTransport::with_client(
            http.clone(),
            StreamableHttpClientTransportConfig::with_uri(url.clone()).auth_header(token.clone()),
        )
    };
    let client = ().serve(transport()).await.unwrap();
    let listed = client.list_all_tools().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "agent_client");
    assert!(
        client
            .call_tool(CallToolRequestParams::new("exec_command"))
            .await
            .is_err()
    );
    let request = |args: Value| {
        CallToolRequestParams::new("agent_client").with_arguments(args.as_object().unwrap().clone())
    };
    let invalid = client
        .call_tool(request(json!({"action":"status"})))
        .await
        .unwrap();
    assert_eq!(invalid.is_error, Some(true));
    let created=client.call_tool(request(json!({"action":"setup","requestId":"http-team","clientName":"Coding app",
        "projectPath":"repo","model":"fixture-model","effort":"high","workers":[{
            "role":"Verifier","goal":"Verify the requested change","scope":"tests","readOnly":true}]}))).await.unwrap();
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let team = created.structured_content.unwrap()["teamId"].clone();
    assert_eq!(supervisor.queued_count().await, 1);
    client.cancel().await.unwrap();
    let client = ().serve(transport()).await.unwrap();
    let recovered = client
        .call_tool(request(json!({"action":"list"})))
        .await
        .unwrap();
    assert_eq!(
        recovered.structured_content.unwrap()["teams"][0]["teamId"],
        team
    );
    let http_recovered: Value = http
        .post(request_url)
        .bearer_auth(&token)
        .json(&json!({"action":"list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(http_recovered["teams"][0]["teamId"], team);
    let stopped = client
        .call_tool(request(
            json!({"action":"stop_team","teamId":team,"requestId":"stop-all"}),
        ))
        .await
        .unwrap();
    assert_ne!(stopped.is_error, Some(true), "{stopped:?}");
    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn coding_app_http_endpoint_is_absent_until_enabled() {
    let root = tempfile::tempdir().unwrap();
    let state = coding_app_state(root.path(), false);
    let owner = state.token.clone();
    let token = agent_client::bearer(&owner);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/mcp/agents"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    for path in ["/api/agent-client/request", "/api/agent-client/schema"] {
        assert_eq!(
            client
                .get(format!("{base}{path}"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        client
            .get(format!("{base}/api/agent-client"))
            .bearer_auth(&owner)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    server.abort();
}

#[test]
fn universal_agent_connection_is_published_at_boot_refreshed_and_disabled_without_client_adapters()
{
    let root = tempfile::tempdir().unwrap();
    let mut state = coding_app_state(root.path(), false);
    let directory = root.path().join("agents");
    let mut info = RuntimeInfo {
        port: 3210,
        token: state.token.clone(),
    };
    assert!(
        agent_client::publish(&state, &info, &directory)
            .unwrap()
            .is_none()
    );
    assert!(!directory.exists());
    Arc::make_mut(&mut state.config)
        .markdown_chat
        .features
        .external_agent_control = true;
    let path = agent_client::publish(&state, &info, &directory)
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(first["enabled"], true);
    assert_eq!(first["url"], "http://127.0.0.1:3210/mcp/agents");
    assert!(directory.join("client.mjs").is_file());
    let helper = std::fs::read(directory.join("client.mjs")).unwrap();
    assert!(!first.to_string().contains(&info.token));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    info.port = 3211;
    agent_client::publish(&state, &info, &directory).unwrap();
    let updated: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(updated["headers"], first["headers"]);
    assert_eq!(
        updated["requestUrl"],
        "http://127.0.0.1:3211/api/agent-client/request"
    );
    assert_eq!(std::fs::read(directory.join("client.mjs")).unwrap(), helper);
    Arc::make_mut(&mut state.config)
        .markdown_chat
        .features
        .external_agent_control = false;
    agent_client::publish(&state, &info, &directory).unwrap();
    let disabled: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(disabled["enabled"], false);
    assert!(disabled.get("headers").is_none());
}
