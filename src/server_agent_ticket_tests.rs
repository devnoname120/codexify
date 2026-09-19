fn ticket_test_config(root: &std::path::Path, enabled: bool) -> AppConfig {
    use clap::Parser;
    let path = root.join("ticket-config.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "workDir": root,
            "experimental": {"agentTickets": enabled},
            "codexMcp": { "enabled": false }
        }))
        .unwrap(),
    )
    .unwrap();
    let cli = crate::config::Cli::parse_from(["codexify", "--config", path.to_str().unwrap()]);
    crate::config::load_config(cli).unwrap()
}

#[test]
fn agent_tickets_only_augment_enabled_model_tool_schemas() {
    let root = tempfile::tempdir().unwrap();
    let tool = SecretInputTool;
    let disabled = advertised_tool(&tool, &ticket_test_config(root.path(), false));
    assert!(
        disabled.input_schema["properties"]
            .get("codexify_ticket")
            .is_none()
    );
    let enabled = advertised_tool(&tool, &ticket_test_config(root.path(), true));
    assert_eq!(
        enabled.input_schema["properties"]["codexify_ticket"]["type"],
        "string"
    );
    assert_eq!(
        enabled.output_schema.as_ref().unwrap()["properties"]["new_codexify_ticket"]["type"],
        "string"
    );
}

struct TicketProbe {
    name: &'static str,
    calls: Arc<AtomicU64>,
    private: bool,
}

#[async_trait]
impl Tool for TicketProbe {
    fn name(&self) -> &'static str {
        self.name
    }
    fn title(&self) -> String {
        self.name.into()
    }
    fn description(&self) -> String {
        "Counts actual dispatches without project access.".into()
    }
    fn behavior(&self) -> crate::tool::ToolBehavior {
        crate::tool::ToolBehavior::new(false, false, false, false, "Increments the test counter.")
    }
    fn input_schema(&self) -> Value {
        if self.name == "collision_probe" {
            return json!({
                "type":"object",
                "properties":{"codexify_ticket":{"type":"integer"}},
                "required":["codexify_ticket"], "additionalProperties":false
            });
        }
        json!({
            "type":"object",
            "properties":{"fail":{"type":"boolean"}, "cancel":{"type":"boolean"}},
            "additionalProperties":false
        })
    }
    fn output_schema(&self) -> Option<Value> {
        if self.name == "collision_probe" {
            return Some(json!({
                "type":"object",
                "properties":{"new_codexify_ticket":{"type":"integer"}},
                "required":["new_codexify_ticket"], "additionalProperties":false
            }));
        }
        Some(crate::tool::text_output_schema())
    }
    fn meta(&self) -> Option<rmcp::model::MetaObject> {
        self.private
            .then(|| serde_json::from_value(json!({"ui":{"visibility":["app"]}})).unwrap())
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    async fn call(&self, args: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.name == "collision_probe" {
            assert_eq!(args, json!({"codexify_ticket":42}));
            return ToolResult::text("dispatched")
                .with_structured(json!({"new_codexify_ticket":42}));
        }
        assert!(args.get("codexify_ticket").is_none());
        if args["fail"] == true {
            ToolResult::error("expected failure")
        } else {
            ToolResult::text("dispatched")
        }
    }
    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        if args["cancel"] == true {
            context.cancellation.cancel();
        }
        self.call(args, config, session).await
    }
}

fn ticket_request(name: &str, ticket: Option<&str>, mut args: Value) -> CallToolRequestParams {
    if let Some(ticket) = ticket {
        args["codexify_ticket"] = json!(ticket);
    }
    CallToolRequestParams::new(name.to_string()).with_arguments(args.as_object().unwrap().clone())
}

fn next_ticket(result: &CallToolResult) -> String {
    let ticket = result.structured_content.as_ref().unwrap()["new_codexify_ticket"]
        .as_str()
        .expect("accepted calls must deliver a ticket")
        .to_string();
    assert_eq!(ticket.len(), 8);
    assert!(result.content.iter().any(|block| {
        block
            .as_text()
            .is_some_and(|text| text.text.contains(&ticket))
    }));
    ticket
}

