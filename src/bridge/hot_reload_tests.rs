//! Isolated, real HTTP MCP fault-injection coverage for per-source reload.
use super::*;
use axum::Router;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolResponse, Implementation, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Fixture {
    version: &'static str,
    new_tool: bool,
    list_delay: Duration,
    call_delay: Duration,
    started: Arc<AtomicUsize>,
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hot-reload-fixture", self.version))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        tokio::time::sleep(self.list_delay).await;
        let schema = serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
        .as_object()
        .cloned()
        .unwrap();
        let mut tools = vec![
            rmcp::model::Tool::new("probe", "Report this fixture's version", schema.clone()),
            rmcp::model::Tool::new(
                "slow",
                "Report this fixture's version after a delay",
                schema.clone(),
            ),
        ];
        if self.new_tool {
            tools.push(rmcp::model::Tool::new(
                "new_tool",
                "Added in version two",
                schema,
            ));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        if request.name.as_ref() == "slow" {
            tokio::time::sleep(self.call_delay).await;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(self.version)]).into())
    }
}

async fn fixture(
    version: &'static str,
    new_tool: bool,
    list_delay: Duration,
    call_delay: Duration,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture {
        version,
        new_tool,
        list_delay,
        call_delay,
        started: calls.clone(),
    };
    let mut transport = StreamableHttpServerConfig::default();
    transport.json_response = true;
    let service = StreamableHttpService::new(
        move || Ok(fixture.clone()),
        Arc::new(LocalSessionManager::default()),
        transport,
    );
    let app = Router::new().nest_service("/mcp", service);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}/mcp"), calls, task)
}

fn spec(url: &str, exposure: McpToolExposure) -> crate::types::McpServerSpec {
    crate::types::McpServerSpec {
        url: Some(url.to_string()),
        mode: Some(exposure),
        ..Default::default()
    }
}

async fn invoke(view: Arc<hot_reload::BridgeView>, name: &str) -> ToolResult {
    invoke_args(view, name, json!({})).await
}

async fn invoke_args(view: Arc<hot_reload::BridgeView>, name: &str, args: Value) -> ToolResult {
    let tool = view
        .tools
        .iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("tool {name} missing from view"));
    tool.call(
        args,
        &crate::config::default_config(std::env::temp_dir()),
        &SessionState::new(),
    )
    .await
}

