//! Transactional replacement of one upstream MCP connection.
//!
//! All model-visible tools are rebuilt as one immutable registry snapshot. A
//! dispatch keeps its snapshot for its entire call, so old generations cannot
//! be disposed while requests are using them.
use super::*;
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{Mutex as AsyncMutex, Notify};

const MAX_INTERNED_TOOL_NAMES: usize = 4096;
const REPLACEMENT_PING_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Generation {
    pub(super) spec: crate::types::McpServerSpec,
    pub(super) peer: Peer<RoleClient>,
    pub(super) tools: Vec<rmcp::model::Tool>,
    pub(super) timeout: Option<Duration>,
    pub(super) control: Arc<crate::upstream_control::UpstreamControl>,
    pub(super) number: u64,
    // Keep the transport alive until every snapshot/call using it is dropped.
    _service: Mutex<RunningService<RoleClient, ()>>,
    _monitor: UpstreamMonitor,
    calls: Mutex<usize>,
    drained: Notify,
}

pub(crate) struct CallLease<'a>(&'a Generation);

impl Drop for CallLease<'_> {
    fn drop(&mut self) {
        let mut calls = self.0.calls.lock().unwrap();
        *calls -= 1;
        if *calls == 0 {
            self.0.drained.notify_one();
        }
    }
}

impl Generation {
    pub(super) fn new(
        spec: crate::types::McpServerSpec,
        service: RunningService<RoleClient, ()>,
        tools: Vec<rmcp::model::Tool>,
        timeout: Option<Duration>,
        number: u64,
    ) -> Arc<Self> {
        let peer = service.peer().clone();
        let control = Arc::new(crate::upstream_control::UpstreamControl::new(
            spec.request_limits.clone(),
        ));
        let monitor = monitor_upstream(peer.clone(), control.clone());
        Arc::new(Self {
            spec,
            peer,
            tools,
            timeout,
            control,
            number,
            _service: Mutex::new(service),
            _monitor: monitor,
            calls: Mutex::new(0),
            drained: Notify::new(),
        })
    }

    pub(crate) fn begin_call(&self) -> CallLease<'_> {
        *self.calls.lock().unwrap() += 1;
        CallLease(self)
    }

    async fn wait_for_calls(&self, limit: Duration) -> bool {
        tokio::time::timeout(limit, async {
            loop {
                if *self.calls.lock().unwrap() == 0 {
                    break;
                }
                self.drained.notified().await;
            }
        })
        .await
        .is_ok()
    }

    fn version(&self) -> Option<String> {
        self.peer.peer_info().and_then(|info| {
            info.server_info
                .as_ref()
                .map(|info| info.version.to_string())
        })
    }
}