fn assert_ticket_rejected(result: &CallToolResult) {
    assert_eq!(result.is_error, Some(true));
    assert!(result.content.iter().any(|block| {
        block
            .as_text()
            .is_some_and(|text| text.text.contains("Ticket rejected"))
    }));
    assert!(
        result
            .structured_content
            .as_ref()
            .and_then(|v| v.get("new_codexify_ticket"))
            .is_none()
    );
}

async fn ticket_client(
    root: &std::path::Path,
    calls: Arc<AtomicU64>,
    enabled: bool,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
) {
    let tools = ["ticket_probe", "setup", "widget_probe", "collision_probe"]
        .into_iter()
        .map(|name| {
            Box::new(TicketProbe {
                name,
                calls: calls.clone(),
                private: name == "widget_probe",
            }) as Box<dyn Tool>
        })
        .chain(std::iter::once(
            Box::new(InvalidStructuredOutputTool) as Box<dyn Tool>
        ))
        .collect();
    let mut handler = handler_with_tools(root, tools, crate::types::ToolLogLevel::Info);
    handler.config = Arc::new(ticket_test_config(root, enabled));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    (().serve(client_transport).await.unwrap(), task)
}

#[tokio::test]
async fn agent_tickets_reserve_once_before_dispatch_and_do_not_allow_setup_to_reset() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let (client, server) = ticket_client(root.path(), calls.clone(), true).await;
    let first = client
        .call_tool(ticket_request("setup", None, json!({})))
        .await
        .unwrap();
    assert_ne!(first.is_error, Some(true));
    let ticket = next_ticket(&first);
    let (a, b) = tokio::join!(
        client.call_tool(ticket_request("ticket_probe", Some(&ticket), json!({}))),
        client.call_tool(ticket_request("ticket_probe", Some(&ticket), json!({}))),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let (winner, loser) = if a.is_error == Some(true) {
        (b, a)
    } else {
        (a, b)
    };
    assert_ne!(winner.is_error, Some(true));
    assert_ticket_rejected(&loser);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let current = next_ticket(&winner);
    assert_ne!(ticket, current);
    assert!(!serde_json::to_string(&loser).unwrap().contains(&current));
    for name in ["setup", "ticket_probe"] {
        let reset = client
            .call_tool(ticket_request(name, None, json!({})))
            .await
            .unwrap();
        assert_ticket_rejected(&reset);
        let stale = client
            .call_tool(ticket_request(name, Some(&ticket), json!({})))
            .await
            .unwrap();
        assert_ticket_rejected(&stale);
    }
    let next = client
        .call_tool(ticket_request("ticket_probe", Some(&current), json!({})))
        .await
        .unwrap();
    assert_ne!(next.is_error, Some(true));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn agent_tickets_return_successors_on_errors_and_exempt_widget_helpers() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let (client, server) = ticket_client(root.path(), calls.clone(), true).await;
    let initial = client
        .call_tool(ticket_request("ticket_probe", None, json!({})))
        .await
        .unwrap();
    let mut ticket = next_ticket(&initial);
    let advertised = client.list_all_tools().await.unwrap();
    let widget = advertised
        .iter()
        .find(|tool| tool.name == "widget_probe")
        .unwrap();
    assert!(
        widget.input_schema["properties"]
            .get("codexify_ticket")
            .is_none()
    );
    assert!(
        widget.output_schema.as_ref().unwrap()["properties"]
            .get("new_codexify_ticket")
            .is_none()
    );
    let widget = client
        .call_tool(ticket_request("widget_probe", None, json!({})))
        .await
        .unwrap();
    assert_ne!(widget.is_error, Some(true));
    assert!(
        widget
            .structured_content
            .as_ref()
            .unwrap()
            .get("new_codexify_ticket")
            .is_none()
    );
    for (name, args) in [
        ("ticket_probe", json!({"fail":true})),
        ("ticket_probe", json!({"invalid":true})),
        ("invalid_structured_output_fixture", json!({})),
        ("unknown_tool", json!({})),
    ] {
        let result = client
            .call_tool(ticket_request(name, Some(&ticket), args))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let next = next_ticket(&result);
        assert_ne!(ticket, next);
        ticket = next;
    }
    let result = client
        .call_tool(ticket_request("ticket_probe", Some(&ticket), json!({})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn agent_tickets_disabled_preserves_ticket_free_calls_and_results() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let (client, server) = ticket_client(root.path(), calls.clone(), false).await;
    for _ in 0..3 {
        let result = client
            .call_tool(ticket_request("ticket_probe", None, json!({})))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert!(
            result
                .structured_content
                .as_ref()
                .unwrap()
                .get("new_codexify_ticket")
                .is_none()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn agent_tickets_warn_once_without_consuming_user_messages_or_recording_loser_activity() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let mut handler = handler_with_tools(
        root.path(),
        vec![Box::new(TicketProbe {
            name: "ticket_probe",
            calls: calls.clone(),
            private: false,
        })],
        crate::types::ToolLogLevel::Info,
    );
    let mut config = ticket_test_config(root.path(), true);
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    handler.config = Arc::new(config.clone());
    let chat = handler
        .markdown_chat
        .chat(&config, None, &handler.session)
        .unwrap();
    chat.ensure().await.unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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
    let first = client
        .call_tool(ticket_request("ticket_probe", None, json!({})))
        .await
        .unwrap();
    let ticket = next_ticket(&first);
    std::fs::OpenOptions::new()
        .append(true)
        .open(chat.path())
        .unwrap()
        .write_all(b"\nPending instruction for the winning agent\n")
        .unwrap();
    for _ in 0..3 {
        let rejected = client
            .call_tool(ticket_request("ticket_probe", None, json!({})))
            .await
            .unwrap();
        assert_ticket_rejected(&rejected);
        assert!(
            !serde_json::to_string(&rejected)
                .unwrap()
                .contains("Pending instruction")
        );
    }
    let transcript = std::fs::read_to_string(chat.path()).unwrap();
    assert_eq!(
        transcript
            .matches("Possible duplicate agent detected and blocked")
            .count(),
        1
    );
    assert_eq!(
        chat.read(false).await.unwrap().text.trim(),
        "Pending instruction for the winning agent"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = client
        .call_tool(ticket_request("ticket_probe", Some(&ticket), json!({})))
        .await
        .unwrap();
    assert_ne!(next.is_error, Some(true));
    assert!(
        serde_json::to_string(&next)
            .unwrap()
            .contains("Pending instruction")
    );
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn agent_tickets_preserve_previous_ticket_on_server_observed_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let (client, server) = ticket_client(root.path(), calls.clone(), true).await;
    let first = client
        .call_tool(ticket_request("ticket_probe", None, json!({})))
        .await
        .unwrap();
    let ticket = next_ticket(&first);
    let cancelled = client
        .call_tool(ticket_request(
            "ticket_probe",
            Some(&ticket),
            json!({"cancel":true}),
        ))
        .await
        .unwrap();
    assert_eq!(cancelled.is_error, Some(true));
    assert!(
        cancelled
            .structured_content
            .as_ref()
            .and_then(|v| v.get("new_codexify_ticket"))
            .is_none()
    );
    assert!(
        cancelled.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("ticket is unchanged")
    );
    let next = client
        .call_tool(ticket_request("ticket_probe", Some(&ticket), json!({})))
        .await
        .unwrap();
    assert_ne!(next.is_error, Some(true));
    assert_ne!(next_ticket(&next), ticket);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[test]
fn agent_tickets_native_schemas_are_valid_with_chat_and_workspace_fields() {
    let root = tempfile::tempdir().unwrap();
    let mut config = ticket_test_config(root.path(), true);
    config.multi_project = true;
    config.markdown_chat.enabled = true;
    config.conversation_auth_token = Some("a".repeat(64).into());
    for tool in crate::registry::load_tools_for_config(&config) {
        let advertised = advertised_tool(tool.as_ref(), &config);
        jsonschema::validator_for(&Value::Object((*advertised.input_schema).clone())).unwrap();
        jsonschema::validator_for(&Value::Object(
            (**advertised.output_schema.as_ref().unwrap()).clone(),
        ))
        .unwrap();
        assert_eq!(
            advertised.input_schema["properties"]
                .get("codexify_ticket")
                .is_some(),
            !app_only_tool(tool.as_ref()),
            "{}",
            tool.name()
        );
        if !app_only_tool(tool.as_ref()) {
            assert_eq!(
                advertised.annotations.as_ref().unwrap().idempotent_hint,
                Some(false)
            );
        }
    }
}

#[tokio::test]
async fn agent_tickets_survive_fresh_handlers_and_transport_replacement() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let mut current = None;
    for _ in 0..3 {
        let mut handler = handler_with_tools(
            root.path(),
            vec![Box::new(TicketProbe {
                name: "ticket_probe",
                calls: calls.clone(),
                private: false,
            })],
            crate::types::ToolLogLevel::Info,
        );
        Arc::make_mut(&mut handler.config)
            .experimental
            .agent_tickets = true;
        handler.agent_tickets = Arc::new(AgentTicketStore::persistent(root.path().join("tickets")));
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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
        let request = |ticket: Option<&str>| {
            let mut value =
                serde_json::to_value(ticket_request("ticket_probe", ticket, json!({}))).unwrap();
            value["_meta"] = json!({"openai/session":"persistent-ticket-conversation"});
            serde_json::from_value::<CallToolRequestParams>(value).unwrap()
        };
        if current.is_some() {
            assert_ticket_rejected(&client.call_tool(request(None)).await.unwrap());
        }
        let result = client.call_tool(request(current.as_deref())).await.unwrap();
        assert_ne!(result.is_error, Some(true));
        current = Some(next_ticket(&result));
        client.cancel().await.unwrap();
        server.await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn agent_tickets_widget_helpers_do_not_require_markdown_chat() {
    let root = tempfile::tempdir().unwrap();
    for markdown_chat in [false, true] {
        let mut config = ticket_test_config(root.path(), true);
        config.multi_project = true;
        config.markdown_chat.enabled = markdown_chat;
        let tools = crate::registry::load_tools_for_config(&config);
        for name in [
            "setup_ui_list_projects",
            "setup_ui_select_project",
            "setup_ui_update",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name() == name)
                .expect("ticket mode needs widget-only actions independently of Markdown chat");
            assert!(app_only_tool(tool.as_ref()));
            let advertised = advertised_tool(tool.as_ref(), &config);
            assert!(
                advertised.input_schema["properties"]
                    .get("codexify_ticket")
                    .is_none()
            );
            assert!(
                advertised.output_schema.as_ref().unwrap()["properties"]
                    .get("new_codexify_ticket")
                    .is_none()
            );
        }
        config.ui_widgets = false;
        assert!(
            !crate::registry::load_tools_for_config(&config)
                .iter()
                .any(|tool| tool.name().starts_with("setup_ui_"))
        );
    }
}

#[tokio::test]
async fn agent_tickets_remain_available_when_output_budget_is_tiny() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let mut handler = handler_with_tools(
        root.path(),
        vec![Box::new(TicketProbe {
            name: "ticket_probe",
            calls: calls.clone(),
            private: false,
        })],
        crate::types::ToolLogLevel::Info,
    );
    let config = Arc::make_mut(&mut handler.config);
    config.experimental.agent_tickets = true;
    config.output.max_tool_output_tokens = Some(1);
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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
    let first = client
        .call_tool(ticket_request("ticket_probe", None, json!({})))
        .await
        .unwrap();
    assert_eq!(first.is_error, Some(true));
    let first_ticket = next_ticket(&first);
    let second = client
        .call_tool(ticket_request(
            "ticket_probe",
            Some(&first_ticket),
            json!({}),
        ))
        .await
        .unwrap();
    assert_ne!(next_ticket(&second), first_ticket);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn agent_tickets_preserve_colliding_fields_through_dispatch_and_validation() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let (client, server) = ticket_client(root.path(), calls.clone(), true).await;
    let mut ticket = None;
    for args in [
        json!({}),
        json!({"arguments":42}),
        json!({"arguments":{"codexify_ticket":42}, "extra":true}),
        json!({"arguments":{"codexify_ticket":"wrong type"}}),
    ] {
        let result = client
            .call_tool(ticket_request("collision_probe", ticket.as_deref(), args))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        ticket = Some(next_ticket(&result));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let result = client
        .call_tool(ticket_request(
            "collision_probe",
            ticket.as_deref(),
            json!({"arguments":{"codexify_ticket":42}}),
        ))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_ne!(Some(next_ticket(&result)), ticket);
    assert_eq!(
        result.structured_content.as_ref().unwrap()["upstream_result"]["new_codexify_ticket"],
        42
    );
    let tool = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|tool| tool.name == "collision_probe")
        .unwrap();
    assert!(jsonschema::is_valid(
        &Value::Object((**tool.output_schema.as_ref().unwrap()).clone()),
        result.structured_content.as_ref().unwrap()
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn agent_tickets_elect_one_branch_across_independent_handlers() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut handler = handler_with_tools(
            root.path(),
            vec![Box::new(TicketProbe {
                name: "ticket_probe",
                calls: calls.clone(),
                private: false,
            })],
            crate::types::ToolLogLevel::Info,
        );
        Arc::make_mut(&mut handler.config)
            .experimental
            .agent_tickets = true;
        handler.agent_tickets = Arc::new(AgentTicketStore::persistent(root.path().join("tickets")));
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            handler
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        clients.push((().serve(client_transport).await.unwrap(), server));
    }
    let request = |ticket: Option<&str>| {
        let mut value =
            serde_json::to_value(ticket_request("ticket_probe", ticket, json!({}))).unwrap();
        value["_meta"] = json!({"openai/session":"forked-ticket-conversation"});
        serde_json::from_value::<CallToolRequestParams>(value).unwrap()
    };
    let mut ticket = None;
    for expected_calls in 1..=4 {
        if expected_calls >= 3 {
            let identity =
                ConversationIdentity::from_openai_session("forked-ticket-conversation").unwrap();
            let path = root
                .path()
                .join("tickets")
                .join(format!("{}.ticket", identity.stable_key()));
            let offline = std::time::SystemTime::now()
                - Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS + 1);
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(offline)
                .unwrap();
            ticket = (expected_calls == 4).then(|| "oldstate".to_string());
        }
        let (a, b) = tokio::join!(
            clients[0].0.call_tool(request(ticket.as_deref())),
            clients[1].0.call_tool(request(ticket.as_deref())),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let (winner, loser) = if a.is_error == Some(true) {
            (b, a)
        } else {
            (a, b)
        };
        assert_ne!(winner.is_error, Some(true));
        assert_ticket_rejected(&loser);
        ticket = Some(next_ticket(&winner));
        assert!(
            !serde_json::to_string(&loser)
                .unwrap()
                .contains(ticket.as_ref().unwrap())
        );
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
    let result = clients[0]
        .0
        .call_tool(request(ticket.as_deref()))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_ne!(Some(next_ticket(&result)), ticket);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    for (client, server) in clients {
        client.cancel().await.unwrap();
        server.await.unwrap();
    }
}