#[tokio::test]
async fn single_source_reload_updates_tools_but_not_other_mcp() {
    let (old_url, _, old_task) = fixture("1", false, Duration::ZERO, Duration::ZERO).await;
    let (new_url, _, new_task) = fixture("2", true, Duration::ZERO, Duration::ZERO).await;
    let (other_url, _, other_task) = fixture("other", false, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("alpha".into(), spec(&old_url, McpToolExposure::Direct));
    config
        .mcp_servers
        .insert("beta".into(), spec(&other_url, McpToolExposure::Direct));
    let bridge = connect_upstreams(&config).await;
    let runtime = bridge.runtime.clone();
    let before = runtime.snapshot();
    assert_eq!(
        invoke(before.clone(), "alpha__probe").await.joined_text(),
        "1"
    );
    assert_eq!(
        invoke(before.clone(), "beta__probe").await.joined_text(),
        "other"
    );

    let result = runtime
        .reload_with_spec("alpha", spec(&new_url, McpToolExposure::Direct), None)
        .await
        .unwrap();
    assert_eq!(result["outcome"], "reloaded");
    assert_eq!(result["previousGeneration"], 1);
    assert_eq!(result["replacementGeneration"], 2);
    assert_eq!(result["previousVersion"], "1");
    assert_eq!(result["replacementVersion"], "2");
    assert_eq!(result["toolChanges"]["added"], 1);
    assert_eq!(result["oldCallsDrained"], true);

    let after = runtime.snapshot();
    assert!(Arc::ptr_eq(&before.sources["beta"], &after.sources["beta"]));
    assert!(!Arc::ptr_eq(
        &before.sources["alpha"],
        &after.sources["alpha"]
    ));
    assert_eq!(
        invoke(after.clone(), "alpha__probe").await.joined_text(),
        "2"
    );
    assert_eq!(
        invoke(after.clone(), "alpha__new_tool").await.joined_text(),
        "2"
    );
    assert_eq!(invoke(after, "beta__probe").await.joined_text(), "other");
    assert_eq!(invoke(before, "alpha__probe").await.joined_text(), "1");
    old_task.abort();
    new_task.abort();
    other_task.abort();
}

#[tokio::test]
async fn active_calls_keep_their_original_generation_without_replay() {
    let (old_url, old_count, old_task) =
        fixture("1", false, Duration::ZERO, Duration::from_millis(450)).await;
    let (new_url, new_count, new_task) = fixture("2", true, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("alpha".into(), spec(&old_url, McpToolExposure::Direct));
    let runtime = connect_upstreams(&config).await.runtime;
    let initial = runtime.snapshot();
    let pending = tokio::spawn(invoke(initial, "alpha__slow"));
    tokio::time::timeout(Duration::from_secs(2), async {
        while old_count.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let result = runtime
        .reload_with_spec("alpha", spec(&new_url, McpToolExposure::Direct), None)
        .await
        .unwrap();
    assert_eq!(result["oldCallsDrained"], true);
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(pending.await.unwrap().joined_text(), "1");
    assert_eq!(old_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        invoke(runtime.snapshot(), "alpha__probe")
            .await
            .joined_text(),
        "2"
    );
    assert_eq!(new_count.load(Ordering::SeqCst), 1);
    old_task.abort();
    new_task.abort();
}

#[tokio::test]
async fn failed_replacement_and_unknown_ids_cannot_mutate_active_generation() {
    let (old_url, _, old_task) = fixture("1", false, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("alpha".into(), spec(&old_url, McpToolExposure::Direct));
    let runtime = connect_upstreams(&config).await.runtime;
    let old = runtime.snapshot();

    assert_eq!(
        runtime
            .reload_with_spec(
                "missing",
                spec("http://127.0.0.1:9/mcp", McpToolExposure::Direct),
                None
            )
            .await
            .unwrap_err(),
        "SOURCE_UNKNOWN"
    );
    assert_eq!(
        runtime
            .reload_with_spec(
                "alpha",
                spec("http://127.0.0.1:9/mcp", McpToolExposure::Direct),
                None
            )
            .await
            .unwrap_err(),
        "INITIALIZE_FAILED"
    );
    let after = runtime.snapshot();
    assert!(Arc::ptr_eq(&old, &after));
    assert_eq!(invoke(after, "alpha__probe").await.joined_text(), "1");
    old_task.abort();
}

#[tokio::test]
async fn same_source_reload_conflicts_while_other_source_can_reload() {
    let (a1, _, first_task) = fixture("1", false, Duration::ZERO, Duration::ZERO).await;
    let (a2, _, second_task) = fixture("2", true, Duration::from_millis(300), Duration::ZERO).await;
    let (b1, _, third_task) = fixture("other1", false, Duration::ZERO, Duration::ZERO).await;
    let (b2, _, fourth_task) = fixture("other2", false, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("alpha".into(), spec(&a1, McpToolExposure::Direct));
    config
        .mcp_servers
        .insert("beta".into(), spec(&b1, McpToolExposure::Direct));
    let runtime = connect_upstreams(&config).await.runtime;
    let clone = runtime.clone();
    let initial = tokio::spawn(async move {
        clone
            .reload_with_spec("alpha", spec(&a2, McpToolExposure::Direct), None)
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        runtime
            .reload_with_spec("alpha", spec(&a1, McpToolExposure::Direct), None)
            .await
            .unwrap_err(),
        "RELOAD_IN_PROGRESS"
    );
    let other = runtime
        .reload_with_spec("beta", spec(&b2, McpToolExposure::Direct), None)
        .await
        .unwrap();
    assert_eq!(other["replacementGeneration"], 2);
    assert_eq!(initial.await.unwrap().unwrap()["replacementGeneration"], 2);
    let view = runtime.snapshot();
    assert_eq!(
        invoke(view.clone(), "alpha__probe").await.joined_text(),
        "2"
    );
    assert_eq!(invoke(view, "beta__probe").await.joined_text(), "other2");
    first_task.abort();
    second_task.abort();
    third_task.abort();
    fourth_task.abort();
}

#[tokio::test]
async fn catalog_reload_refreshes_search_schema_and_call_routing() {
    let (old_url, _, old_task) = fixture("1", false, Duration::ZERO, Duration::ZERO).await;
    let (new_url, _, new_task) = fixture("2", true, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("catalog".into(), spec(&old_url, McpToolExposure::Catalog));
    let runtime = connect_upstreams(&config).await.runtime;
    let before = runtime.snapshot();
    let source = invoke(before, "mcp_list_sources")
        .await
        .structured_content
        .unwrap();
    assert_eq!(source["sources"][0]["toolCount"], 2);
    let result = runtime
        .reload_with_spec("catalog", spec(&new_url, McpToolExposure::Catalog), None)
        .await
        .unwrap();
    assert_eq!(result["toolChanges"]["added"], 1);
    let after = runtime.snapshot();
    let list = invoke(after.clone(), "mcp_list_sources")
        .await
        .structured_content
        .unwrap();
    assert_eq!(list["sources"][0]["toolCount"], 3);
    let source_id = list["sources"][0]["id"].as_str().unwrap();
    let found = invoke_args(
        after.clone(),
        "mcp_search_tools",
        json!({"query":"new_tool","source":source_id}),
    )
    .await
    .structured_content
    .unwrap();
    assert!(found["total"].as_u64().unwrap_or(0) >= 1);
    let tool_id = found["matches"][0]["toolId"].as_str().unwrap();
    let call = invoke_args(
        after,
        "mcp_call_tool",
        json!({"source":source_id,"tool":tool_id,"arguments":{}}),
    )
    .await;
    assert!(!call.is_error);
    assert_eq!(call.joined_text(), "2");
    old_task.abort();
    new_task.abort();
}

#[tokio::test]
async fn gateway_reload_reflects_added_functions() {
    let (old_url, _, old_task) = fixture("1", false, Duration::ZERO, Duration::ZERO).await;
    let (new_url, _, new_task) = fixture("2", true, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("gateway".into(), spec(&old_url, McpToolExposure::Gateway));
    let runtime = connect_upstreams(&config).await.runtime;
    let before = runtime.snapshot();
    let old = invoke_args(
        before,
        "gateway",
        json!({"function":"probe","arguments":{}}),
    )
    .await;
    assert_eq!(old.joined_text(), "1");
    runtime
        .reload_with_spec("gateway", spec(&new_url, McpToolExposure::Gateway), None)
        .await
        .unwrap();
    let after = runtime.snapshot();
    let tool = after
        .tools
        .iter()
        .find(|tool| tool.name() == "gateway")
        .unwrap();
    assert!(
        tool.input_schema()["properties"]["function"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("new_tool"))
    );
    let new = invoke_args(
        after,
        "gateway",
        json!({"function":"new_tool","arguments":{}}),
    )
    .await;
    assert_eq!(new.joined_text(), "2");
    old_task.abort();
    new_task.abort();
}

#[tokio::test]
async fn reconnect_failed_initial_source_and_cancel_without_switching() {
    let (url, _, task) = fixture("recovered", false, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config.mcp_servers.insert(
        "unavailable".into(),
        crate::types::McpServerSpec {
            url: Some("http://127.0.0.1:9/mcp".into()),
            startup_timeout_sec: Some(0.5),
            ..Default::default()
        },
    );
    let runtime = connect_upstreams(&config).await.runtime;
    assert!(!runtime.snapshot().sources.contains_key("unavailable"));
    let result = runtime
        .reload_with_spec("unavailable", spec(&url, McpToolExposure::Direct), None)
        .await
        .unwrap();
    assert_eq!(result["previousGeneration"], 0);
    assert_eq!(result["replacementGeneration"], 1);
    assert_eq!(
        invoke(runtime.snapshot(), "unavailable__probe")
            .await
            .joined_text(),
        "recovered"
    );

    let old = runtime.snapshot();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        runtime
            .reload_with_spec(
                "unavailable",
                spec(&url, McpToolExposure::Direct),
                Some(&cancelled)
            )
            .await
            .unwrap_err(),
        "CANCELLED"
    );
    assert!(Arc::ptr_eq(&old, &runtime.snapshot()));
    task.abort();
}

#[tokio::test]
async fn repeated_reloads_keep_one_active_generation_per_source() {
    let (url, _, task) = fixture("stable", false, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config
        .mcp_servers
        .insert("alpha".into(), spec(&url, McpToolExposure::Direct));
    let runtime = connect_upstreams(&config).await.runtime;
    for expected in 2..=6 {
        let result = runtime
            .reload_with_spec("alpha", spec(&url, McpToolExposure::Direct), None)
            .await
            .unwrap();
        assert_eq!(result["replacementGeneration"], expected);
        assert_eq!(result["oldCallsDrained"], true);
        let view = runtime.snapshot();
        assert_eq!(view.sources.len(), 1);
        assert_eq!(invoke(view, "alpha__probe").await.joined_text(), "stable");
    }
    task.abort();
}

#[tokio::test]
async fn direct_name_collisions_are_not_ambiguous_after_reload() {
    let (url1, _, task1) = fixture("one", false, Duration::ZERO, Duration::ZERO).await;
    let (url2, _, task2) = fixture("two", true, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    // sanitize("dup!")==sanitize("dup?"), so both would generate dup___probe.
    config
        .mcp_servers
        .insert("dup!".into(), spec(&url1, McpToolExposure::Direct));
    config
        .mcp_servers
        .insert("dup?".into(), spec(&url2, McpToolExposure::Direct));
    let runtime = connect_upstreams(&config).await.runtime;
    let view = runtime.snapshot();
    let names = view
        .tools
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>();
    let unique = names
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), names.len());
    assert_eq!(
        names.iter().filter(|name| **name == "dup___probe").count(),
        1
    );
    task1.abort();
    task2.abort();
}

#[tokio::test]
async fn old_generation_is_released_after_draining_without_stale_snapshots() {
    let (old_url, _, old_task) =
        fixture("old", false, Duration::ZERO, Duration::ZERO).await;
    let (new_url, _, new_task) =
        fixture("new", true, Duration::ZERO, Duration::ZERO).await;
    let mut config = crate::config::default_config(std::env::temp_dir());
    config.mcp_servers.insert("alpha".into(), spec(&old_url, McpToolExposure::Direct));
    let bridge = connect_upstreams(&config).await;
    let runtime = bridge.runtime.clone();
    let first = runtime.snapshot();
    let weak = Arc::downgrade(&first.sources["alpha"]);
    drop(first);
    // The service keeps only the runtime after consuming the startup report.
    drop(bridge);
    assert!(weak.upgrade().is_some());

    let result = runtime.reload_with_spec(
        "alpha", spec(&new_url, McpToolExposure::Direct), None
    ).await.unwrap();
    assert_eq!(result["oldCallsDrained"], true);
    assert!(weak.upgrade().is_none(), "retired service generation leaked");
    assert_eq!(invoke(runtime.snapshot(), "alpha__probe").await.joined_text(), "new");
    old_task.abort();
    new_task.abort();
}