struct NamePool(HashMap<String, &'static str>);

impl NamePool {
    fn intern(&mut self, name: String) -> Result<&'static str, &'static str> {
        if let Some(value) = self.0.get(&name) {
            return Ok(*value);
        }
        if self.0.len() >= MAX_INTERNED_TOOL_NAMES {
            return Err("TOOL_NAME_LIMIT");
        }
        let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
        self.0.insert(name, leaked);
        Ok(leaked)
    }
}

pub(crate) struct BridgeView {
    pub(crate) tools: Arc<Vec<Box<dyn Tool>>>,
    pub(crate) sources: HashMap<String, Arc<Generation>>,
}

pub(crate) struct HotReloadRuntime {
    view: RwLock<Arc<BridgeView>>,
    config: Arc<AppConfig>,
    resources: Arc<BridgedResourceStore>,
    names: Mutex<NamePool>,
    locks: HashMap<String, Arc<AsyncMutex<()>>>,
    pub(crate) upstreams:
        Arc<RwLock<HashMap<String, Arc<crate::upstream_control::UpstreamControl>>>>,
}

impl HotReloadRuntime {
    pub(super) fn new(
        config: Arc<AppConfig>,
        resources: Arc<BridgedResourceStore>,
        sources: HashMap<String, Arc<Generation>>,
    ) -> Arc<Self> {
        let upstreams = sources
            .iter()
            .map(|(name, generation)| (name.clone(), generation.control.clone()))
            .collect();
        let locks = config
            .mcp_servers
            .keys()
            .map(|name| (name.clone(), Arc::new(AsyncMutex::new(()))))
            .collect();
        let runtime = Arc::new(Self {
            view: RwLock::new(Arc::new(BridgeView {
                tools: Arc::new(Vec::new()),
                sources: sources.clone(),
            })),
            config,
            resources,
            names: Mutex::new(NamePool(HashMap::new())),
            locks,
            upstreams: Arc::new(RwLock::new(upstreams)),
        });
        match runtime.build_view(sources) {
            Ok(view) => *runtime.view.write().unwrap() = Arc::new(view),
            Err(code) => tracing::error!(%code, "initial MCP registry could not be built"),
        }
        runtime
    }

    pub(crate) fn snapshot(&self) -> Arc<BridgeView> {
        self.view.read().unwrap().clone()
    }

    fn build_view(
        &self,
        sources: HashMap<String, Arc<Generation>>,
    ) -> Result<BridgeView, &'static str> {
        let mut exposed = Vec::new();
        let mut catalog = Vec::new();
        let mut names = self.names.lock().unwrap();
        let mut sorted: Vec<_> = sources.keys().collect();
        sorted.sort();
        for name in sorted {
            let source = &sources[name];
            let sanitized = sanitize(name);
            match source.spec.exposure() {
                McpToolExposure::Gateway => {
                    let functions = gateway_functions(&source.tools);
                    let tool = GatewayTool {
                        name: names.intern(sanitized)?,
                        server: name.clone(),
                        description: gateway_description(name, &functions),
                        function_names: functions.iter().map(|tool| tool.name.clone()).collect(),
                        peer: source.peer.clone(),
                        tool_timeout: source.timeout,
                        control: source.control.clone(),
                        resources: self.resources.clone(),
                        generation: source.clone(),
                    };
                    exposed.push(Box::new(tool) as Box<dyn Tool>);
                }
                McpToolExposure::Direct => {
                    let mut used = HashSet::new();
                    for tool in &source.tools {
                        let raw_name = tool.name.to_string();
                        let display = unique_name(bridged_name(&sanitized, &raw_name), &used);
                        used.insert(display.clone());
                        let description = tool
                            .description
                            .as_ref()
                            .map(ToString::to_string)
                            .filter(|description| !description.trim().is_empty())
                            .unwrap_or_else(|| {
                                format!("Call the '{raw_name}' tool on the '{name}' MCP server.")
                            });
                        exposed.push(Box::new(BridgedTool {
                            name: names.intern(display)?,
                            original_name: raw_name,
                            server: name.clone(),
                            title: tool.title.clone().filter(|value| !value.trim().is_empty()),
                            description,
                            input_schema: Value::Object((*tool.input_schema).clone()),
                            output_schema: tool
                                .output_schema
                                .as_ref()
                                .map(|schema| Value::Object((**schema).clone())),
                            annotations: tool.annotations.clone(),
                            icons: tool.icons.clone(),
                            meta: tool.meta.clone(),
                            peer: source.peer.clone(),
                            tool_timeout: source.timeout,
                            control: source.control.clone(),
                            resources: self.resources.clone(),
                            generation: source.clone(),
                        }) as Box<dyn Tool>);
                    }
                }
                McpToolExposure::Catalog => {
                    catalog.push(CatalogSourceInput {
                        raw_name: name.clone(),
                        provenance: source.spec.provenance,
                        transport: transport_label(&source.spec),
                        peer_info: source.peer.peer_info(),
                        tools: source.tools.clone(),
                        peer: source.peer.clone(),
                        tool_timeout: source.timeout,
                        control: source.control.clone(),
                        generation: source.clone(),
                    });
                }
            }
        }
        let mut tools = build_catalog_tools(catalog, self.resources.clone());
        tools.append(&mut exposed);
        let mut seen = HashSet::new();
        let tools = tools
            .into_iter()
            .filter_map(|tool| {
                if !seen.insert(tool.name()) {
                    tracing::warn!(
                        name = tool.name(),
                        "duplicate upstream MCP tool name skipped"
                    );
                    return None;
                }
                match crate::tool::validate_and_wrap_tool(tool) {
                    Ok(tool) => Some(tool),
                    Err(error) => {
                        tracing::warn!(%error, "invalid upstream MCP tool skipped");
                        None
                    }
                }
            })
            .collect();
        Ok(BridgeView {
            sources,
            tools: Arc::new(tools),
        })
    }

    fn latest_spec(&self, name: &str) -> Result<crate::types::McpServerSpec, &'static str> {
        // Parse the same CLI arguments as startup; this is read-only and never
        // rewrites a config migration. No executable or path comes from the MCP
        // request: only the operator's configured source ID is accepted.
        let cli = crate::config::Cli::try_parse_from(std::env::args_os())
            .map_err(|_| "CONFIG_CLI_INVALID")?;
        let config =
            crate::config::load_config_quiet_read_only(cli).map_err(|_| "CONFIG_INVALID")?;
        config
            .mcp_servers
            .get(name)
            .cloned()
            .ok_or("SOURCE_REMOVED")
    }

    pub(crate) async fn reload(
        &self,
        name: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, &'static str> {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err("SOURCE_ID_INVALID");
        }
        if !self.locks.contains_key(name) {
            return Err("SOURCE_UNKNOWN");
        }
        let spec = self.latest_spec(name)?;
        self.reload_with_spec(name, spec, cancellation).await
    }

    // A test harness can supply an isolated spec without reloading real
    // operator configuration. Production accepts source ID only and always
    // resolves the current configured spec before reaching this boundary.
    pub(super) async fn reload_with_spec(
        &self,
        name: &str,
        spec: crate::types::McpServerSpec,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, &'static str> {
        let lock = self.locks.get(name).ok_or("SOURCE_UNKNOWN")?;
        let _lease = lock.try_lock().map_err(|_| "RELOAD_IN_PROGRESS")?;
        self.reload_with_spec_locked(name, spec, cancellation).await
    }

    async fn reload_with_spec_locked(
        &self,
        name: &str,
        spec: crate::types::McpServerSpec,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, &'static str> {
        if spec.disabled {
            return Err("SOURCE_DISABLED");
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("CANCELLED");
        }
        let started = std::time::Instant::now();
        let previous = self.snapshot().sources.get(name).cloned();
        let old_generation = previous.as_ref().map_or(0, |source| source.number);
        let future = connect_one(name, &spec, &self.config);
        let (service, tools, timeout) = if let Some(token) = cancellation {
            tokio::select! {
                _ = token.cancelled() => return Err("CANCELLED"),
                result = future => result.map_err(|_| "INITIALIZE_FAILED")?,
            }
        } else {
            future.await.map_err(|_| "INITIALIZE_FAILED")?
        };
        let ready = matches!(
            tokio::time::timeout(
                REPLACEMENT_PING_TIMEOUT,
                service
                    .peer()
                    .send_request(ClientRequest::PingRequest(Default::default()))
            )
            .await,
            Ok(Ok(_) | Err(ServiceError::McpError(_)))
        );
        if !ready {
            return Err("READINESS_FAILED");
        }

        let added;
        let removed;
        let changed;
        let unchanged;
        {
            let before: HashMap<_, _> = previous
                .as_ref()
                .map(|generation| &generation.tools)
                .into_iter()
                .flatten()
                .map(|tool| {
                    (
                        tool.name.as_ref(),
                        serde_json::to_value(tool).unwrap_or(Value::Null),
                    )
                })
                .collect();
            let after: HashMap<_, _> = tools
                .iter()
                .map(|tool| {
                    (
                        tool.name.as_ref(),
                        serde_json::to_value(tool).unwrap_or(Value::Null),
                    )
                })
                .collect();
            added = after
                .keys()
                .filter(|key| !before.contains_key(**key))
                .count();
            removed = before
                .keys()
                .filter(|key| !after.contains_key(**key))
                .count();
            changed = after
                .iter()
                .filter(|(key, value)| before.get(**key).is_some_and(|old| old != *value))
                .count();
            unchanged = after
                .iter()
                .filter(|(key, value)| before.get(**key).is_some_and(|old| old == *value))
                .count();
        }
        let initialization_ms = started.elapsed().as_millis() as u64;
        let new = Generation::new(
            spec,
            service,
            tools,
            timeout,
            old_generation + 1,
        );
        let new_version = new.version();
        // Connection preparation and view construction are outside the write
        // lock. Compare-and-rebuild prevents concurrent distinct-source reloads
        // from overwriting each other.
        let switching = std::time::Instant::now();
        loop {
            let before = self.snapshot();
            let mut sources = before.sources.clone();
            sources.insert(name.to_owned(), new.clone());
            let candidate = Arc::new(self.build_view(sources)?);
            let mut current = self.view.write().unwrap();
            if !Arc::ptr_eq(&before, &current) {
                continue;
            }
            *current = candidate;
            break;
        }
        self.upstreams
            .write()
            .unwrap()
            .insert(name.to_owned(), new.control.clone());
        let switching_ms = switching.elapsed().as_millis() as u64;

        if new.spec.exposure() == McpToolExposure::Gateway {
            let funcs = gateway_functions(&new.tools);
            write_gateway_skill(&self.config, name, &sanitize(name), &funcs);
        }

        // The old generation stays alive in any request's registry snapshot.
        // On a bounded drain timeout its existing calls are NOT cancelled or
        // replayed; the generation retires when the last call releases it.
        let drain_ms = previous
            .as_ref()
            .map_or(0, |source| source.spec.request_limits.slow_cancellation_ms);
        let draining = std::time::Instant::now();
        let drained = if let Some(old) = &previous {
            old.wait_for_calls(Duration::from_millis(drain_ms)).await
        } else {
            true
        };
        let in_flight = previous
            .as_ref()
            .map_or(0, |old| *old.calls.lock().unwrap());
        Ok(json!({
            "source": name,
            "outcome": "reloaded",
            "previousGeneration": old_generation,
            "replacementGeneration": new.number,
            "previousVersion": previous.as_ref().and_then(|old| old.version()),
            "replacementVersion": new_version,
            "ready": true,
            "toolChanges": {
                "added": added, "removed": removed, "changed": changed, "unchanged": unchanged
            },
            "oldCallsDrained": drained,
            "oldCallsStillRunning": in_flight,
            "oldGenerationRetiring": !drained,
            "rollback": "not_required",
            "timingsMs": {
                "initializationAndReadiness": initialization_ms,
                "registrySwitch": switching_ms,
                "drain": draining.elapsed().as_millis() as u64,
                "total": started.elapsed().as_millis() as u64
            },
            "clientRediscoveryRequired": true,
            "diagnosticCode": "OK"
        }))
    }
}

pub(crate) struct ReloadMcpTool {
    pub(crate) runtime: Arc<HotReloadRuntime>,
}

#[async_trait]
impl Tool for ReloadMcpTool {
    fn name(&self) -> &'static str {
        "reload_mcp"
    }

    fn title(&self) -> String {
        "Reload one MCP source".into()
    }

    fn description(&self) -> String {
        "Transactionally replace one configured upstream MCP source without restarting Codexify or reconnecting any other source. The source is its exact configured MCP server key, not an executable path. Existing calls keep the previous generation. Tool definition changes require the client to rediscover tools/list; in-chat ChatGPT schema updates are not guaranteed.".into()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            false,
            "Process replacement is an operator action; no arbitrary executable arguments are accepted.",
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {"type": "string", "minLength": 1, "maxLength": 128,
                    "description": "Exact stable key of an existing configured MCP server"}
            },
            "required": ["source"],
            "additionalProperties": false
        })
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        self.run(&args, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        _config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(&args, Some(&context.cancellation)).await
    }
}

impl ReloadMcpTool {
    async fn run(&self, args: &Value, cancellation: Option<&CancellationToken>) -> ToolResult {
        let Some(source) = args.get("source").and_then(Value::as_str) else {
            return ToolResult::error("SOURCE_ID_INVALID");
        };
        match self.runtime.reload(source, cancellation).await {
            Ok(report) => {
                ToolResult::text("MCP source reloaded; rediscover tools to refresh client schemas.")
                    .with_structured(report)
            }
            Err(code) => {
                let old = self.runtime.snapshot().sources.get(source).cloned();
                let report = json!({
                    "source": source,
                    "outcome": "failed_without_switch",
                    "previousGeneration": old.as_ref().map(|generation| generation.number),
                    "previousVersion": old.as_ref().and_then(|generation| generation.version()),
                    "rollback": "previous_registry_retained",
                    "clientRediscoveryRequired": false,
                    "diagnosticCode": code
                });
                ToolResult::error(format!("MCP reload did not switch connections: {code}"))
                    .with_structured(report)
            }
        }
    }
}
